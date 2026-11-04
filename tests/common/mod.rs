#![allow(dead_code)]
use rustplay::config::Timeouts;
use rustplay::static_server::{self, StaticServer};
use rustplay::{sandbox, SessionConfig};
use std::path::Path;

/// Returns None (and the test should return early) when no browser is installed,
/// so `cargo test` still passes the pure unit tests on machines without Chrome.
pub fn browser_available() -> bool {
    match sandbox::find_chrome(None) {
        Ok(_) => true,
        Err(_) => {
            eprintln!("SKIP: no Chrome/Chromium found (set RUSTPLAY_CHROME)");
            false
        }
    }
}

pub async fn fixture() -> StaticServer {
    static_server::serve(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).await.expect("fixture server")
}

pub fn cfg(out_dir: &Path) -> SessionConfig {
    SessionConfig {
        // CI containers often run as root, where Chromium's sandbox cannot start.
        chrome_sandbox: !sandbox::is_root(),
        timeouts: Timeouts { launch_ms: 20_000, navigation_ms: 5_000, action_ms: 2_000, task_ms: 30_000 },
        allowed_hosts: Some(vec!["127.0.0.1".into()]),
        out_dir: out_dir.to_path_buf(),
        // CI points this at a delegated cgroup v2 dir to exercise the pure-v2 path.
        cgroup_parent: std::env::var_os("RUSTPLAY_CGROUP_PARENT").map(Into::into),
        ..Default::default()
    }
}

/// Any live process whose command line mentions `needle` (Linux only).
pub fn processes_mentioning(needle: &str) -> Vec<u32> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        if let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) {
            let cmd = String::from_utf8_lossy(&cmd);
            if cmd.contains(needle) {
                // Skip zombies: they are dead, just not yet reaped.
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                if !stat.contains(") Z ") {
                    out.push(pid);
                }
            }
        }
    }
    out
}
