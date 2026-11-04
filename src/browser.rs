//! Owns the Chromium OS process. We spawn it ourselves (instead of
//! chromiumoxide's launcher) so we control env scrubbing, process groups,
//! parent-death signals, the bubblewrap wrapper, the CDP endpoint (pipe fds
//! or TCP) and cgroup placement, then attach over CDP.

use crate::config::{CdpTransport, SessionConfig};
use crate::errors::{Result, RpError};
use crate::sandbox;
use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

const STDERR_TAIL: usize = 15;

/// Last lines Chromium wrote to stderr, for launch/crash diagnostics.
#[derive(Clone, Default)]
pub struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    fn push(&self, line: String) {
        let mut q = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if q.len() == STDERR_TAIL {
            q.pop_front();
        }
        q.push_back(line);
    }
    pub fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).iter().cloned().collect()
    }
}

pub struct BrowserProcess {
    child: Option<Child>,
    pid: Option<u32>,
    pub stderr: StderrTail,
}

/// Where to talk CDP.
pub enum Endpoint {
    #[cfg(unix)]
    Pipe(crate::cdp_pipe::PipeTransport),
    Tcp(String),
}

pub struct Launched {
    pub process: BrowserProcess,
    pub endpoint: Endpoint,
}

impl BrowserProcess {
    /// Spawn Chromium. `cgroup_procs` are open `cgroup.procs` fds the child
    /// writes its own pid into before exec (see `limits`).
    pub async fn launch(
        cfg: &SessionConfig,
        session_root: &Path,
        profile: &Path,
        cache: &Path,
        downloads: Option<&Path>,
        cgroup_procs: Vec<RawFd>,
    ) -> Result<Launched> {
        if cfg.chrome_sandbox && sandbox::is_root() {
            return Err(RpError::config(
                "Chromium refuses to run its sandbox as root. Run as a normal user (recommended), \
                 or pass --no-chrome-sandbox together with --isolation bwrap",
            ));
        }
        let chrome = sandbox::find_chrome(cfg.chrome_path.as_deref())?;
        let args = sandbox::chrome_args(cfg, profile, cache);
        sandbox::ensure_session_profile(&args, session_root)?;
        let (program, argv) = sandbox::wrap_command(cfg, &chrome, args, session_root, downloads)?;
        tracing::debug!(program = %program.display(), transport = cfg.cdp_transport.name(), "launching browser");

        let mut cmd = Command::new(&program);
        cmd.args(&argv)
            .env_clear()
            .envs(sandbox::child_env(session_root))
            .current_dir(session_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            // Own process group so we can kill Chromium and all its helpers at once.
            cmd.process_group(0);
        }

        #[cfg(unix)]
        let pipes = match cfg.cdp_transport {
            CdpTransport::Pipe => {
                Some(crate::cdp_pipe::ChildPipes::new().map_err(|e| RpError::io("create CDP pipes", e))?)
            }
            CdpTransport::Tcp => None,
        };
        #[cfg(not(unix))]
        if cfg.cdp_transport == CdpTransport::Pipe {
            return Err(RpError::TransportUnsupported { reason: "pipe transport needs a unix platform".into() });
        }

        #[cfg(unix)]
        {
            let child_fds = pipes.as_ref().map(|p| p.child_fds());
            // SAFETY: the closure only makes async-signal-safe syscalls
            // (prctl, getpid, write, fcntl, dup2, close) between fork and exec.
            unsafe {
                cmd.pre_exec(move || {
                    // Join the session cgroups first so every later helper inherits them.
                    if !cgroup_procs.is_empty() {
                        crate::limits::write_pid_to(&cgroup_procs)?;
                    }
                    // If rustplay dies (even SIGKILL), the kernel kills the browser.
                    #[cfg(target_os = "linux")]
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if let Some((cmd_read, resp_write)) = child_fds {
                        crate::cdp_pipe::install_child_fds(cmd_read, resp_write)?;
                    }
                    Ok(())
                });
            }
        }

        let mut child = cmd.spawn().map_err(|e| RpError::Launch {
            reason: format!("spawn {}: {e}", program.display()),
            hint: Some(
                "check the binary is executable and the cgroup (if any) is writable; pass --chrome to override".into(),
            ),
        })?;
        let pid = child.id();
        let stderr_pipe = child.stderr.take().expect("stderr piped");
        let stderr = StderrTail::default();

        // Drain stderr into the tail buffer forever (a full pipe would block
        // Chromium). In TCP mode also watch for the DevTools URL.
        let want_ws = cfg.cdp_transport == CdpTransport::Tcp;
        let (tx, rx) = oneshot::channel::<String>();
        {
            let tail = stderr.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr_pipe).lines();
                let mut tx = if want_ws { Some(tx) } else { None };
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(i) = line.find("ws://") {
                        if let Some(t) = tx.take() {
                            let _ = t.send(line[i..].trim().to_string());
                            continue;
                        }
                    }
                    tracing::trace!(target: "chromium", "{line}");
                    tail.push(line);
                }
            });
        }

        let mut proc = BrowserProcess { child: Some(child), pid, stderr };

        #[cfg(unix)]
        if let Some(p) = pipes {
            // Closes Chrome's ends in this process; EOF now tracks Chrome's lifetime.
            let t = p.into_transport().map_err(|e| {
                proc.kill_now();
                RpError::io("open CDP pipes", e)
            })?;
            return Ok(Launched { process: proc, endpoint: Endpoint::Pipe(t) });
        }

        match tokio::time::timeout(Duration::from_millis(cfg.timeouts.launch_ms), rx).await {
            Ok(Ok(ws_url)) => Ok(Launched { process: proc, endpoint: Endpoint::Tcp(ws_url) }),
            Ok(Err(_)) => {
                proc.kill_now();
                Err(launch_error(&proc.stderr.lines()))
            }
            Err(_) => {
                proc.kill_now();
                Err(RpError::Launch {
                    reason: format!("no DevTools endpoint within {}ms", cfg.timeouts.launch_ms),
                    hint: Some("the browser hung at startup; raise --launch-timeout or run `rustplay doctor`".into()),
                })
            }
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Has the process exited? Returns the status text if so.
    pub fn exited(&mut self) -> Option<String> {
        let c = self.child.as_mut()?;
        match c.try_wait() {
            Ok(Some(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    /// Error for a browser that died or never answered during startup.
    pub fn startup_failure(&mut self, what: &str) -> RpError {
        let mut tail = self.stderr.lines();
        if let Some(status) = self.exited() {
            tail.push(format!("(browser exited: {status})"));
        } else {
            tail.push(format!("(browser still running but {what})"));
        }
        launch_error(&tail)
    }

    /// Wait up to `grace` for a clean exit, then SIGKILL the whole group.
    pub async fn shutdown(&mut self, grace: Duration) {
        if let Some(c) = self.child.as_mut() {
            if tokio::time::timeout(grace, c.wait()).await.is_ok() {
                self.kill_group();
                self.child = None;
                return;
            }
        }
        self.kill_now();
        if let Some(mut c) = self.child.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), c.wait()).await;
        }
    }

    fn kill_group(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: plain syscall; negative pid targets the process group we created.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }

    /// Synchronous kill, safe to call from Drop and during panic unwinding.
    /// No-op once the child has been reaped, so we never signal a recycled pid.
    pub fn kill_now(&mut self) {
        if self.child.is_some() {
            self.kill_group();
        }
        if let Some(c) = self.child.as_mut() {
            let _ = c.start_kill();
        }
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.kill_now();
        }
    }
}

fn launch_error(tail: &[String]) -> RpError {
    let joined = tail.join("\n");
    let hint = if joined.contains("No usable sandbox") || joined.contains("--no-sandbox") {
        "Chromium's sandbox cannot start here (no user namespaces or running as root); \
         enable unprivileged user namespaces, or use --no-chrome-sandbox --isolation bwrap"
    } else if joined.contains("Missing X server") || joined.contains("DISPLAY") {
        "a headed browser needs a display; drop --headed"
    } else if joined.contains("error while loading shared libraries") {
        "Chromium is missing system libraries; install the distro chromium package"
    } else if joined.contains("remote-debugging-pipe") || joined.contains("Bad file descriptor") {
        "this Chromium build rejected --remote-debugging-pipe; try a newer Chrome, or --cdp-transport tcp (deprecated)"
    } else {
        "run `rustplay doctor`, or `RUST_LOG=chromium=trace` for full browser output"
    };
    let reason = if joined.is_empty() {
        "browser exited before CDP was ready".to_string()
    } else {
        format!("browser did not become ready; last output:\n{joined}")
    };
    RpError::Launch { reason, hint: Some(hint.into()) }
}
