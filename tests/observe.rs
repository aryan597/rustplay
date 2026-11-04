//! Acceptance tests for the `observe` action and `ref=` addressing.
//!
//! Numbering follows the QA/Acceptance list in docs/SPEC-observe.md. Items 1,
//! 2 and the schema/AGENTS.md sync are enforced by tests/docs_sync.rs instead.

mod common;
use common::*;
use rustplay::access::Role;
use rustplay::actions::{execute, Step};
use rustplay::{run_task, Session, SessionConfig, Task};
use serde_json::{json, Value};

macro_rules! need_browser {
    () => {
        if !browser_available() {
            return;
        }
    };
}

/// Launch, navigate to `page`, return the session and the server that backs it.
async fn page(cfg: SessionConfig, page: &str) -> (Session, rustplay::static_server::StaticServer) {
    let srv = fixture().await;
    let mut s = Session::launch(cfg).await.unwrap();
    s.goto(&format!("{}/{page}", srv.base_url())).await.unwrap();
    (s, srv)
}

fn nodes_of(res: &Value) -> &Vec<Value> {
    res["nodes"].as_array().expect("nodes array")
}

fn by_id<'a>(res: &'a Value, id: &str) -> Option<&'a Value> {
    nodes_of(res).iter().find(|n| n["id"].as_str() == Some(id))
}

fn by_name<'a>(res: &'a Value, tag: &str, name: &str) -> Option<&'a Value> {
    nodes_of(res).iter().find(|n| n["tag"].as_str() == Some(tag) && n["name"].as_str() == Some(name))
}

fn has_state(node: &Value, state: &str) -> bool {
    node["state"].as_array().map(|s| s.contains(&json!(state))).unwrap_or(false)
}

fn ref_of(node: &Value) -> String {
    format!("ref={}", node["ref"].as_str().expect("ref"))
}

// ---------- 3: capability ----------

#[tokio::test(flavor = "multi_thread")]
async fn item03_viewer_may_observe_but_not_eval() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.role = Role::Viewer;

    let observe = Task::from_json(&format!(
        r#"{{"steps":[{{"action":"goto","url":"{}/"}},{{"action":"observe"}}]}}"#,
        srv.base_url()
    ))
    .unwrap();
    let r = run_task(&observe, &c).await;
    assert!(r.ok, "viewer must be allowed to observe: {:?}", r.error);

    let eval = Task::from_json(&format!(
        r#"{{"steps":[{{"action":"goto","url":"{}/"}},{{"action":"eval","expression":"1+1"}}]}}"#,
        srv.base_url()
    ))
    .unwrap();
    let r = run_task(&eval, &c).await;
    assert!(!r.ok);
    assert_eq!(r.error.unwrap().code, "FORBIDDEN");
}

// ---------- 4-9: outline content ----------

#[tokio::test(flavor = "multi_thread")]
async fn item04_outline_names_the_page_controls() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;

    // No selector: the default path must walk the document, not return nothing.
    let (res, _) = s.observe(None, None, None, None, false).await.unwrap();
    assert!(!nodes_of(&res).is_empty(), "default observe returned an empty outline");

    assert!(by_name(&res, "h1", "Fixture Page").is_some(), "h1 missing or unnamed");
    assert!(by_id(&res, "name").is_some(), "input#name missing");
    assert!(by_name(&res, "button", "Greet").is_some(), "button#submit missing or unnamed");
    assert!(by_id(&res, "link").is_some(), "a#link missing");

    // The input has no <label>, so its name falls back to the placeholder.
    assert_eq!(by_id(&res, "name").unwrap()["name"].as_str(), Some("your name"));
}

#[tokio::test(flavor = "multi_thread")]
async fn item04b_containers_are_not_named_after_their_descendants() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;
    let (res, _) = s.observe(None, None, None, None, false).await.unwrap();

    // A name must come from an element's own text, never its subtree, or every
    // ancestor is named after the whole page and the outline is unusable.
    for tag in ["html", "body"] {
        if let Some(n) = nodes_of(&res).iter().find(|n| n["tag"].as_str() == Some(tag)) {
            assert!(n["name"].is_null(), "<{tag}> should have no name, got {:?}", n["name"]);
        }
    }
    let longest = nodes_of(&res).iter().filter_map(|n| n["name"].as_str()).map(str::len).max().unwrap_or(0);
    assert!(longest < 200, "some node absorbed descendant text (longest name {longest} chars)");
}

#[tokio::test(flavor = "multi_thread")]
async fn item05_hidden_absent_by_default_present_on_request() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;

    let (res, _) = s.observe(None, None, None, None, false).await.unwrap();
    assert!(by_id(&res, "hidden").is_none(), "display:none button leaked into the outline");
    assert!(by_id(&res, "ghost").is_none(), "invisible button leaked into the outline");

    let (res, _) = s.observe(None, None, None, None, true).await.unwrap();
    let hidden = by_id(&res, "hidden").expect("#hidden with include_hidden");
    assert!(has_state(hidden, "hidden"));
}

#[tokio::test(flavor = "multi_thread")]
async fn item06_disabled_and_readonly_states() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;
    let (res, _) = s.observe(None, None, None, None, true).await.unwrap();

    assert!(has_state(by_id(&res, "disabled").expect("#disabled"), "disabled"));
    assert!(has_state(by_id(&res, "readonly").expect("#readonly"), "not_editable"));
}

#[tokio::test(flavor = "multi_thread")]
async fn item07_covered_element_reports_obscured() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;
    let (res, _) = s.observe(None, None, None, None, false).await.unwrap();

    assert!(has_state(by_id(&res, "covered").expect("#covered"), "obscured"));
}

#[tokio::test(flavor = "multi_thread")]
async fn item08_iframe_is_marked_but_not_entered() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "shadow.html").await;
    let (res, _) = s.observe(None, Some(500), Some(1 << 20), None, true).await.unwrap();

    let frame = by_id(&res, "frame").expect("iframe node");
    assert_eq!(frame["role"].as_str(), Some("frame"));
    assert_eq!(frame["src"].as_str(), Some("/data.txt"));
    assert!(has_state(frame, "not_traversed"));

    // /data.txt renders inside a <pre>; no interior node may appear.
    assert!(nodes_of(&res).iter().all(|n| n["tag"].as_str() != Some("pre")), "frame interior was traversed");
}

#[tokio::test(flavor = "multi_thread")]
async fn item09_two_observes_of_a_static_page_are_identical() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;

    let (a, _) = s.observe(None, None, None, None, false).await.unwrap();
    let (b, _) = s.observe(None, None, None, None, false).await.unwrap();
    assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
}

// ---------- 10-15: ref addressing ----------

#[tokio::test(flavor = "multi_thread")]
async fn item10_refs_drive_a_form_round_trip() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;
    let (res, _) = s.observe(None, None, None, None, false).await.unwrap();

    let input = ref_of(by_id(&res, "name").unwrap());
    let button = ref_of(by_name(&res, "button", "Greet").unwrap());
    let ms = 2_000;

    s.type_text(&input, "Ferris", false, ms).await.unwrap();
    s.click(&button, ms).await.unwrap();
    let text = s.text("#out", false, ms).await.unwrap();
    assert_eq!(text.as_str(), Some("Hello, Ferris"));
}

#[tokio::test(flavor = "multi_thread")]
async fn item11_selector_based_assertions_accept_refs() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;
    let (res, _) = s.observe(None, None, None, None, true).await.unwrap();

    let title = ref_of(nodes_of(&res).iter().find(|n| n["tag"].as_str() == Some("h1")).unwrap());
    let hidden = ref_of(by_id(&res, "hidden").unwrap());
    let link = ref_of(by_id(&res, "link").unwrap());

    // assert_url takes no selector, so the five selector-based asserts are the
    // whole surface here.
    let steps = vec![
        Step::AssertText { selector: title.clone(), equals: None, contains: Some("Fixture".into()), matches: None },
        Step::AssertVisible { selector: title.clone(), timeout_ms: None },
        Step::AssertHidden { selector: hidden, timeout_ms: None },
        Step::AssertCount { selector: title, equals: 1 },
        Step::AssertAttribute { selector: link, name: "data-id".into(), equals: "42".into() },
    ];
    for step in steps {
        execute(&mut s, &step).await.unwrap_or_else(|e| panic!("{:?} failed: {e}", step.name()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn item12_refs_die_on_navigation() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let mut s = Session::launch(cfg(out.path())).await.unwrap();
    s.goto(&format!("{}/", srv.base_url())).await.unwrap();

    let (res, _) = s.observe(None, None, None, None, false).await.unwrap();
    let button = ref_of(by_name(&res, "button", "Greet").unwrap());

    s.goto(&format!("{}/", srv.base_url())).await.unwrap();
    let err = s.click(&button, 2_000).await.unwrap_err();
    assert_eq!(err.code(), "STALE_REF", "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn item13_unissued_refs_are_distinguishable_from_stale_ones() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;
    let (res, _) = s.observe(None, None, None, None, false).await.unwrap();
    let epoch = res["epoch"].as_u64().unwrap();

    // Right epoch, index never issued.
    let err = s.click(&format!("ref=e9999@{epoch}"), 2_000).await.unwrap_err();
    assert_eq!(err.code(), "REF_NOT_FOUND", "got {err}");

    // Malformed: no epoch at all.
    let err = s.click("ref=e9999", 2_000).await.unwrap_err();
    assert_eq!(err.code(), "REF_NOT_FOUND", "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn item14_detached_element_is_reported_not_panicked() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;
    let (res, _) = s.observe(None, None, None, None, false).await.unwrap();
    let button = ref_of(by_name(&res, "button", "Greet").unwrap());

    s.eval("document.getElementById('submit').remove()").await.unwrap();
    let err = s.click(&button, 1_000).await.unwrap_err();
    assert_eq!(err.code(), "ELEMENT_DETACHED", "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn item15_css_selectors_containing_equals_still_work() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;

    let text = s.text(r#"[data-id="42"]"#, false, 2_000).await.unwrap();
    assert_eq!(text.as_str(), Some("Next"));
}

// ---------- 16-19: chunking ----------

#[tokio::test(flavor = "multi_thread")]
async fn item16_max_nodes_is_respected_and_yields_a_cursor() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;

    let (res, _) = s.observe(None, Some(5), None, None, false).await.unwrap();
    assert!(nodes_of(&res).len() <= 5);
    assert!(res["next_cursor"].is_string(), "expected a cursor to continue from");
}

#[tokio::test(flavor = "multi_thread")]
async fn item17_paging_covers_every_node_exactly_once() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;

    let (whole, _) = s.observe(None, Some(10_000), Some(1 << 20), None, false).await.unwrap();
    let expected: Vec<String> = nodes_of(&whole).iter().map(|n| n["ref"].as_str().unwrap().to_string()).collect();

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..100 {
        let (chunk, _) = s.observe(None, Some(3), Some(1 << 20), cursor.as_deref(), false).await.unwrap();
        seen.extend(nodes_of(&chunk).iter().map(|n| n["ref"].as_str().unwrap().to_string()));
        match chunk["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, expected, "paging did not reproduce the unchunked outline");
    let mut deduped = seen.clone();
    deduped.dedup();
    assert_eq!(deduped.len(), seen.len(), "a node was emitted twice");
}

#[tokio::test(flavor = "multi_thread")]
async fn item18_cursor_dies_on_navigation() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let mut s = Session::launch(cfg(out.path())).await.unwrap();
    s.goto(&format!("{}/", srv.base_url())).await.unwrap();

    let (res, _) = s.observe(None, Some(2), None, None, false).await.unwrap();
    let cursor = res["next_cursor"].as_str().unwrap().to_string();

    s.goto(&format!("{}/", srv.base_url())).await.unwrap();
    let err = s.observe(None, Some(2), None, Some(&cursor), false).await.unwrap_err();
    assert_eq!(err.code(), "STALE_SNAPSHOT", "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn item19_max_bytes_is_never_exceeded() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "many.html").await;

    let budget = 2_000;
    let (res, _) = s.observe(None, Some(10_000), Some(budget), None, false).await.unwrap();
    let serialized = serde_json::to_string(&res["nodes"]).unwrap();
    assert!(serialized.len() <= budget, "nodes serialized to {} bytes, budget {budget}", serialized.len());
    assert!(res["next_cursor"].is_string(), "a truncated outline must offer a cursor");
}

// ---------- 20-22: guard ----------

#[tokio::test(flavor = "multi_thread")]
async fn item20_injection_flags_attach_to_the_offending_node() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;
    let (res, union) = s.observe(None, Some(500), Some(1 << 20), None, false).await.unwrap();

    let review = by_id(&res, "review").expect("#review");
    let flags = review["flags"].as_array().unwrap();
    assert!(
        flags.iter().any(|f| f["pattern"].as_str() == Some("ignore_instructions")),
        "hostile node not flagged: {review}"
    );

    let title = by_name(&res, "h1", "Fixture Page").unwrap();
    assert!(title["flags"].as_array().unwrap().is_empty(), "benign node flagged");

    // The union is what task.rs warns the operator about.
    assert!(!union.is_empty(), "step-level flags must surface the per-node findings");
}

#[tokio::test(flavor = "multi_thread")]
async fn item21_pii_is_masked_in_the_outline() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.redact_pii = true;
    let (mut s, _srv) = page(c, "").await;

    let (res, _) = s.observe(None, Some(500), Some(1 << 20), None, false).await.unwrap();
    let contact = by_id(&res, "contact").expect("#contact");
    let name = contact["name"].as_str().unwrap();
    assert!(name.contains("[REDACTED_EMAIL]"), "email not masked: {name}");
    assert!(name.contains("[REDACTED_PHONE]"), "phone not masked: {name}");
}

#[tokio::test(flavor = "multi_thread")]
async fn item22_every_node_is_scanned_on_a_large_page() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.timeouts.action_ms = 15_000; // a 1,000-node walk, not a single probe
    let (mut s, _srv) = page(c, "many.html").await;

    let (res, union) = s.observe(None, Some(5_000), Some(4 << 20), None, false).await.unwrap();
    assert!(nodes_of(&res).len() > 900, "expected the whole page, got {}", nodes_of(&res).len());

    // Exactly two rows carry planted injections, at opposite ends of the page,
    // so this fails if scanning stops early or skips nodes.
    let patterns: Vec<&str> = union.iter().map(|f| f.pattern).collect();
    assert!(patterns.contains(&"ignore_instructions"), "missed the early injection: {patterns:?}");
    assert!(patterns.contains(&"role_override"), "missed the late injection: {patterns:?}");
}

// ---------- policy: a ref is an address, never a privilege ----------

#[tokio::test(flavor = "multi_thread")]
async fn refs_do_not_bypass_role_checks() {
    need_browser!();
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.role = Role::Viewer;

    // A viewer may observe, so it can hold a valid ref. Acting on one still
    // needs Interact, which a viewer does not have.
    let mut s = Session::launch(c.clone()).await.unwrap();
    s.goto(&format!("{}/", srv.base_url())).await.unwrap();
    let (res, _) = s.observe(None, None, None, None, false).await.unwrap();
    let button = ref_of(by_name(&res, "button", "Greet").unwrap());
    drop(s);

    let task = Task::from_json(&format!(
        r#"{{"steps":[{{"action":"goto","url":"{}/"}},{{"action":"click","selector":"{button}"}}]}}"#,
        srv.base_url()
    ))
    .unwrap();
    let r = run_task(&task, &c).await;
    assert!(!r.ok, "a viewer must not be able to click, ref or not");
    assert_eq!(r.error.unwrap().code, "FORBIDDEN");
}

#[tokio::test(flavor = "multi_thread")]
async fn refs_do_not_bypass_pii_redaction() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let mut c = cfg(out.path());
    c.redact_pii = true;
    let (mut s, _srv) = page(c, "").await;

    let (res, _) = s.observe(None, Some(500), Some(1 << 20), None, false).await.unwrap();
    let contact = ref_of(by_id(&res, "contact").unwrap());

    // Reading through a ref goes through the same guard as a CSS selector.
    let step = Step::Text { selector: contact, all: false };
    let out = execute(&mut s, &step).await.unwrap();
    let text = out.value.as_str().unwrap();
    assert!(text.contains("[REDACTED_EMAIL]"), "ref read leaked an email: {text}");
    assert!(text.contains("[REDACTED_PHONE]"), "ref read leaked a phone: {text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn hostile_refs_are_rejected_without_executing() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "").await;
    s.observe(None, None, None, None, false).await.unwrap();

    // Quoting is a unit test; this proves the end-to-end path errors cleanly
    // instead of evaluating the payload or panicking.
    for hostile in [r#"ref="); alert(1); ("@1"#, "ref=../../etc/passwd", "ref=@@@", "ref="] {
        let err = s.click(hostile, 1_000).await.unwrap_err();
        assert!(
            matches!(err.code(), "REF_NOT_FOUND" | "STALE_REF" | "CONFIG_ERROR"),
            "unexpected error for {hostile:?}: {err} ({})",
            err.code()
        );
    }
}

// ---------- 23-24: shadow DOM ----------

#[tokio::test(flavor = "multi_thread")]
async fn item23_open_shadow_roots_are_reachable() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "shadow.html").await;

    let (res, _) = s.observe(None, Some(500), Some(1 << 20), None, false).await.unwrap();
    let button = by_name(&res, "button", "Shadow Button").expect("open shadow button");
    let out_node = by_id(&res, "shadow-out").expect("shadow output node");
    let out_ref = ref_of(out_node);

    s.click(&ref_of(button), 2_000).await.unwrap();
    let text = s.text(&out_ref, false, 2_000).await.unwrap();
    assert_eq!(text.as_str(), Some("shadow clicked"));
}

#[tokio::test(flavor = "multi_thread")]
async fn item24_closed_shadow_roots_stay_invisible() {
    need_browser!();
    let out = tempfile::tempdir().unwrap();
    let (mut s, _srv) = page(cfg(out.path()), "shadow.html").await;

    let (res, _) = s.observe(None, Some(500), Some(1 << 20), None, true).await.unwrap();
    assert!(by_name(&res, "button", "Closed Button").is_none(), "closed shadow root leaked");
    assert!(by_id(&res, "closed-button").is_none(), "closed shadow root leaked");
}
