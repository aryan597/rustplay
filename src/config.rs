//! Session configuration. The operator's config (CLI flags / server flags)
//! is the ceiling; a task file can only tighten it, never loosen it.

use crate::access::Role;
use crate::errors::{Result, RpError};
use crate::limits::ResourceLimits;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, Default)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// Chromium's own sandbox plus process/profile hardening.
    #[default]
    None,
    /// Additionally wrap Chromium in bubblewrap namespaces (Linux only).
    Bwrap,
}

/// How rustplay talks CDP to Chromium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, Default)]
#[serde(rename_all = "snake_case")]
pub enum CdpTransport {
    /// `--remote-debugging-pipe`: NUL-framed JSON on inherited fds 3/4. No socket at all.
    #[default]
    Pipe,
    /// Deprecated: `--remote-debugging-port=0` on 127.0.0.1, reachable by any local process.
    Tcp,
}

impl CdpTransport {
    pub fn name(self) -> &'static str {
        match self {
            CdpTransport::Pipe => "pipe",
            CdpTransport::Tcp => "tcp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Timeouts {
    pub launch_ms: u64,
    pub navigation_ms: u64,
    pub action_ms: u64,
    pub task_ms: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self { launch_ms: 20_000, navigation_ms: 30_000, action_ms: 10_000, task_ms: 120_000 }
    }
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub chrome_path: Option<PathBuf>,
    pub headless: bool,
    /// Keep Chromium's own sandbox on. Turning it off is only acceptable inside another sandbox.
    pub chrome_sandbox: bool,
    pub isolation: Isolation,
    pub timeouts: Timeouts,
    pub viewport: (u32, u32),
    /// If set, downloads are allowed and land in a fresh per-session subdirectory of this dir.
    pub downloads_dir: Option<PathBuf>,
    /// If set, only these hosts resolve (enforced in Chromium's resolver for every request).
    pub allowed_hosts: Option<Vec<String>>,
    pub allow_file_urls: bool,
    /// Directory that task-relative output paths (screenshots) are confined to.
    pub out_dir: PathBuf,
    pub overwrite: bool,
    /// Role that every step is checked against.
    pub role: Role,
    /// Mask emails, phone numbers and card-like numbers in extracted output.
    pub redact_pii: bool,
    /// Where per-session profiles are created (default: system temp dir).
    /// A fresh `--user-data-dir` inside it is always used; it cannot be disabled.
    pub temp_root: Option<PathBuf>,
    pub cdp_transport: CdpTransport,
    pub limits: ResourceLimits,
    /// Delegated cgroup v2 directory to create session cgroups under.
    pub cgroup_parent: Option<PathBuf>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            chrome_path: None,
            headless: true,
            chrome_sandbox: true,
            isolation: Isolation::None,
            timeouts: Timeouts::default(),
            viewport: (1280, 720),
            downloads_dir: None,
            allowed_hosts: None,
            allow_file_urls: false,
            out_dir: PathBuf::from("."),
            overwrite: false,
            role: Role::Operator,
            redact_pii: false,
            temp_root: None,
            cdp_transport: CdpTransport::Pipe,
            limits: ResourceLimits::default(),
            cgroup_parent: None,
        }
    }
}

/// Options a task may set. All of them can only make things stricter.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOptions {
    pub navigation_timeout_ms: Option<u64>,
    pub action_timeout_ms: Option<u64>,
    pub task_timeout_ms: Option<u64>,
    pub viewport: Option<[u32; 2]>,
    pub allowed_hosts: Option<Vec<String>>,
    pub redact_pii: Option<bool>,
    /// Hard wall-clock limit for the browser (OS watchdog). Can only lower the operator's.
    pub wall_limit_ms: Option<u64>,
    /// Total CPU time of the browser process tree. Can only lower the operator's.
    pub cpu_limit_ms: Option<u64>,
    /// Memory cap for the browser process tree, in MiB. Can only lower the operator's.
    pub memory_limit_mb: Option<u64>,
}

fn tighten(base: u64, req: Option<u64>) -> u64 {
    req.map_or(base, |r| r.min(base))
}

impl SessionConfig {
    pub fn validate(&self) -> Result<()> {
        let t = &self.timeouts;
        for (name, v) in [
            ("launch timeout", t.launch_ms),
            ("navigation timeout", t.navigation_ms),
            ("action timeout", t.action_ms),
            ("task timeout", t.task_ms),
        ] {
            if v == 0 {
                return Err(RpError::config(format!("{name} must be > 0")));
            }
        }
        let (w, h) = self.viewport;
        if !(100..=7680).contains(&w) || !(100..=4320).contains(&h) {
            return Err(RpError::config(format!("viewport {w}x{h} out of range (100..7680 x 100..4320)")));
        }
        if let Some(hosts) = &self.allowed_hosts {
            for h in hosts {
                validate_host(h)?;
            }
        }
        self.limits.validate()?;
        if self.cdp_transport == CdpTransport::Pipe && !cfg!(unix) {
            return Err(RpError::TransportUnsupported { reason: "pipe transport needs a unix platform".into() });
        }
        Ok(())
    }

    /// Apply task options on top of the operator config, only ever tightening.
    pub fn with_task_options(&self, o: &TaskOptions) -> Result<SessionConfig> {
        let mut c = self.clone();
        c.timeouts.navigation_ms = tighten(c.timeouts.navigation_ms, o.navigation_timeout_ms);
        c.timeouts.action_ms = tighten(c.timeouts.action_ms, o.action_timeout_ms);
        c.timeouts.task_ms = tighten(c.timeouts.task_ms, o.task_timeout_ms);
        if let Some([w, h]) = o.viewport {
            c.viewport = (w, h);
        }
        if o.redact_pii == Some(true) {
            c.redact_pii = true;
        }
        c.limits = self.limits.tightened(&ResourceLimits {
            wall_ms: o.wall_limit_ms,
            cpu_ms: o.cpu_limit_ms,
            memory_bytes: o.memory_limit_mb.map(|m| m.saturating_mul(1 << 20)),
        });
        if let Some(req) = &o.allowed_hosts {
            for h in req {
                validate_host(h)?;
            }
            c.allowed_hosts = Some(match &self.allowed_hosts {
                // Operator already restricts: the task may only narrow the list.
                Some(base) => {
                    let narrowed: Vec<String> =
                        req.iter().filter(|h| base.iter().any(|b| b.eq_ignore_ascii_case(h))).cloned().collect();
                    if narrowed.len() != req.len() {
                        return Err(RpError::policy(
                            "task allowed_hosts must be a subset of the operator's --allow-host list",
                        ));
                    }
                    narrowed
                }
                None => req.clone(),
            });
        }
        c.validate()?;
        Ok(c)
    }
}

pub fn validate_host(h: &str) -> Result<()> {
    let ok = !h.is_empty()
        && h.len() <= 253
        && h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '*' | ':' | '[' | ']'));
    if ok {
        Ok(())
    } else {
        Err(RpError::config(format!("invalid host `{h}` in allowed hosts")))
    }
}

pub fn parse_viewport(s: &str) -> std::result::Result<(u32, u32), String> {
    let (w, h) = s.split_once(['x', 'X']).ok_or("expected WIDTHxHEIGHT, e.g. 1280x720")?;
    let w = w.trim().parse::<u32>().map_err(|e| format!("width: {e}"))?;
    let h = h.trim().parse::<u32>().map_err(|e| format!("height: {e}"))?;
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_options_only_tighten() {
        let base = SessionConfig::default();
        let o =
            TaskOptions { navigation_timeout_ms: Some(999_999), action_timeout_ms: Some(500), ..Default::default() };
        let c = base.with_task_options(&o).unwrap();
        assert_eq!(c.timeouts.navigation_ms, base.timeouts.navigation_ms);
        assert_eq!(c.timeouts.action_ms, 500);
    }

    #[test]
    fn task_cannot_widen_host_allowlist() {
        let base = SessionConfig { allowed_hosts: Some(vec!["example.com".into()]), ..Default::default() };
        let o = TaskOptions { allowed_hosts: Some(vec!["evil.com".into()]), ..Default::default() };
        assert_eq!(base.with_task_options(&o).unwrap_err().code(), "POLICY_VIOLATION");
        let o = TaskOptions { allowed_hosts: Some(vec!["EXAMPLE.com".into()]), ..Default::default() };
        assert!(base.with_task_options(&o).is_ok());
    }

    #[test]
    fn task_limits_only_tighten() {
        let base = SessionConfig {
            limits: ResourceLimits { wall_ms: Some(5_000), cpu_ms: None, memory_bytes: Some(1 << 30) },
            ..Default::default()
        };
        let o = TaskOptions {
            wall_limit_ms: Some(60_000),
            cpu_limit_ms: Some(2_000),
            memory_limit_mb: Some(4096),
            ..Default::default()
        };
        let c = base.with_task_options(&o).unwrap();
        assert_eq!(c.limits.wall_ms, Some(5_000), "task cannot raise");
        assert_eq!(c.limits.cpu_ms, Some(2_000), "task can add");
        assert_eq!(c.limits.memory_bytes, Some(1 << 30), "task cannot raise");
        assert_eq!(c.cdp_transport, CdpTransport::Pipe, "pipe is the default");
    }

    #[test]
    fn rejects_bad_values() {
        let mut c = SessionConfig::default();
        c.timeouts.action_ms = 0;
        assert_eq!(c.validate().unwrap_err().code(), "CONFIG_INVALID");
        assert!(validate_host("a b").is_err());
        assert!(validate_host("--disable-web-security").is_err() || validate_host("x,y").is_err());
        assert_eq!(parse_viewport("800x600").unwrap(), (800, 600));
        assert!(parse_viewport("800").is_err());
    }
}
