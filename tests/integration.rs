//! End-to-end tests against a real headless Chromium and a local fixture.
//! No external network is used.

mod common;
use common::*;
use rustplay::session::WaitState;
use rustplay::{run_task, Session, Task};
use std::time::Duration;

macro_rules! need_browser {
    () => {
        if !browser_available() {
            return;
        }
    };
}

#[tokio::test(flavor = "multi_thread")]
async fn full_task_flow() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let task = Task::from_json(&format!(
        r##"{{"name":"flow","steps":[
          {{"action":"goto","url":"{base}/"}},
          {{"action":"wait_for","selector":"#late"}},
          {{"action":"type","selector":"#name","text":"Ferris é","clear":true}},
          {{"action":"click","selector":"#submit"}},
          {{"action":"text","selector":"#out"}},
          {{"action":"attribute","selector":"#link","name":"data-id"}},
          {{"action":"text","selector":"li.item","all":true}},
          {{"action":"scroll","selector":"#bottom"}},
          {{"action":"screenshot","path":"shots/page.png"}},
          {{"action":"screenshot"}},
          {{"action":"assert_count","selector":"li.item","equals":3}}
        ]}}"##,
        base = srv.base_url()
    ))
    .unwrap();
    let r = run_task(&task, &cfg(out.path())).await;
    assert!(r.ok, "{}", serde_json::to_string_pretty(&r).unwrap());
    assert_eq!(r.steps[0].output["title"], "rustplay fixture");
    assert_eq!(r.steps[4].output, "Hello, Ferris é", "non-ASCII text must type correctly");
    assert_eq!(r.steps[5].output, "42");
    assert_eq!(r.steps[6].output, serde_json::json!(["one", "two", "three"]));
    assert!(r.steps[7].output["y"].as_f64().unwrap() > 1000.0);
    let png = std::fs::read(out.path().join("shots/page.png")).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert!(r.steps[9].output["base64"].as_str().unwrap().len() > 100);
    assert!(r.untrusted_content);
    assert!(r.warnings.is_empty(), "cleanup warnings: {:?}", r.warnings);
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_errors_have_codes() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let mut s = Session::launch(cfg(out.path())).await.expect("launch");
    s.goto(&format!("{}/", srv.base_url())).await.unwrap();

    let ms = 600;
    let cases: Vec<(&str, rustplay::RpError)> = vec![
        ("ELEMENT_DISABLED", s.click("#disabled", ms).await.unwrap_err()),
        ("ELEMENT_HIDDEN", s.click("#hidden", ms).await.unwrap_err()),
        ("ELEMENT_HIDDEN", s.click("#ghost", ms).await.unwrap_err()),
        ("ELEMENT_OBSCURED", s.click("#covered", ms).await.unwrap_err()),
        ("ELEMENT_NOT_EDITABLE", s.type_text("#readonly", "x", false, ms).await.unwrap_err()),
        ("SELECTOR_NOT_FOUND", s.click("#nope", ms).await.unwrap_err()),
        ("SELECTOR_INVALID", s.click("##bad[", ms).await.unwrap_err()),
        ("SELECTOR_NOT_FOUND", s.text("#nope", false, ms).await.unwrap_err()),
        ("JS_EVAL_FAILED", s.eval("throw new Error('boom')").await.unwrap_err()),
        ("POLICY_VIOLATION", s.goto("file:///etc/passwd").await.unwrap_err()),
        ("POLICY_VIOLATION", s.goto("https://example.com/").await.unwrap_err()),
        ("ACTION_TIMEOUT", s.wait_for("#title", WaitState::Hidden, ms).await.unwrap_err()),
    ];
    for (code, e) in &cases {
        assert_eq!(e.code(), *code, "{e}");
        assert!(!e.to_string().is_empty());
    }
    let obscured = &cases[3].1;
    assert!(obscured.to_string().contains("div#overlay"), "{obscured}");
    assert!(cases[8].1.to_string().contains("boom"));

    // Navigation errors.
    let closed_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let e = s.goto(&format!("http://127.0.0.1:{closed_port}/")).await.unwrap_err();
    assert_eq!(e.code(), "NAV_NETWORK", "{e}");
    assert!(e.hint().unwrap().contains("listening"));
    s.close().await.unwrap();

    let mut c = cfg(out.path());
    c.timeouts.navigation_ms = 800;
    let mut s = Session::launch(c).await.unwrap();
    let e = s.goto(&format!("{}/__slow", srv.base_url())).await.unwrap_err();
    assert_eq!(e.code(), "NAV_TIMEOUT", "{e}");
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn close_kills_process_and_removes_profile() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let s = Session::launch(cfg(out.path())).await.unwrap();
    let profile = s.profile_dir().to_path_buf();
    let needle = profile.display().to_string();
    assert!(profile.exists());
    assert!(!processes_mentioning(&needle).is_empty(), "browser should be running");
    s.close().await.unwrap();
    assert!(!profile.exists(), "profile dir must be deleted");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(processes_mentioning(&needle).is_empty(), "no browser process may survive close()");
}

#[tokio::test(flavor = "multi_thread")]
async fn drop_without_close_still_kills() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let s = Session::launch(cfg(out.path())).await.unwrap();
    let profile = s.profile_dir().to_path_buf();
    let needle = profile.display().to_string();
    drop(s); // simulates panic unwinding / cancellation
    for _ in 0..30 {
        if processes_mentioning(&needle).is_empty() && !profile.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("browser or profile survived drop: pids={:?} exists={}", processes_mentioning(&needle), profile.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_task_cleans_up() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let task = Task::from_json(&format!(
        r#"{{"steps":[{{"action":"goto","url":"{}/"}},{{"action":"sleep","ms":30000}}]}}"#,
        srv.base_url()
    ))
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let needle = root.path().display().to_string();
    let mut c = cfg(out.path());
    c.temp_root = Some(root.path().to_path_buf());
    // Same mechanism as Ctrl-C in the CLI: the future is dropped mid-task.
    let r = tokio::time::timeout(Duration::from_secs(4), run_task(&task, &c)).await;
    assert!(r.is_err(), "task should have been cancelled");
    for _ in 0..30 {
        let left = std::fs::read_dir(root.path()).unwrap().count();
        if processes_mentioning(&needle).is_empty() && left == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("cancelled task leaked: pids={:?}", processes_mentioning(&needle));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_sessions_are_isolated() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let base = srv.base_url();
    // Each session writes localStorage; none may see another's value.
    let mk = |i: usize| {
        Task::from_json(&format!(
            r#"{{"steps":[
              {{"action":"goto","url":"{base}/"}},
              {{"action":"eval","expression":"(() => {{ const prev = localStorage.getItem('k'); localStorage.setItem('k', '{i}'); return prev; }})()"}}
            ]}}"#
        ))
        .unwrap()
    };
    let mut c = cfg(out.path());
    c.role = rustplay::access::Role::Admin;
    let (t1, t2, t3) = (mk(1), mk(2), mk(3));
    let (a, b, d) = tokio::join!(run_task(&t1, &c), run_task(&t2, &c), run_task(&t3, &c));
    let mut ids = std::collections::HashSet::new();
    for r in [a, b, d] {
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.steps[1].output, serde_json::Value::Null, "storage leaked between sessions");
        assert!(ids.insert(r.session_id.unwrap()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn example_suite_passes() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let files = rustplay::suite::discover(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/suite")))
        .unwrap();
    let rep = rustplay::suite::run_suite(&files, &cfg(out.path()), Some(&srv.base_url()), 2, false).await;
    assert!(rep.ok, "{}", serde_json::to_string_pretty(&rep).unwrap());
    assert_eq!(rep.passed, files.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_assertion_reports_step() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let t = Task::from_json(&format!(
        r##"{{"steps":[{{"action":"goto","url":"{}/"}},{{"action":"assert_text","selector":"#title","equals":"Wrong"}}]}}"##,
        srv.base_url()
    ))
    .unwrap();
    let r = run_task(&t, &cfg(out.path())).await;
    assert!(!r.ok);
    let e = r.error.unwrap();
    assert_eq!(e.code, "ASSERTION_FAILED");
    assert!(e.message.contains("Fixture Page"), "{}", e.message);
    assert_eq!(r.steps.last().unwrap().index, 1);
}
