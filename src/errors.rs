//! Typed error model. Every error has a stable machine-readable code,
//! a concise message and, where useful, a suggested fix.

use serde::Serialize;
use std::io;
use thiserror::Error;

/// Why an element could not be acted on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NotActionable {
    Hidden,
    Disabled,
    Detached,
    ZeroSize,
    NotEditable,
    Obscured { by: String },
}

impl std::fmt::Display for NotActionable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hidden => write!(f, "element is hidden"),
            Self::Disabled => write!(f, "element is disabled"),
            Self::Detached => write!(f, "element was detached from the DOM"),
            Self::ZeroSize => write!(f, "element has zero size"),
            Self::NotEditable => write!(f, "element is not editable"),
            Self::Obscured { by } => write!(f, "element is covered by {by}"),
        }
    }
}

#[derive(Debug, Error)]
pub enum RpError {
    #[error("browser failed to launch: {reason}")]
    Launch { reason: String, hint: Option<String> },

    #[error("navigation to {url} timed out after {ms}ms")]
    NavigationTimeout { url: String, ms: u64 },

    #[error("navigation to {url} failed: {reason}")]
    Network { url: String, reason: String },

    #[error("no element matches `{selector}` after {ms}ms")]
    SelectorNotFound { selector: String, ms: u64 },

    #[error("invalid selector `{selector}`: {reason}")]
    SelectorInvalid { selector: String, reason: String },

    #[error("cannot act on `{selector}`: {reason}")]
    NotActionable { selector: String, reason: NotActionable },

    #[error("javascript evaluation failed: {message}")]
    JsEval { message: String },

    #[error("browser crashed or CDP session disconnected: {reason}")]
    Disconnected { reason: String },

    #[error("action `{action}` timed out after {ms}ms")]
    ActionTimeout { action: String, ms: u64 },

    #[error("task exceeded its total runtime of {ms}ms")]
    TaskTimeout { ms: u64 },

    #[error("blocked by sandbox policy: {reason}")]
    Policy { reason: String },

    #[error("permission denied: role `{role}` lacks capability `{capability}`")]
    Forbidden { role: String, capability: String },

    #[error("sandbox unavailable: {reason}")]
    SandboxUnavailable { reason: String },

    #[error("session cleanup failed: {reason}")]
    Cleanup { reason: String },

    #[error("invalid configuration: {reason}")]
    Config { reason: String },

    #[error("i/o error ({context}): {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },

    #[error("assertion failed: {what}: expected {expected}, got {actual}")]
    Assertion { what: String, expected: String, actual: String },

    #[error("{} limit exceeded: {}", kind.name(), fmt_limit(*kind, *limit, *observed))]
    LimitExceeded { kind: crate::limits::LimitKind, limit: u64, observed: u64 },

    #[error("cannot enforce the {limit} limit: {reason}")]
    LimitUnsupported { limit: String, reason: String },

    #[error("pipe-mode browser is listening on TCP {addrs}; refusing to run")]
    CdpPortExposed { addrs: String },

    #[error("CDP transport unsupported: {reason}")]
    TransportUnsupported { reason: String },

    #[error(
        "--cdp-transport tcp is deprecated: DevTools listens on a loopback TCP port any local process can connect to"
    )]
    TcpTransportDeprecated,

    #[error("cancelled")]
    Cancelled,
}

fn fmt_limit(kind: crate::limits::LimitKind, limit: u64, observed: u64) -> String {
    use crate::limits::LimitKind;
    match kind {
        LimitKind::Wall | LimitKind::Cpu => format!("{observed} ms used, limit {limit} ms"),
        LimitKind::Memory => format!("OOM at limit {} MiB (peak {} MiB)", limit >> 20, observed >> 20),
    }
}

pub type Result<T, E = RpError> = std::result::Result<T, E>;

/// JSON shape of an error in every output.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ErrorReport {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub retryable: bool,
}

impl RpError {
    pub fn config(reason: impl Into<String>) -> Self {
        Self::Config { reason: reason.into() }
    }
    pub fn policy(reason: impl Into<String>) -> Self {
        Self::Policy { reason: reason.into() }
    }
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io { context: context.into(), source }
    }

    /// Stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Launch { .. } => "LAUNCH_FAILED",
            Self::NavigationTimeout { .. } => "NAV_TIMEOUT",
            Self::Network { .. } => "NAV_NETWORK",
            Self::SelectorNotFound { .. } => "SELECTOR_NOT_FOUND",
            Self::SelectorInvalid { .. } => "SELECTOR_INVALID",
            Self::NotActionable { reason, .. } => match reason {
                NotActionable::Hidden => "ELEMENT_HIDDEN",
                NotActionable::Disabled => "ELEMENT_DISABLED",
                NotActionable::Detached => "ELEMENT_DETACHED",
                NotActionable::ZeroSize => "ELEMENT_ZERO_SIZE",
                NotActionable::NotEditable => "ELEMENT_NOT_EDITABLE",
                NotActionable::Obscured { .. } => "ELEMENT_OBSCURED",
            },
            Self::JsEval { .. } => "JS_EVAL_FAILED",
            Self::Disconnected { .. } => "BROWSER_DISCONNECTED",
            Self::ActionTimeout { .. } => "ACTION_TIMEOUT",
            Self::TaskTimeout { .. } => "TASK_TIMEOUT",
            Self::Policy { .. } => "POLICY_VIOLATION",
            Self::Forbidden { .. } => "FORBIDDEN",
            Self::SandboxUnavailable { .. } => "SANDBOX_UNAVAILABLE",
            Self::Cleanup { .. } => "CLEANUP_FAILED",
            Self::Config { .. } => "CONFIG_INVALID",
            Self::Io { .. } => "IO_ERROR",
            Self::Assertion { .. } => "ASSERTION_FAILED",
            Self::LimitExceeded { kind, .. } => match kind {
                crate::limits::LimitKind::Wall => "WALL_LIMIT_EXCEEDED",
                crate::limits::LimitKind::Cpu => "CPU_LIMIT_EXCEEDED",
                crate::limits::LimitKind::Memory => "MEMORY_LIMIT_EXCEEDED",
            },
            Self::LimitUnsupported { .. } => "LIMIT_UNSUPPORTED",
            Self::CdpPortExposed { .. } => "CDP_PORT_EXPOSED",
            Self::TransportUnsupported { .. } => "TRANSPORT_UNSUPPORTED",
            Self::TcpTransportDeprecated => "TCP_TRANSPORT_DEPRECATED",
            Self::Cancelled => "CANCELLED",
        }
    }

    /// Suggested fix, if there is a useful one.
    pub fn hint(&self) -> Option<String> {
        let s = match self {
            Self::Launch { hint, .. } => return hint.clone(),
            Self::NavigationTimeout { .. } => {
                "the page never fired `load`; raise --nav-timeout or check the server is responding"
            }
            Self::Network { reason, .. } => {
                if reason.contains("NAME_NOT_RESOLVED") {
                    "DNS lookup failed; check the hostname, or whether --allow-host blocks it"
                } else if reason.contains("CONNECTION_REFUSED") {
                    "nothing is listening at that address; check host and port"
                } else if reason.contains("FILE_NOT_FOUND") || reason.contains("ACCESS_DENIED") {
                    "file not found or not readable; under --isolation bwrap only system dirs and the session dir are visible"
                } else if reason.contains("UNSAFE_PORT") {
                    "Chromium blocks this port as unsafe; use a different port"
                } else if reason.contains("CERT") {
                    "TLS certificate error; the site's certificate is invalid or untrusted"
                } else {
                    "network error; check connectivity and the URL"
                }
            }
            Self::SelectorNotFound { .. } => {
                "check the selector in devtools, add a `wait_for` step, or raise the step timeout"
            }
            Self::SelectorInvalid { .. } => "use a valid CSS selector (document.querySelector syntax)",
            Self::NotActionable { reason, .. } => match reason {
                NotActionable::Hidden | NotActionable::ZeroSize => {
                    "wait for the element to become visible (`wait_for` with state=visible)"
                }
                NotActionable::Disabled => "wait until the element is enabled, or target another element",
                NotActionable::Detached => "the page re-rendered; retry the step",
                NotActionable::NotEditable => "target an <input>, <textarea> or contenteditable element",
                NotActionable::Obscured { .. } => {
                    "close the overlay (modal, cookie banner) first, or scroll the element clear"
                }
            },
            Self::JsEval { .. } => "fix the script; the exception text is in the message",
            Self::Disconnected { .. } => {
                "the browser died (often OOM or a renderer crash); rerun, or lower --parallel / --max-sessions"
            }
            Self::ActionTimeout { .. } => "raise --action-timeout or the step's timeout_ms",
            Self::TaskTimeout { .. } => "raise --task-timeout or split the task",
            Self::Policy { .. } => "the request is outside the sandbox policy; see `rustplay --help` for allow flags",
            Self::Forbidden { .. } => "run with a role that grants this capability (see --role)",
            Self::SandboxUnavailable { .. } => {
                "install bubblewrap (`apt install bubblewrap`) or run with --isolation none"
            }
            Self::Cleanup { .. } => "a temp profile may be left in the system temp dir; delete rustplay-* dirs",
            Self::Assertion { .. } => "the page did not match the expectation; check the selector and expected value",
            Self::LimitExceeded { kind, .. } => match kind {
                crate::limits::LimitKind::Wall => "the browser was killed at the wall-clock limit; raise --wall-limit or split the task",
                crate::limits::LimitKind::Cpu => "the browser tree used its CPU budget (busy page or heavy JS); raise --cpu-limit",
                crate::limits::LimitKind::Memory => "the page exceeded --memory-limit and was OOM-killed; raise it or avoid heavy pages",
            },
            Self::LimitUnsupported { .. } => {
                "cpu/memory limits need Linux cgroups: start the binary alone in a delegated scope \
                 (`systemd-run --user --scope -p Delegate=yes ./target/release/rustplay ...`), \
                 or pass --cgroup-parent DIR (an empty delegated cgroup v2 dir), or drop the limit"
            }
            Self::CdpPortExposed { .. } => {
                "this should never happen in pipe mode; check for a wrapper adding --remote-debugging-port and report a bug"
            }
            Self::TransportUnsupported { .. } => "use --cdp-transport tcp on this platform (deprecated)",
            Self::TcpTransportDeprecated => "drop --cdp-transport tcp; pipe is the default and opens no port",
            Self::Config { .. } => return None,
            Self::Io { .. } => return None,
            Self::Cancelled => return None,
        };
        Some(s.to_string())
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::NavigationTimeout { .. }
                | Self::Network { .. }
                | Self::Disconnected { .. }
                | Self::ActionTimeout { .. }
                | Self::NotActionable { reason: NotActionable::Detached, .. }
        )
    }

    pub fn report(&self) -> ErrorReport {
        ErrorReport { code: self.code(), message: self.to_string(), hint: self.hint(), retryable: self.retryable() }
    }

    /// Process exit code for the CLI.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config { .. } | Self::Policy { .. } | Self::Forbidden { .. } => 2,
            Self::Launch { .. }
            | Self::SandboxUnavailable { .. }
            | Self::LimitUnsupported { .. }
            | Self::CdpPortExposed { .. }
            | Self::TransportUnsupported { .. } => 3,
            Self::Cancelled => 130,
            _ => 1,
        }
    }

    /// Map a chromiumoxide error raised while doing `action`.
    pub fn from_cdp(action: &str, e: chromiumoxide::error::CdpError) -> Self {
        use chromiumoxide::error::CdpError as C;
        match e {
            C::JavascriptException(d) => Self::JsEval { message: describe_exception(&d) },
            C::Ws(_) | C::ChannelSendError(_) | C::NoResponse => {
                Self::Disconnected { reason: format!("{action}: {e}") }
            }
            C::Timeout => Self::ActionTimeout { action: action.to_string(), ms: 0 },
            other => Self::Disconnected { reason: format!("{action}: {other}") },
        }
    }
}

pub(crate) fn describe_exception(d: &chromiumoxide::cdp::js_protocol::runtime::ExceptionDetails) -> String {
    let desc = d.exception.as_ref().and_then(|o| o.description.clone()).unwrap_or_else(|| d.text.clone());
    let first = desc.lines().next().unwrap_or("").trim().to_string();
    format!("{first} (line {}, col {})", d.line_number + 1, d.column_number + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_hinted() {
        let e = RpError::NotActionable {
            selector: "#x".into(),
            reason: NotActionable::Obscured { by: "div#modal".into() },
        };
        assert_eq!(e.code(), "ELEMENT_OBSCURED");
        assert!(e.hint().unwrap().contains("overlay"));
        assert!(e.to_string().contains("div#modal"));
        assert_eq!(e.exit_code(), 1);
    }

    #[test]
    fn report_serializes() {
        let e = RpError::Network { url: "http://x.invalid".into(), reason: "net::ERR_NAME_NOT_RESOLVED".into() };
        let j = serde_json::to_value(e.report()).unwrap();
        assert_eq!(j["code"], "NAV_NETWORK");
        assert_eq!(j["retryable"], true);
        assert!(j["hint"].as_str().unwrap().contains("DNS"));
    }

    #[test]
    fn config_errors_exit_2() {
        assert_eq!(RpError::config("bad").exit_code(), 2);
        assert_eq!(RpError::config("bad").hint(), None);
        assert_eq!(RpError::Cancelled.exit_code(), 130);
    }
}
