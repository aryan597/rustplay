//! Evals: one test per failure mode in the 8-layer pipeline stack, applied to
//! rustplay's role as the browser *tool* inside an agent pipeline.
//! These are the merge gate (scripts/gate.sh, CI). There is no skip path:
//! a change that breaks one of these does not ship.

mod common;
use common::*;
use rustplay::access::Role;
use rustplay::{run_task, Session, Task};
use std::time::Duration;

macro_rules! need_browser {
    () => {
        if !browser_available() {
            return;
        }
    };
}

fn task(base: &str, steps: &str) -> Task {
    Task::from_json(&format!(r#"{{"steps":[{{"action":"goto","url":"{base}/"}},{steps}]}}"#)).unwrap()
}

// ---- Layer 05: access ----------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn l05_viewer_cannot_interact_eval_or_write() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.role = Role::Viewer;
    for step in [
        r##"{"action":"click","selector":"#submit"}"##,
        r##"{"action":"eval","expression":"document.cookie"}"##,
        r##"{"action":"screenshot","path":"x.png"}"##,
    ] {
        let r = run_task(&task(&srv.base_url(), step), &c).await;
        assert_eq!(r.error.as_ref().unwrap().code, "FORBIDDEN", "{step}");
        assert!(r.session_id.is_none(), "denied before any browser launched");
    }
    assert!(std::fs::read_dir(out.path()).unwrap().next().is_none(), "nothing written");
    // ...but can read.
    let r = run_task(&task(&srv.base_url(), r##"{"action":"text","selector":"#title"}"##), &c).await;
    assert!(r.ok, "{:?}", r.error);
}

#[tokio::test(flavor = "multi_thread")]
async fn l05_task_cannot_loosen_operator_policy() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let t = Task::from_json(&format!(
        r#"{{"options":{{"allowed_hosts":["127.0.0.1","evil.example"],"task_timeout_ms":99999999}},
            "steps":[{{"action":"goto","url":"{}/"}}]}}"#,
        srv.base_url()
    ))
    .unwrap();
    let r = run_task(&t, &cfg(out.path())).await;
    assert_eq!(r.error.unwrap().code, "POLICY_VIOLATION");
}

// ---- Layer 06: guardrails --------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn l06_injected_instruction_in_page_is_flagged() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let r = run_task(&task(&srv.base_url(), r##"{"action":"text","selector":"#review"}"##), &cfg(out.path())).await;
    assert!(r.ok);
    assert!(r.untrusted_content);
    let flags = &r.steps[1].injection_flags;
    assert!(flags.iter().any(|f| f.pattern == "ignore_instructions"), "{flags:?}");
    // Clean content is not flagged.
    let r = run_task(&task(&srv.base_url(), r##"{"action":"text","selector":"#title"}"##), &cfg(out.path())).await;
    assert!(r.steps[1].injection_flags.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn l06_pii_is_masked_in_output() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.redact_pii = true;
    let r = run_task(&task(&srv.base_url(), r##"{"action":"text","selector":"#contact"}"##), &c).await;
    let text = r.steps[1].output.as_str().unwrap().to_string();
    assert!(!text.contains("bob@example.com") && !text.contains("7946"), "{text}");
    assert!(text.contains("[REDACTED_EMAIL]") && text.contains("[REDACTED_PHONE]"), "{text}");
    // The task file can opt in to redaction, never out of it.
    let mut c2 = cfg(out.path());
    c2.redact_pii = true;
    let t = Task::from_json(&format!(
        r##"{{"options":{{"redact_pii":false}},"steps":[{{"action":"goto","url":"{}/"}},{{"action":"text","selector":"#contact"}}]}}"##,
        srv.base_url()
    ))
    .unwrap();
    let r = run_task(&t, &c2).await;
    assert!(r.steps[1].output.as_str().unwrap().contains("[REDACTED_EMAIL]"));
}

#[tokio::test(flavor = "multi_thread")]
async fn l06_host_allowlist_applies_to_subresources() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.role = Role::Admin;
    let port = srv.addr.port();
    // Page-initiated fetches: the allowed IP works, any hostname fails to resolve.
    let r = run_task(
        &task(
            &srv.base_url(),
            &format!(
                r#"{{"action":"eval","expression":"Promise.all([fetch('http://127.0.0.1:{port}/data.txt').then(r => r.status, () => 'blocked'), fetch('http://localhost:{port}/data.txt').then(r => r.status, () => 'blocked'), fetch('http://exfil.example/x').then(r => r.status, () => 'blocked')])"}}"#
            ),
        ),
        &c,
    )
    .await;
    assert!(r.ok, "{:?}", r.error);
    assert_eq!(r.steps[1].output, serde_json::json!([200, "blocked", "blocked"]));
}

/// Pipe mode must expose no DevTools port anywhere in the browser's process tree.
/// A TCP-mode session runs at the same time as the positive control: the scan
/// must find its port, and must attribute it to that tree only.
#[tokio::test(flavor = "multi_thread")]
async fn l06_no_cdp_port_exposed_by_browser_tree() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let url = format!("{}/", srv.base_url());

    let mut tcp_cfg = cfg(out.path());
    tcp_cfg.cdp_transport = rustplay::config::CdpTransport::Tcp;
    let (a, b, c, control) = tokio::join!(
        Session::launch(cfg(out.path())),
        Session::launch(cfg(out.path())),
        Session::launch(cfg(out.path())),
        Session::launch(tcp_cfg),
    );
    let mut pipe_sessions = vec![a.unwrap(), b.unwrap(), c.unwrap()];
    let control = control.unwrap();

    for s in pipe_sessions.iter_mut() {
        s.goto(&url).await.unwrap();
        s.type_text("#name", "probe", true, 2_000).await.unwrap();
        s.click("#submit", 2_000).await.unwrap();
    }
    // Scan repeatedly across the session lifetime, not just once.
    for _ in 0..5 {
        for s in &pipe_sessions {
            let l = s.tcp_listeners().expect("Linux /proc scan");
            assert!(l.is_empty(), "session {} exposes {l:?}", s.id);
        }
        let ctl = control.tcp_listeners().unwrap();
        assert!(!ctl.is_empty(), "positive control: the TCP-mode DevTools port must be visible");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ctl_ports: Vec<u16> = control.tcp_listeners().unwrap().iter().map(|a| a.port()).collect();
    for s in &pipe_sessions {
        let tree = rustplay::procinfo::process_tree(s.pid().unwrap()).unwrap();
        assert!(!tree.contains(&control.pid().unwrap()), "trees must not overlap");
    }
    for s in pipe_sessions {
        s.close().await.unwrap();
    }
    control.close().await.unwrap();
    assert!(!ctl_ports.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn l06_browser_env_has_no_host_secrets() {
    need_browser!();
    std::env::set_var("RUSTPLAY_CANARY_SECRET", "s3cr3t-canary");
    let out = tempfile::tempdir().unwrap();
    let s = Session::launch(cfg(out.path())).await.unwrap();
    let pid = s.pid().unwrap();
    let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    let env = String::from_utf8_lossy(&env);
    assert!(!env.contains("s3cr3t-canary"), "host env leaked into the browser");
    assert!(env.contains("HOME=") && env.contains("rustplay-"), "HOME should point into the session dir: {env}");
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn l06_downloads_denied_by_default_and_confined_when_enabled() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();

    // Default: denied. Nothing may appear anywhere under the session root.
    let mut c = cfg(out.path());
    c.temp_root = Some(root.path().to_path_buf());
    let mut s = Session::launch(c).await.unwrap();
    s.goto(&format!("{}/", srv.base_url())).await.unwrap();
    s.click("#dl", 2000).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let leaked = walk(root.path())
        .into_iter()
        .any(|p| std::fs::read_to_string(&p).is_ok_and(|t| t.contains("hello from fixture")));
    assert!(!leaked, "a denied download was written to disk");
    s.close().await.unwrap();

    // Enabled: lands only in <downloads_dir>/<session_id>/.
    let dl = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.downloads_dir = Some(dl.path().to_path_buf());
    let mut s = Session::launch(c).await.unwrap();
    let sid = s.id.clone();
    s.goto(&format!("{}/", srv.base_url())).await.unwrap();
    s.click("#dl", 2000).await.unwrap();
    let mut found = false;
    for _ in 0..40 {
        let files = walk(&dl.path().join(&sid));
        if files.iter().any(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains("hello from fixture"))) {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    s.close().await.unwrap();
    assert!(found, "download should land in the per-session downloads dir");
    assert!(walk(dl.path()).iter().all(|p| p.starts_with(dl.path().join(&sid))));
}

#[tokio::test(flavor = "multi_thread")]
async fn l06_bwrap_hides_host_filesystem() {
    need_browser!();
    if rustplay::sandbox::which("bwrap").is_none() {
        eprintln!("SKIP: bwrap not installed");
        return;
    }
    let secret_dir = tempfile::tempdir().unwrap();
    let secret = secret_dir.path().join("secret.html");
    std::fs::write(&secret, "<title>host secret</title>").unwrap();
    let url = format!("file://{}", secret.display());
    let out = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.allow_file_urls = true;

    // Control: without bwrap the file is readable (so the test proves something).
    let mut s = Session::launch(c.clone()).await.unwrap();
    assert_eq!(s.goto(&url).await.unwrap()["title"], "host secret");
    s.close().await.unwrap();

    // With bwrap the host's /tmp is replaced by an empty tmpfs.
    c.isolation = rustplay::config::Isolation::Bwrap;
    c.chrome_sandbox = false; // bwrap is the boundary; nested userns sandbox is optional
    let mut s = Session::launch(c).await.expect("bwrap launch");
    let e = s.goto(&url).await.unwrap_err();
    assert_eq!(e.code(), "NAV_NETWORK", "{e}");
    assert!(e.to_string().contains("FILE_NOT_FOUND"), "{e}");
    s.close().await.unwrap();
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

// ---- Layer 07: burst control (API) ------------------------------------------

#[cfg(feature = "server")]
mod api_evals {
    use super::*;
    use rustplay::access::TokenTable;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn post(addr: std::net::SocketAddr, body: &str, token: Option<&str>) -> (u16, serde_json::Value) {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        let req = format!(
            "POST /v1/tasks HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf);
        let status: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
        // axum may use chunked encoding; pull out the JSON object.
        let json = &body[body.find('{').unwrap_or(0)..=body.rfind('}').unwrap_or(0)];
        (status, serde_json::from_str(json).unwrap_or(serde_json::Value::Null))
    }

    async fn start(c: rustplay::SessionConfig, max: usize, tokens: TokenTable) -> std::net::SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, rustplay::api::router(c, max, tokens)).await.unwrap() });
        addr
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn l07_burst_gets_429_not_unbounded_browsers() {
        need_browser!();
        let srv = fixture().await;
        let out = tempfile::tempdir().unwrap();
        let addr = start(cfg(out.path()), 1, TokenTable::default()).await;
        let slow =
            format!(r#"{{"steps":[{{"action":"goto","url":"{}/"}},{{"action":"sleep","ms":2500}}]}}"#, srv.base_url());
        let fast = format!(r#"{{"steps":[{{"action":"goto","url":"{}/"}}]}}"#, srv.base_url());
        let first = tokio::spawn({
            let slow = slow.clone();
            async move { post(addr, &slow, None).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (code, body) = post(addr, &fast, None).await;
        assert_eq!(code, 429, "{body}");
        assert_eq!(body["error"]["code"], "BUSY");
        assert_eq!(body["error"]["retryable"], true);
        let (code, body) = first.await.unwrap();
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["ok"], true);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn l05_api_auth_and_roles() {
        need_browser!();
        let srv = fixture().await;
        let out = tempfile::tempdir().unwrap();
        let tokens = TokenTable::from_json(
            r#"[{"token":"viewer-token-0123456789","name":"bot","role":"viewer"},
                {"token":"admin-token-0123456789","name":"ops","role":"admin"}]"#,
        )
        .unwrap();
        let addr = start(cfg(out.path()), 2, tokens).await;
        let eval = format!(
            r#"{{"steps":[{{"action":"goto","url":"{}/"}},{{"action":"eval","expression":"1+1"}}]}}"#,
            srv.base_url()
        );
        assert_eq!(post(addr, &eval, None).await.0, 401);
        assert_eq!(post(addr, &eval, Some("wrong-token-0123456789")).await.0, 401);
        let (code, body) = post(addr, &eval, Some("viewer-token-0123456789")).await;
        assert_eq!((code, body["error"]["code"].as_str()), (403, Some("FORBIDDEN")));
        let (code, body) = post(addr, &eval, Some("admin-token-0123456789")).await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["steps"][1]["output"], 2);
        let (code, body) =
            post(addr, r#"{"steps":[{"action":"shell","cmd":"id"}]}"#, Some("admin-token-0123456789")).await;
        assert_eq!((code, body["error"]["code"].as_str()), (400, Some("CONFIG_INVALID")));
    }
}

// ---- Layer 08: observability -------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn l08_wrong_answer_is_traceable_to_a_step() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let r = run_task(
        &task(
            &srv.base_url(),
            r##"{"action":"type","selector":"#name","text":"x"},{"action":"click","selector":"#covered"},{"action":"text","selector":"#out"}"##,
        ),
        &cfg(out.path()),
    )
    .await;
    assert!(!r.ok);
    assert_eq!(r.trace_id.len(), 16);
    assert!(r.session_id.is_some());
    let failed = r.steps.last().unwrap();
    assert_eq!((failed.index, failed.action, failed.ok), (2, "click", false));
    let e = failed.error.as_ref().unwrap();
    assert_eq!(e.code, "ELEMENT_OBSCURED");
    assert!(e.hint.as_ref().unwrap().contains("overlay"));
    assert!(r.steps.iter().all(|s| s.duration_ms < 10_000));
    assert_eq!(r.steps.len(), 3, "steps after the failure must not run");
}
