//! Command-line interface definitions and conversion into `SessionConfig`.

use crate::access::Role;
use crate::config::{parse_viewport, CdpTransport, Isolation, SessionConfig, Timeouts};
use crate::limits::{parse_size, ResourceLimits};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "rustplay",
    version,
    about = "Sandboxed headless Chromium automation: CLI, test runner and local HTTP API",
    after_help = "Exit codes: 0 ok, 1 task failed, 2 config/policy/permission, 3 browser launch, 130 cancelled.\nOutput: JSON on stdout, logs on stderr."
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Debug, Args)]
pub struct GlobalOpts {
    /// Chrome/Chromium binary (default: autodetect, or $RUSTPLAY_CHROME)
    #[arg(long, global = true, value_name = "PATH")]
    pub chrome: Option<PathBuf>,
    /// Disable Chromium's own sandbox. Only do this inside --isolation bwrap or a container.
    #[arg(long, global = true)]
    pub no_chrome_sandbox: bool,
    /// Extra OS isolation around the browser
    #[arg(long, global = true, value_enum, default_value_t = Isolation::None)]
    pub isolation: Isolation,
    /// How rustplay talks to Chromium. `pipe` (default) uses inherited fds and opens no
    /// port; `tcp` is deprecated (loopback DevTools port reachable by local processes).
    #[arg(long, global = true, value_enum, default_value_t = CdpTransport::Pipe)]
    pub cdp_transport: CdpTransport,
    /// Hard wall-clock limit for each browser, enforced by an OS watchdog (SIGKILL).
    #[arg(long, global = true, value_name = "MS")]
    pub wall_limit: Option<u64>,
    /// Total CPU time for each browser process tree (Linux cgroups)
    #[arg(long, global = true, value_name = "MS")]
    pub cpu_limit: Option<u64>,
    /// Memory cap for each browser process tree, e.g. 512M, 1G (Linux cgroups)
    #[arg(long, global = true, value_name = "SIZE", value_parser = parse_size)]
    pub memory_limit: Option<u64>,
    /// Delegated cgroup v2 directory to create session cgroups in (default: our own cgroup)
    #[arg(long, global = true, value_name = "DIR", env = "RUSTPLAY_CGROUP_PARENT")]
    pub cgroup_parent: Option<PathBuf>,
    /// Show the browser window (debugging only)
    #[arg(long, global = true)]
    pub headed: bool,
    #[arg(long, global = true, value_name = "MS", default_value_t = Timeouts::default().launch_ms)]
    pub launch_timeout: u64,
    #[arg(long, global = true, value_name = "MS", default_value_t = Timeouts::default().navigation_ms)]
    pub nav_timeout: u64,
    #[arg(long, global = true, value_name = "MS", default_value_t = Timeouts::default().action_ms)]
    pub action_timeout: u64,
    #[arg(long, global = true, value_name = "MS", default_value_t = Timeouts::default().task_ms)]
    pub task_timeout: u64,
    #[arg(long, global = true, value_parser = parse_viewport, default_value = "1280x720")]
    pub viewport: (u32, u32),
    /// Only these hosts resolve, for every request the page makes (repeatable; `*.example.com` ok)
    #[arg(long = "allow-host", global = true, value_name = "HOST")]
    pub allow_hosts: Vec<String>,
    /// Allow file:// navigation
    #[arg(long, global = true)]
    pub allow_file_urls: bool,
    /// Enable downloads into a fresh per-session subdirectory of DIR (default: downloads denied)
    #[arg(long, global = true, value_name = "DIR")]
    pub downloads_dir: Option<PathBuf>,
    /// Directory that task output paths are confined to
    #[arg(long, global = true, value_name = "DIR", default_value = ".")]
    pub out_dir: PathBuf,
    /// Allow tasks to overwrite existing output files
    #[arg(long, global = true)]
    pub overwrite: bool,
    /// Capability ceiling for tasks: viewer (read), operator (+interact, files), admin (+eval)
    #[arg(long, global = true, value_enum, default_value_t = Role::Operator)]
    pub role: Role,
    /// Mask emails, phone and card numbers in extracted content
    #[arg(long, global = true)]
    pub redact_pii: bool,
    /// Where per-session browser profiles are created (default: system temp dir)
    #[arg(long, global = true, value_name = "DIR")]
    pub temp_root: Option<PathBuf>,
    /// Pretty-print JSON output
    #[arg(long, global = true)]
    pub pretty: bool,
    /// Structured JSON logs on stderr
    #[arg(long, global = true)]
    pub log_json: bool,
    /// -v info, -vv debug, -vvv trace (RUST_LOG overrides)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Open one URL and optionally screenshot / extract
    Open {
        url: String,
        /// Save a screenshot (path relative to --out-dir)
        #[arg(long, value_name = "PATH")]
        screenshot: Option<String>,
        #[arg(long)]
        full_page: bool,
        /// Wait for this selector to be visible first
        #[arg(long, value_name = "SELECTOR")]
        wait_for: Option<String>,
        /// Extract innerText of this selector
        #[arg(long, value_name = "SELECTOR")]
        text: Option<String>,
    },
    /// Run task files, each in its own isolated session
    Run {
        #[arg(required = true, value_name = "TASK.json")]
        tasks: Vec<PathBuf>,
        /// Sessions to run concurrently
        #[arg(long, short = 'j', default_value_t = 2)]
        parallel: usize,
    },
    /// Run a directory of task files with assert_* steps as a test suite
    Test {
        #[arg(value_name = "DIR|FILE")]
        path: PathBuf,
        /// Serve this directory on 127.0.0.1 and set ${BASE_URL} to it
        #[arg(long, value_name = "DIR", conflicts_with = "base_url")]
        serve: Option<PathBuf>,
        /// Value for ${BASE_URL}
        #[arg(long, value_name = "URL")]
        base_url: Option<String>,
        #[arg(long, short = 'j', default_value_t = 2)]
        parallel: usize,
        /// Include full step reports for passing cases too
        #[arg(long)]
        full_report: bool,
    },
    /// Start the local HTTP API
    #[cfg(feature = "server")]
    Serve {
        #[arg(long, default_value_t = 8787)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        bind: std::net::IpAddr,
        /// Max concurrent browser sessions; extra requests get HTTP 429
        #[arg(long, default_value_t = 4)]
        max_sessions: usize,
        /// JSON file: [{"token":"...","name":"...","role":"viewer|operator|admin"}]
        #[arg(long, value_name = "FILE")]
        tokens_file: Option<PathBuf>,
        /// Read a single bearer token from this env var (role = --role)
        #[arg(long, value_name = "VAR")]
        token_env: Option<String>,
    },
    /// Check the environment: browser, sandbox support, a real launch
    Doctor,
    /// Print the JSON Schema for task files
    Schema,
}

impl GlobalOpts {
    pub fn session_config(&self) -> SessionConfig {
        SessionConfig {
            chrome_path: self.chrome.clone(),
            headless: !self.headed,
            chrome_sandbox: !self.no_chrome_sandbox,
            isolation: self.isolation,
            timeouts: Timeouts {
                launch_ms: self.launch_timeout,
                navigation_ms: self.nav_timeout,
                action_ms: self.action_timeout,
                task_ms: self.task_timeout,
            },
            viewport: self.viewport,
            downloads_dir: self.downloads_dir.clone(),
            allowed_hosts: if self.allow_hosts.is_empty() { None } else { Some(self.allow_hosts.clone()) },
            allow_file_urls: self.allow_file_urls,
            out_dir: self.out_dir.clone(),
            overwrite: self.overwrite,
            role: self.role,
            redact_pii: self.redact_pii,
            temp_root: self.temp_root.clone(),
            cdp_transport: self.cdp_transport,
            limits: ResourceLimits {
                wall_ms: self.wall_limit,
                cpu_ms: self.cpu_limit,
                memory_bytes: self.memory_limit,
            },
            cgroup_parent: self.cgroup_parent.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_examples() {
        let c = Cli::try_parse_from(["rustplay", "open", "https://example.com", "--screenshot", "out.png"]).unwrap();
        assert!(matches!(c.cmd, Cmd::Open { .. }));
        let c =
            Cli::try_parse_from(["rustplay", "run", "task.json", "--role", "viewer", "--allow-host", "example.com"])
                .unwrap();
        let cfg = c.global.session_config();
        assert_eq!(cfg.role, Role::Viewer);
        assert_eq!(cfg.allowed_hosts.unwrap(), vec!["example.com"]);
        assert!(cfg.chrome_sandbox);
        assert!(Cli::try_parse_from(["rustplay", "run"]).is_err());
        assert_eq!(cfg.cdp_transport, CdpTransport::Pipe, "pipe is the default");
        assert!(cfg.limits.is_empty(), "limits are opt-in");
        let c = Cli::try_parse_from([
            "rustplay",
            "run",
            "t.json",
            "--cdp-transport",
            "tcp",
            "--wall-limit",
            "5000",
            "--cpu-limit",
            "2000",
            "--memory-limit",
            "768M",
        ])
        .unwrap();
        let cfg = c.global.session_config();
        assert_eq!(cfg.cdp_transport, CdpTransport::Tcp);
        assert_eq!(
            cfg.limits,
            ResourceLimits { wall_ms: Some(5000), cpu_ms: Some(2000), memory_bytes: Some(768 << 20) }
        );
        assert!(Cli::try_parse_from(["rustplay", "run", "t.json", "--cdp-transport", "websocket"]).is_err());
        assert!(Cli::try_parse_from(["rustplay", "run", "t.json", "--memory-limit", "lots"]).is_err());
        assert!(Cli::try_parse_from(["rustplay", "test", "s", "--serve", "d", "--base-url", "x"]).is_err());
    }
}
