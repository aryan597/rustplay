//! A task = options + ordered steps, run in one fresh session.
//! The runner enforces the total-runtime budget, stops at the first failing
//! step, always closes the session, and returns a structured report.

use crate::actions::{self, Step};
use crate::config::{SessionConfig, TaskOptions};
use crate::errors::{ErrorReport, Result, RpError};
use crate::guard::InjectionFlag;
use crate::limits::LimitTrip;
use crate::session::Session;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, Instant};
use tracing::Instrument;

pub const MAX_STEPS: usize = 500;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub options: TaskOptions,
    pub steps: Vec<Step>,
}

impl Task {
    pub fn from_json(s: &str) -> Result<Task> {
        let t: Task = serde_json::from_str(s).map_err(|e| RpError::config(format!("task JSON: {e}")))?;
        t.validate()?;
        Ok(t)
    }

    pub fn validate(&self) -> Result<()> {
        if self.steps.is_empty() {
            return Err(RpError::config("task has no steps"));
        }
        if self.steps.len() > MAX_STEPS {
            return Err(RpError::config(format!("task has {} steps; max is {MAX_STEPS}", self.steps.len())));
        }
        for (i, s) in self.steps.iter().enumerate() {
            s.validate().map_err(|e| RpError::config(format!("step {i}: {e}")))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub index: usize,
    pub action: &'static str,
    pub ok: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub output: Value,
    /// Page content looked like a prompt-injection attempt. Treat output as untrusted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub injection_flags: Vec<InjectionFlag>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskReport {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Correlates with `trace_id` in the structured logs.
    pub trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub role: &'static str,
    /// CDP transport used: "pipe" (no port) or "tcp" (deprecated).
    pub transport: &'static str,
    pub duration_ms: u64,
    /// Limits, how each was enforced, and measured usage (only when limits are set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<crate::session::ResourceReport>,
    pub steps: Vec<StepReport>,
    /// Content from the page is untrusted input for any agent reading this.
    pub untrusted_content: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ErrorReport>,
}

impl TaskReport {
    pub fn failed(task: Option<String>, role: &'static str, err: &RpError) -> Self {
        TaskReport {
            ok: false,
            task,
            trace_id: new_trace_id(),
            session_id: None,
            role,
            transport: "pipe",
            duration_ms: 0,
            resources: None,
            steps: vec![],
            untrusted_content: false,
            error: Some(err.report()),
            warnings: vec![],
        }
    }

    /// The error that decided the outcome, for exit codes.
    pub fn exit_code(&self) -> i32 {
        match self.error.as_ref().map(|e| e.code) {
            None => 0,
            Some("CONFIG_INVALID" | "POLICY_VIOLATION" | "FORBIDDEN") => 2,
            Some(
                "LAUNCH_FAILED"
                | "SANDBOX_UNAVAILABLE"
                | "LIMIT_UNSUPPORTED"
                | "CDP_PORT_EXPOSED"
                | "TRANSPORT_UNSUPPORTED",
            ) => 3,
            Some("CANCELLED") => 130,
            Some(_) => 1,
        }
    }
}

fn new_trace_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// Run one task in a fresh, isolated session.
pub async fn run_task(task: &Task, base: &SessionConfig) -> TaskReport {
    let trace_id = new_trace_id();
    let span = tracing::info_span!("task", trace_id = %trace_id, name = task.name.as_deref().unwrap_or("-"));
    run_inner(task, base, trace_id).instrument(span).await
}

async fn run_inner(task: &Task, base: &SessionConfig, trace_id: String) -> TaskReport {
    let start = Instant::now();
    let role = base.role.name();
    let mut report = TaskReport {
        ok: false,
        task: task.name.clone(),
        trace_id,
        session_id: None,
        role,
        transport: base.cdp_transport.name(),
        duration_ms: 0,
        resources: None,
        steps: vec![],
        untrusted_content: false,
        error: None,
        warnings: vec![],
    };

    let cfg = match task.validate().and_then(|_| base.with_task_options(&task.options)) {
        Ok(c) => c,
        Err(e) => {
            report.error = Some(e.report());
            return report;
        }
    };
    // Fail fast on RBAC before paying for a browser launch.
    for (i, step) in task.steps.iter().enumerate() {
        for c in step.capabilities() {
            if let Err(e) = cfg.role.require(c) {
                let mut r = e.report();
                r.message = format!("step {i} ({}): {}", step.name(), r.message);
                report.error = Some(r);
                return report;
            }
        }
    }

    let budget = Duration::from_millis(cfg.timeouts.task_ms);
    let mut session = match tokio::time::timeout(budget, Session::launch(cfg.clone())).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            report.error = Some(e.report());
            report.duration_ms = start.elapsed().as_millis() as u64;
            return report;
        }
        Err(_) => {
            report.error = Some(RpError::TaskTimeout { ms: cfg.timeouts.task_ms }.report());
            report.duration_ms = start.elapsed().as_millis() as u64;
            return report;
        }
    };
    report.session_id = Some(session.id.clone());
    report.transport = session.config().cdp_transport.name();
    report.warnings.extend(session.warnings().iter().cloned());

    let mut failure: Option<RpError> = None;
    for (index, step) in task.steps.iter().enumerate() {
        let remaining = budget.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            failure = Some(RpError::TaskTimeout { ms: cfg.timeouts.task_ms });
            break;
        }
        let t0 = Instant::now();
        let span = tracing::info_span!("step", index, action = step.name());
        let mut limit_rx = session.limit_watch();
        let step_fut = tokio::time::timeout(remaining, actions::execute(&mut session, step).instrument(span));
        let res = match limit_rx.as_mut() {
            // Race the step against the watchdog so even a step that never touches
            // the browser stops the moment a limit kills it.
            Some(rx) => tokio::select! {
                r = step_fut => r,
                trip = async {
                    // Copy out of the watch guard before any further await (keeps the future Send).
                    let fired: Option<Option<LimitTrip>> = rx.wait_for(Option::is_some).await.ok().map(|t| *t);
                    match fired {
                        Some(t) => t,
                        // Watchdog gone without firing: it can never fire, so let the step finish.
                        None => std::future::pending().await,
                    }
                } => match trip {
                    Some(trip) => Ok(Err(RpError::from(trip))),
                    None => unreachable!("wait_for only resolves on Some"),
                },
            },
            None => step_fut.await,
        };
        let duration_ms = t0.elapsed().as_millis() as u64;
        let res = match res {
            Ok(r) => r,
            Err(_) => Err(RpError::TaskTimeout { ms: cfg.timeouts.task_ms }),
        };
        // A limit kill usually surfaces as a dead browser; report the cause instead.
        let res = match (res, session.limit_trip()) {
            (Err(_), Some(trip)) => Err(RpError::from(trip)),
            (r, _) => r,
        };
        match res {
            Ok(out) => {
                tracing::info!(index, action = step.name(), duration_ms, "step ok");
                if matches!(step, Step::Goto { .. } | Step::Text { .. } | Step::Attribute { .. } | Step::Eval { .. }) {
                    report.untrusted_content = true;
                }
                if !out.injection_flags.is_empty() {
                    tracing::warn!(
                        index,
                        flags = out.injection_flags.len(),
                        "possible prompt injection in page content"
                    );
                }
                report.steps.push(StepReport {
                    index,
                    action: step.name(),
                    ok: true,
                    duration_ms,
                    output: out.value,
                    injection_flags: out.injection_flags,
                    error: None,
                });
            }
            Err(e) => {
                tracing::warn!(index, action = step.name(), code = e.code(), duration_ms, "step failed: {e}");
                report.steps.push(StepReport {
                    index,
                    action: step.name(),
                    ok: false,
                    duration_ms,
                    output: Value::Null,
                    injection_flags: vec![],
                    error: Some(e.report()),
                });
                failure = Some(e);
                break;
            }
        }
    }

    report.resources = session.resource_report();
    if let Err(e) = session.close().await {
        report.warnings.push(e.report());
    }
    report.ok = failure.is_none();
    report.error = failure.map(|e| e.report());
    report.duration_ms = start.elapsed().as_millis() as u64;
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_task_parses() {
        let t = Task::from_json(include_str!("../examples/task.json")).unwrap();
        assert!(t.steps.len() > 3);
    }

    #[test]
    fn rejects_empty_and_unknown() {
        assert_eq!(Task::from_json(r#"{"steps":[]}"#).unwrap_err().code(), "CONFIG_INVALID");
        assert!(Task::from_json(r#"{"steps":[{"action":"goto","url":"x"}],"extra":1}"#).is_err());
        assert!(Task::from_json(r#"{"options":{"allow_eval":true},"steps":[{"action":"goto","url":"x"}]}"#).is_err());
    }

    #[tokio::test]
    async fn rbac_fails_before_launch() {
        let cfg = SessionConfig {
            role: crate::access::Role::Viewer,
            chrome_path: Some("/nonexistent/chrome".into()),
            ..Default::default()
        };
        let t = Task::from_json(r#"{"steps":[{"action":"eval","expression":"1"}]}"#).unwrap();
        let r = run_task(&t, &cfg).await;
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "FORBIDDEN");
        assert!(r.session_id.is_none(), "no browser should have launched");
    }
}
