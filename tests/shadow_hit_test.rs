//! Regression test for the actionability hit test and open shadow roots.
//!
//! `document.elementFromPoint` retargets a hit inside a shadow tree to that
//! tree's host, so a probe that does not descend through open roots reports the
//! host as the obscuring element. The same flaw makes every element *inside* an
//! open shadow root look permanently obscured by its own host, which is how it
//! was found — but that path needs ref addressing to reach, so this test uses
//! the CSS-reachable half of the bug.

mod common;
use common::*;
use rustplay::Session;

#[tokio::test(flavor = "multi_thread")]
async fn obscuring_element_inside_an_open_shadow_root_is_named() {
    if !browser_available() {
        return;
    }
    let srv = fixture().await;
    let out = tempfile::tempdir().unwrap();
    let mut s = Session::launch(cfg(out.path())).await.unwrap();
    s.goto(&format!("{}/shadow_overlay.html", srv.base_url())).await.unwrap();

    let err = s.click("#under", 1_000).await.unwrap_err();
    assert_eq!(err.code(), "ELEMENT_OBSCURED", "got {err}");

    // Before the fix this said `div#over-host` — the shadow host — rather than
    // the overlay that actually covers the button.
    let msg = err.to_string();
    assert!(msg.contains("veil"), "blamed the wrong element: {msg}");
}
