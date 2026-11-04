use clap::Parser;
use rustplay::cli::{Cli, Cmd, GlobalOpts};
use rustplay::errors::RpError;
use rustplay::{sandbox, suite, Session, SessionConfig, Task, TaskReport};
use serde::Serialize;
use std::time::Instant;

fn main() {
    let cli = Cli::parse();
    init_tracing(&cli.global);
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start async runtime: {e}");
            std::process::exit(1);
        }
    };
    // `serve` handles Ctrl-C itself (graceful drain of in-flight tasks).
    #[cfg(feature = "server")]
    let drains_itself = matches!(cli.cmd, Cmd::Serve { .. });
    #[cfg(not(feature = "server"))]
    let drains_itself = false;
    let code = rt.block_on(async {
        if drains_itself {
            return dispatch(cli).await;
        }
        tokio::select! {
            code = dispatch(cli) => code,
            _ = tokio::signal::ctrl_c() => {
                // Dropping `dispatch` drops every Session, which kills Chromium
                // process groups and deletes their temp profiles.
                print_json(&serde_json::json!({ "ok": false, "error": RpError::Cancelled.report() }), false);
                130
            }
        }
    });
    // Cancel any stragglers (their Drop impls kill browsers) before exiting.
    rt.shutdown_timeout(std::time::Duration::from_secs(3));
    std::process::exit(code);
}

fn init_tracing(g: &GlobalOpts) {
    use tracing_subscriber::EnvFilter;
    let level = match g.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{level},chromiumoxide=error,tungstenite=warn")));
    let b = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr);
    if g.log_json {
        b.json().init();
    } else {
        b.with_target(false).compact().init();
    }
}

fn print_json<T: Serialize>(v: &T, pretty: bool) {
    let s = if pretty { serde_json::to_string_pretty(v) } else { serde_json::to_string(v) };
    println!("{}", s.unwrap_or_else(|e| format!(r#"{{"ok":false,"error":{{"code":"INTERNAL","message":"{e}"}}}}"#)));
}

fn fail(e: RpError, pretty: bool) -> i32 {
    print_json(&serde_json::json!({ "ok": false, "error": e.report() }), pretty);
    e.exit_code()
}

async fn dispatch(cli: Cli) -> i32 {
    let pretty = cli.global.pretty;
    let cfg = cli.global.session_config();
    if let Err(e) = cfg.validate() {
        return fail(e, pretty);
    }
    match cli.cmd {
        Cmd::Open { url, screenshot, full_page, wait_for, text } => {
            let mut steps = vec![serde_json::json!({ "action": "goto", "url": url })];
            if let Some(s) = wait_for {
                steps.push(serde_json::json!({ "action": "wait_for", "selector": s }));
            }
            if let Some(s) = text {
                steps.push(serde_json::json!({ "action": "text", "selector": s }));
            }
            if let Some(p) = screenshot {
                steps.push(serde_json::json!({ "action": "screenshot", "path": p, "full_page": full_page }));
            }
            let task = serde_json::json!({ "name": "open", "steps": steps }).to_string();
            match Task::from_json(&task) {
                Ok(t) => report(rustplay::run_task(&t, &cfg).await, pretty),
                Err(e) => fail(e, pretty),
            }
        }
        Cmd::Run { tasks, parallel } => run_files(tasks, &cfg, parallel, pretty).await,
        Cmd::Test { path, serve, base_url, parallel, full_report } => {
            let files = match suite::discover(&path) {
                Ok(f) => f,
                Err(e) => return fail(e, pretty),
            };
            let server = match serve {
                Some(dir) => match rustplay::static_server::serve(dir).await {
                    Ok(s) => Some(s),
                    Err(e) => return fail(RpError::io("start fixture server", e), pretty),
                },
                None => None,
            };
            let base = server.as_ref().map(|s| s.base_url()).or(base_url);
            let rep = suite::run_suite(&files, &cfg, base.as_deref(), parallel, full_report).await;
            print_json(&rep, pretty);
            if rep.ok {
                0
            } else {
                1
            }
        }
        #[cfg(feature = "server")]
        Cmd::Serve { port, bind, max_sessions, tokens_file, token_env } => {
            let tokens = match load_tokens(tokens_file, token_env, cfg.role) {
                Ok(t) => t,
                Err(e) => return fail(e, pretty),
            };
            let opts = rustplay::api::ServeOptions { addr: (bind, port).into(), max_sessions, tokens };
            match rustplay::api::serve(cfg, opts).await {
                Ok(()) => 0,
                Err(e) => fail(e, pretty),
            }
        }
        Cmd::Doctor => doctor(cfg, pretty).await,
        Cmd::Schema => {
            println!("{}", rustplay::TASK_SCHEMA.trim());
            0
        }
    }
}

fn report(r: TaskReport, pretty: bool) -> i32 {
    let code = r.exit_code();
    print_json(&r, pretty);
    code
}

async fn run_files(files: Vec<std::path::PathBuf>, cfg: &SessionConfig, parallel: usize, pretty: bool) -> i32 {
    use futures::stream::{self, StreamExt};
    let single = files.len() == 1;
    let reports: Vec<TaskReport> = stream::iter(files)
        .map(|f| async move {
            let name = f.display().to_string();
            match std::fs::read_to_string(&f)
                .map_err(|e| RpError::io(format!("read {name}"), e))
                .and_then(|s| Task::from_json(&s))
            {
                Ok(t) => rustplay::run_task(&t, cfg).await,
                Err(e) => TaskReport::failed(Some(name), cfg.role.name(), &e),
            }
        })
        .buffered(parallel.max(1))
        .collect()
        .await;
    let code = reports.iter().map(TaskReport::exit_code).max().unwrap_or(0);
    if single {
        print_json(&reports[0], pretty);
    } else {
        print_json(&reports, pretty);
    }
    code
}

#[cfg(feature = "server")]
fn load_tokens(
    file: Option<std::path::PathBuf>,
    env: Option<String>,
    role: rustplay::access::Role,
) -> rustplay::Result<rustplay::access::TokenTable> {
    use rustplay::access::TokenTable;
    match (file, env) {
        (Some(f), _) => {
            let s = std::fs::read_to_string(&f).map_err(|e| RpError::io(format!("read {}", f.display()), e))?;
            TokenTable::from_json(&s)
        }
        (None, Some(var)) => {
            let t = std::env::var(&var).map_err(|_| RpError::config(format!("env var {var} is not set")))?;
            if t.len() < 16 {
                return Err(RpError::config(format!("token in {var} must be at least 16 chars")));
            }
            Ok(TokenTable::single(t, role))
        }
        (None, None) => Ok(TokenTable::default()),
    }
}

#[derive(Serialize)]
struct Check {
    name: &'static str,
    ok: bool,
    /// Optional checks (bwrap, limits, AppArmor) are reported but do not fail `doctor`.
    required: bool,
    detail: String,
}

async fn doctor(mut cfg: SessionConfig, pretty: bool) -> i32 {
    let mut checks = Vec::new();
    let chrome = sandbox::find_chrome(cfg.chrome_path.as_deref());
    match &chrome {
        Ok(p) => {
            let ver = std::process::Command::new(p)
                .arg("--version")
                .env_clear()
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|e| format!("version check failed: {e}"));
            checks.push(Check {
                name: "browser",
                ok: true,
                required: true,
                detail: format!("{} ({ver})", p.display()),
            });
        }
        Err(e) => checks.push(Check {
            name: "browser",
            ok: false,
            required: true,
            detail: format!("{e} | {}", e.hint().unwrap_or_default()),
        }),
    }
    let root = sandbox::is_root();
    checks.push(Check {
        name: "chrome_sandbox",
        required: true,
        ok: !root || !cfg.chrome_sandbox,
        detail: if root {
            "running as root: Chromium's sandbox will refuse; run as a normal user, or --no-chrome-sandbox --isolation bwrap".into()
        } else {
            "non-root; Chromium sandbox can be used".into()
        },
    });
    // bwrap is opt-in: probe it for real instead of guessing from sysctls.
    let (ok, detail) = match sandbox::which("bwrap") {
        None => (false, "not installed (optional; `apt install bubblewrap`)".to_string()),
        Some(p) => match std::process::Command::new(&p)
            .args([
                "--unshare-user",
                "--unshare-pid",
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "true",
            ])
            .env_clear()
            .output()
        {
            Ok(o) if o.status.success() => (true, format!("{} works (opt-in with --isolation bwrap)", p.display())),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let first = err.lines().next().unwrap_or("").trim().to_string();
                let why = if first.contains("Permission denied") || first.contains("Operation not permitted") {
                    " (AppArmor or sysctl blocks unprivileged user namespaces; see docs/SECURITY.md)"
                } else {
                    ""
                };
                (false, format!("installed but cannot create a sandbox: {first}{why}"))
            }
            Err(e) => (false, format!("cannot run {}: {e}", p.display())),
        },
    };
    checks.push(Check { name: "bubblewrap", ok, required: false, detail });
    if let Ok(v) = std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns") {
        let v = v.trim();
        checks.push(Check {
            name: "userns_apparmor",
            ok: v == "0",
            required: false,
            detail: if v == "0" {
                "unprivileged user namespaces allowed".into()
            } else {
                format!(
                    "apparmor_restrict_unprivileged_userns={v}: fine for the Chrome .deb (its own AppArmor profile / \
                     setuid helper; the launch check is authoritative); may block --isolation bwrap"
                )
            },
        });
    }
    if chrome.is_ok() {
        if root && cfg.chrome_sandbox {
            cfg.chrome_sandbox = false; // probe launch only; real runs still require the explicit flag
        }
        let t = Instant::now();
        let r = match Session::launch(cfg.clone()).await {
            Ok(s) => {
                let listeners = s.tcp_listeners();
                let closed = s.close().await;
                closed.map(|_| listeners)
            }
            Err(e) => Err(e),
        };
        let launch_ms = t.elapsed().as_millis();
        match r {
            Ok(listeners) => {
                checks.push(Check {
                    name: "launch",
                    required: true,
                    ok: true,
                    detail: format!(
                        "{} transport: launched and cleaned up in {launch_ms} ms",
                        cfg.cdp_transport.name()
                    ),
                });
                let (ok, detail) = match (cfg.cdp_transport, listeners) {
                    (rustplay::config::CdpTransport::Pipe, Some(l)) if l.is_empty() => {
                        (true, "pipe: no TCP listener anywhere in the browser process tree".to_string())
                    }
                    (rustplay::config::CdpTransport::Pipe, Some(l)) => {
                        (false, format!("pipe mode but listening on {l:?}"))
                    }
                    (rustplay::config::CdpTransport::Tcp, _) => {
                        (false, "tcp (deprecated): DevTools reachable on a loopback port; use the default pipe".into())
                    }
                    (_, None) => (true, "cannot inspect sockets on this OS (Linux only)".into()),
                };
                checks.push(Check { name: "cdp_exposure", ok, required: true, detail });
            }
            Err(e) => checks.push(Check {
                name: "launch",
                required: true,
                ok: false,
                detail: format!("{}: {e} | {}", e.code(), e.hint().unwrap_or_default()),
            }),
        }

        // Limits are opt-in, so "unsupported" is information, not a failure.
        let mut lc = cfg.clone();
        lc.limits = rustplay::limits::ResourceLimits {
            wall_ms: Some(60_000),
            cpu_ms: Some(60_000),
            memory_bytes: Some(2 << 30),
        };
        let mut limits_ok = true;
        let detail = match Session::launch(lc).await {
            Ok(s) => {
                let rep = s.resource_report().map(|r| r.enforcement);
                let _ = s.close().await;
                match rep {
                    Some(e) => format!(
                        "wall: {}; cpu: {}; memory: {}",
                        e.wall.unwrap_or("-"),
                        e.cpu.unwrap_or("-"),
                        e.memory.unwrap_or("-")
                    ),
                    None => "no limits applied".into(),
                }
            }
            Err(e) => {
                limits_ok = false;
                format!("{}: {e} | fix: {}", e.code(), e.hint().unwrap_or_default())
            }
        };
        checks.push(Check { name: "resource_limits", ok: limits_ok, required: false, detail });
    }
    let ok = checks.iter().filter(|c| c.required).all(|c| c.ok);
    print_json(&serde_json::json!({ "ok": ok, "checks": checks }), pretty);
    if ok {
        0
    } else {
        1
    }
}
