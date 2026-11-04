//! The step vocabulary of a task, and how each step runs against a session.
//! Every step declares the capability it needs (RBAC, layer 05) and every
//! string that leaves the browser passes the output guard (layer 06).

use crate::access::Capability;
use crate::errors::{Result, RpError};
use crate::guard;
use crate::sandbox;
use crate::session::{Session, WaitState};
use base64::Engine;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;

/// Hard cap on inline base64 screenshots so a task cannot balloon the response.
const MAX_INLINE_SCREENSHOT: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Goto {
        url: String,
    },
    WaitFor {
        selector: String,
        #[serde(default)]
        state: WaitState,
        timeout_ms: Option<u64>,
    },
    Click {
        selector: String,
        timeout_ms: Option<u64>,
    },
    Type {
        selector: String,
        text: String,
        #[serde(default)]
        clear: bool,
        timeout_ms: Option<u64>,
    },
    Press {
        key: String,
        selector: Option<String>,
    },
    Scroll {
        selector: Option<String>,
        #[serde(default)]
        x: f64,
        #[serde(default)]
        y: f64,
    },
    Text {
        selector: String,
        #[serde(default)]
        all: bool,
    },
    Attribute {
        selector: String,
        name: String,
        #[serde(default)]
        all: bool,
    },
    Screenshot {
        /// Relative path inside the output dir. Omit to get base64 inline.
        path: Option<String>,
        #[serde(default)]
        full_page: bool,
        selector: Option<String>,
    },
    Eval {
        expression: String,
    },
    Observe {
        /// Subtree root. Defaults to the document.
        selector: Option<String>,
        /// Hard cap on nodes in this chunk. Default 200.
        max_nodes: Option<usize>,
        /// Hard cap on serialized bytes. Default 32 KiB.
        max_bytes: Option<usize>,
        /// Opaque cursor from a previous observe's `next_cursor`.
        cursor: Option<String>,
        #[serde(default)]
        include_hidden: bool,
    },
    Sleep {
        ms: u64,
    },
    /// Custom-test assertions. Fail the task with ASSERTION_FAILED on mismatch.
    AssertText {
        selector: String,
        equals: Option<String>,
        contains: Option<String>,
        matches: Option<String>,
    },
    AssertVisible {
        selector: String,
        timeout_ms: Option<u64>,
    },
    AssertHidden {
        selector: String,
        timeout_ms: Option<u64>,
    },
    AssertCount {
        selector: String,
        equals: u64,
    },
    AssertUrl {
        contains: String,
    },
    AssertAttribute {
        selector: String,
        name: String,
        equals: String,
    },
}

/// Every action name. Kept in sync with the enum by `parses_all_actions`
/// and with docs/task.schema.json + AGENTS.md by tests/docs_sync.rs.
pub const ACTIONS: &[&str] = &[
    "goto",
    "wait_for",
    "click",
    "type",
    "press",
    "scroll",
    "text",
    "attribute",
    "screenshot",
    "eval",
    "observe",
    "sleep",
    "assert_text",
    "assert_visible",
    "assert_hidden",
    "assert_count",
    "assert_url",
    "assert_attribute",
];

impl Step {
    pub fn name(&self) -> &'static str {
        match self {
            Step::Goto { .. } => "goto",
            Step::WaitFor { .. } => "wait_for",
            Step::Click { .. } => "click",
            Step::Type { .. } => "type",
            Step::Press { .. } => "press",
            Step::Scroll { .. } => "scroll",
            Step::Text { .. } => "text",
            Step::Attribute { .. } => "attribute",
            Step::Screenshot { .. } => "screenshot",
            Step::Eval { .. } => "eval",
            Step::Observe { .. } => "observe",
            Step::Sleep { .. } => "sleep",
            Step::AssertText { .. } => "assert_text",
            Step::AssertVisible { .. } => "assert_visible",
            Step::AssertHidden { .. } => "assert_hidden",
            Step::AssertCount { .. } => "assert_count",
            Step::AssertUrl { .. } => "assert_url",
            Step::AssertAttribute { .. } => "assert_attribute",
        }
    }

    /// Capabilities this step needs. Checked before anything touches the browser.
    pub fn capabilities(&self) -> Vec<Capability> {
        use Capability::*;
        match self {
            Step::Goto { .. } => vec![Navigate],
            Step::Click { .. } | Step::Type { .. } | Step::Press { .. } | Step::Scroll { .. } => vec![Interact],
            Step::Screenshot { path: Some(_), .. } => vec![Read, WriteFiles],
            Step::Eval { .. } => vec![Eval],
            _ => vec![Read],
        }
    }

    /// Static validation, done for the whole task before the browser starts.
    pub fn validate(&self) -> Result<()> {
        let nonempty = |what: &str, s: &str| {
            if s.trim().is_empty() {
                Err(RpError::config(format!("{}: `{what}` must not be empty", self.name())))
            } else {
                Ok(())
            }
        };
        match self {
            Step::Goto { url } => nonempty("url", url),
            Step::WaitFor { selector, .. }
            | Step::Click { selector, .. }
            | Step::Text { selector, .. }
            | Step::AssertVisible { selector, .. }
            | Step::AssertHidden { selector, .. }
            | Step::AssertCount { selector, .. } => nonempty("selector", selector),
            Step::Type { selector, .. } => nonempty("selector", selector),
            Step::Attribute { selector, name, .. } | Step::AssertAttribute { selector, name, .. } => {
                nonempty("selector", selector)?;
                nonempty("name", name)
            }
            Step::Press { key, .. } => crate::session::parse_combo(key).map(|_| ()),
            Step::Eval { expression } => nonempty("expression", expression),
            Step::Observe { selector, .. } => {
                if let Some(s) = selector {
                    nonempty("selector", s)
                } else {
                    Ok(())
                }
            }
            Step::Sleep { ms } if *ms > 60_000 => Err(RpError::config("sleep: max 60000ms")),
            Step::AssertText { selector, equals, contains, matches } => {
                nonempty("selector", selector)?;
                if equals.is_none() && contains.is_none() && matches.is_none() {
                    return Err(RpError::config("assert_text needs one of equals / contains / matches"));
                }
                if let Some(re) = matches {
                    regex::Regex::new(re).map_err(|e| RpError::config(format!("assert_text.matches: {e}")))?;
                }
                Ok(())
            }
            Step::Screenshot { path: Some(p), .. } => {
                if p.trim().is_empty() {
                    Err(RpError::config("screenshot: `path` must not be empty"))
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
}

/// What a step produced, plus guard findings.
pub struct StepOutput {
    pub value: Value,
    pub injection_flags: Vec<guard::InjectionFlag>,
}

fn step_ms(s: &Session, requested: Option<u64>) -> u64 {
    let base = s.config().timeouts.action_ms;
    requested.map_or(base, |r| r.min(base))
}

fn fail(what: impl Into<String>, expected: impl Into<String>, actual: &Value) -> RpError {
    let a = match actual {
        Value::String(s) => format!("{s:?}"),
        v => v.to_string(),
    };
    RpError::Assertion { what: what.into(), expected: expected.into(), actual: a.chars().take(200).collect() }
}

pub async fn execute(s: &mut Session, step: &Step) -> Result<StepOutput> {
    let role = s.config().role;
    for c in step.capabilities() {
        role.require(c)?;
    }
    let ms = s.config().timeouts.action_ms;
    // `observe` scans per node, so it hands its flags up rather than being
    // re-scanned wholesale below.
    let mut observe_flags = Vec::new();
    let mut value = match step {
        Step::Goto { url } => s.goto(url).await?,
        Step::WaitFor { selector, state, timeout_ms } => s.wait_for(selector, *state, step_ms(s, *timeout_ms)).await?,
        Step::Click { selector, timeout_ms } => s.click(selector, step_ms(s, *timeout_ms)).await?,
        Step::Type { selector, text, clear, timeout_ms } => {
            s.type_text(selector, text, *clear, step_ms(s, *timeout_ms)).await?
        }
        Step::Press { key, selector } => s.press(key, selector.as_deref(), ms).await?,
        Step::Scroll { selector, x, y } => s.scroll(selector.as_deref(), *x, *y, ms).await?,
        Step::Text { selector, all } => s.text(selector, *all, ms).await?,
        Step::Attribute { selector, name, all } => s.attribute(selector, name, *all, ms).await?,
        Step::Screenshot { path, full_page, selector } => {
            let format =
                match path.as_deref().and_then(|p| p.rsplit('.').next()).map(str::to_ascii_lowercase).as_deref() {
                    Some("jpg") | Some("jpeg") => CaptureScreenshotFormat::Jpeg,
                    Some("webp") => CaptureScreenshotFormat::Webp,
                    _ => CaptureScreenshotFormat::Png,
                };
            // Open the destination before capturing so a policy error costs nothing.
            let dest = match path {
                Some(p) => {
                    let c = s.config();
                    Some(sandbox::open_output(&c.out_dir, p, &["png", "jpg", "jpeg", "webp"], c.overwrite)?)
                }
                None => None,
            };
            let bytes = s.screenshot(selector.as_deref(), *full_page, format, ms).await?;
            match dest {
                Some((p, mut f)) => {
                    f.write_all(&bytes).map_err(|e| RpError::io("write screenshot", e))?;
                    json!({ "path": p.display().to_string(), "bytes": bytes.len() })
                }
                None => {
                    if bytes.len() > MAX_INLINE_SCREENSHOT {
                        return Err(RpError::policy("inline screenshot exceeds 8 MiB; pass a `path` instead"));
                    }
                    json!({ "base64": base64::engine::general_purpose::STANDARD.encode(&bytes), "bytes": bytes.len() })
                }
            }
        }
        Step::Eval { expression } => s.eval(expression).await?,
        Step::Observe { selector, max_nodes, max_bytes, cursor, include_hidden } => {
            let (v, flags) =
                s.observe(selector.as_deref(), *max_nodes, *max_bytes, cursor.as_deref(), *include_hidden).await?;
            observe_flags = flags;
            v
        }
        Step::Sleep { ms } => {
            tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
            Value::Null
        }
        Step::AssertText { selector, equals, contains, matches } => {
            let actual = s.text(selector, false, ms).await?;
            let text = actual.as_str().unwrap_or_default().trim().to_string();
            if let Some(e) = equals {
                if text != e.trim() {
                    return Err(fail(format!("text of `{selector}`"), format!("{e:?}"), &actual));
                }
            }
            if let Some(c) = contains {
                if !text.contains(c.as_str()) {
                    return Err(fail(format!("text of `{selector}`"), format!("to contain {c:?}"), &actual));
                }
            }
            if let Some(m) = matches {
                let re = regex::Regex::new(m).map_err(|e| RpError::config(e.to_string()))?;
                if !re.is_match(&text) {
                    return Err(fail(format!("text of `{selector}`"), format!("to match /{m}/"), &actual));
                }
            }
            json!({ "passed": true })
        }
        Step::AssertVisible { selector, timeout_ms } => {
            let t = step_ms(s, *timeout_ms);
            s.wait_for(selector, WaitState::Visible, t)
                .await
                .map_err(|e| fail(format!("`{selector}`"), "visible", &json!(e.code())))?;
            json!({ "passed": true })
        }
        Step::AssertHidden { selector, timeout_ms } => {
            let t = step_ms(s, *timeout_ms);
            s.wait_for(selector, WaitState::Hidden, t)
                .await
                .map_err(|_| fail(format!("`{selector}`"), "hidden", &json!("visible")))?;
            json!({ "passed": true })
        }
        Step::AssertCount { selector, equals } => {
            let n = s.count(selector).await?;
            if n != *equals {
                return Err(fail(format!("count of `{selector}`"), equals.to_string(), &json!(n)));
            }
            json!({ "passed": true })
        }
        Step::AssertUrl { contains } => {
            let u = s.eval("location.href").await?;
            if !u.as_str().unwrap_or_default().contains(contains.as_str()) {
                return Err(fail("page url", format!("to contain {contains:?}"), &u));
            }
            json!({ "passed": true })
        }
        Step::AssertAttribute { selector, name, equals } => {
            let v = s.attribute(selector, name, false, ms).await?;
            if v.as_str() != Some(equals.as_str()) {
                return Err(fail(format!("`{selector}`[{name}]"), format!("{equals:?}"), &v));
            }
            json!({ "passed": true })
        }
    };

    // Layer 06: page-derived strings are untrusted. Flag injection, mask PII.
    let page_derived =
        matches!(step, Step::Text { .. } | Step::Attribute { .. } | Step::Eval { .. } | Step::Goto { .. });
    let injection_flags = if page_derived { guard::scan_value(&value) } else { observe_flags };
    if page_derived && s.config().redact_pii {
        guard::redact_value(&mut value);
    }
    Ok(StepOutput { value, injection_flags })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::Role;

    #[test]
    fn parses_all_actions() {
        let j = r##"[
          {"action":"goto","url":"https://example.com"},
          {"action":"wait_for","selector":"h1","state":"attached"},
          {"action":"click","selector":"a"},
          {"action":"type","selector":"input","text":"hi","clear":true},
          {"action":"press","key":"Enter"},
          {"action":"scroll","y":500},
          {"action":"text","selector":"h1"},
          {"action":"attribute","selector":"a","name":"href","all":true},
          {"action":"screenshot","path":"a.png","full_page":true},
          {"action":"eval","expression":"1+1"},
          {"action":"observe"},
          {"action":"sleep","ms":10},
          {"action":"assert_text","selector":"h1","contains":"Ex"},
          {"action":"assert_visible","selector":"h1"},
          {"action":"assert_hidden","selector":"#x"},
          {"action":"assert_count","selector":"p","equals":2},
          {"action":"assert_url","contains":"example"},
          {"action":"assert_attribute","selector":"a","name":"href","equals":"/"}
        
        ]"##;
        let steps: Vec<Step> = serde_json::from_str(j).unwrap();
        let names: Vec<&str> = steps.iter().map(Step::name).collect();
        assert_eq!(names, ACTIONS, "update ACTIONS, the schema and AGENTS.md when adding a step");
        assert!(steps.iter().all(|s| s.validate().is_ok()));
    }

    #[test]
    fn rejects_unknown_fields_and_actions() {
        assert!(serde_json::from_str::<Step>(r#"{"action":"click","selector":"a","force":true}"#).is_err());
        assert!(serde_json::from_str::<Step>(r#"{"action":"shell","cmd":"rm -rf /"}"#).is_err());
    }

    #[test]
    fn static_validation() {
        let bad = [
            r#"{"action":"click","selector":"  "}"#,
            r#"{"action":"press","key":"Hyper+x"}"#,
            r#"{"action":"sleep","ms":999999}"#,
            r#"{"action":"assert_text","selector":"h1"}"#,
            r#"{"action":"assert_text","selector":"h1","matches":"("}"#,
        ];
        for b in bad {
            let s: Step = serde_json::from_str(b).unwrap();
            assert_eq!(s.validate().unwrap_err().code(), "CONFIG_INVALID", "{b}");
        }
    }

    #[test]
    fn capabilities_map_to_roles() {
        let eval: Step = serde_json::from_str(r#"{"action":"eval","expression":"1"}"#).unwrap();
        assert!(eval.capabilities().iter().any(|c| !Role::Operator.allows(*c)));
        let shot: Step = serde_json::from_str(r#"{"action":"screenshot","path":"a.png"}"#).unwrap();
        assert!(shot.capabilities().iter().any(|c| !Role::Viewer.allows(*c)));
        let inline: Step = serde_json::from_str(r#"{"action":"screenshot"}"#).unwrap();
        assert!(inline.capabilities().iter().all(|c| Role::Viewer.allows(*c)));
    }
}
