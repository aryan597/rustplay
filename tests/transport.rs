//! CDP transport: pipe mode must open no TCP port; TCP mode is a deprecated,
//! explicit fallback. Every "no port" assertion is paired with a positive
//! control showing the scanner does see a DevTools port when one exists.

mod common;
use common::*;
use rustplay::config::{CdpTransport, Isolation};
use rustplay::{procinfo, run_task, Session, Task};
use std::time::{Duration, Instant};

macro_rules! need_browser {
    () => {
        if !browser_available() {
            return;
        }
    };
}

fn fd_target(pid: u32, fd: u32) -> String {
    std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).map(|p| p.display().to_string()).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn pipe_session_opens_no_tcp_port() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.temp_root = Some(root.path().to_path_buf());
    assert_eq!(c.cdp_transport, CdpTransport::Pipe, "pipe must be the default");

    let mut s = Session::launch(c).await.unwrap();
    let pid = s.pid().unwrap();

    // The browser was started for the pipe, not a port.
    let argv = procinfo::cmdline(pid).unwrap();
    assert!(argv.iter().any(|a| a == "--remote-debugging-pipe"), "{argv:?}");
    assert!(!argv.iter().any(|a| a.starts_with("--remote-debugging-port")), "{argv:?}");
    // Mandatory per-session profile, inside the private temp root.
    let profiles: Vec<&String> = argv.iter().filter(|a| a.starts_with("--user-data-dir=")).collect();
    assert_eq!(profiles.len(), 1, "{argv:?}");
    assert!(profiles[0].contains(&root.path().display().to_string()), "{argv:?}");
    // CDP really runs over inherited fds 3 and 4.
    assert!(fd_target(pid, 3).starts_with("pipe:["), "fd3 = {}", fd_target(pid, 3));
    assert!(fd_target(pid, 4).starts_with("pipe:["), "fd4 = {}", fd_target(pid, 4));

    // Exercise it, then scan the whole tree (browser, zygotes, renderers, GPU, network).
    s.goto(&format!("{}/", srv.base_url())).await.unwrap();
    s.click("#submit", 2_000).await.unwrap();
    let tree = procinfo::process_tree(pid).unwrap();
    assert!(tree.len() > 2, "expected a multi-process tree, got {tree:?}");
    let listeners = s.tcp_listeners().unwrap();
    assert!(listeners.is_empty(), "pipe-mode browser is listening on {listeners:?}");
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_fallback_listens_on_loopback_and_warns() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.cdp_transport = CdpTransport::Tcp;

    // Positive control for the scanner: TCP mode must be detected.
    let s = Session::launch(c.clone()).await.unwrap();
    let listeners = s.tcp_listeners().unwrap();
    assert!(!listeners.is_empty(), "scanner failed to see the DevTools port");
    assert!(listeners.iter().all(|a| a.ip().is_loopback()), "{listeners:?}");
    assert_eq!(s.warnings()[0].code, "TCP_TRANSPORT_DEPRECATED");
    s.close().await.unwrap();

    // Still fully functional, and the report says so.
    let t = Task::from_json(&format!(
        r##"{{"steps":[{{"action":"goto","url":"{}/"}},{{"action":"text","selector":"#title"}}]}}"##,
        srv.base_url()
    ))
    .unwrap();
    let r = run_task(&t, &c).await;
    assert!(r.ok, "{:?}", r.error);
    assert_eq!(r.transport, "tcp");
    assert!(r.warnings.iter().any(|w| w.code == "TCP_TRANSPORT_DEPRECATED"));
    let r = run_task(&t, &cfg(out.path())).await;
    assert_eq!(r.transport, "pipe");
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
}

#[tokio::test(flavor = "multi_thread")]
async fn pipe_works_inside_bwrap() {
    need_browser!();
    if rustplay::sandbox::which("bwrap").is_none() {
        eprintln!("SKIP: bwrap not installed");
        return;
    }
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.isolation = Isolation::Bwrap;
    c.chrome_sandbox = false;
    let mut s = Session::launch(c).await.unwrap();
    assert_eq!(s.goto(&format!("{}/", srv.base_url())).await.unwrap()["title"], "rustplay fixture");
    assert!(s.tcp_listeners().unwrap().is_empty());
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_crash_over_pipe_is_a_typed_disconnect() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let mut s = Session::launch(cfg(out.path())).await.unwrap();
    let pid = s.pid().unwrap();
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    let t = Instant::now();
    let e = s.eval("1 + 1").await.unwrap_err();
    assert_eq!(e.code(), "BROWSER_DISCONNECTED", "{e}");
    // Well under the 2 s action timeout: the EOF itself ends the session, not a timer.
    assert!(t.elapsed() < Duration::from_millis(800), "EOF on fd 4 should be noticed fast, took {:?}", t.elapsed());
    s.close().await.unwrap();
}

fn fake_chrome(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-chrome");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[tokio::test(flavor = "multi_thread")]
async fn pipe_launch_failures_are_explained() {
    let out = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();

    // Exits immediately: EOF on fd 4 before any answer.
    let mut c = cfg(out.path());
    c.chrome_path = Some(fake_chrome(bin.path(), "echo 'fatal: kaboom' >&2; exit 7"));
    let t = Instant::now();
    let e = Session::launch(c).await.err().expect("must fail");
    assert_eq!(e.code(), "LAUNCH_FAILED", "{e}");
    let msg = e.to_string();
    assert!(msg.contains("kaboom") && msg.contains("exit"), "{msg}");
    assert!(t.elapsed() < Duration::from_secs(5), "should not wait for the launch timeout");

    // Never answers: bounded by --launch-timeout, and the process is gone afterwards.
    let mut c = cfg(out.path());
    c.chrome_path = Some(fake_chrome(bin.path(), "while :; do sleep 1; done"));
    c.timeouts.launch_ms = 1_200;
    let t = Instant::now();
    let e = Session::launch(c).await.err().expect("must fail");
    assert_eq!(e.code(), "LAUNCH_FAILED", "{e}");
    assert!(e.to_string().contains("did not answer"), "{e}");
    assert!(t.elapsed() < Duration::from_secs(4));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(processes_mentioning(&bin.path().display().to_string()).is_empty(), "hung fake browser survived");
}
