//! `rustplay test`: run a directory of task files as a test suite.
//! Each `*.json` is one test case; `assert_*` steps are the checks.
//! `${BASE_URL}` in any string is replaced, so suites run against a local
//! server (`--serve DIR`) or a staging host (`--base-url`).

use crate::config::SessionConfig;
use crate::errors::{Result, RpError};
use crate::task::{run_task, Task, TaskReport};
use futures::stream::{self, StreamExt};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
pub struct CaseResult {
    pub file: String,
    pub ok: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<TaskReport>,
}

#[derive(Debug, Serialize)]
pub struct SuiteReport {
    pub ok: bool,
    pub passed: usize,
    pub failed: usize,
    pub duration_ms: u64,
    pub cases: Vec<CaseResult>,
}

/// Collect `*.json` files (sorted). A single file is also accepted.
pub fn discover(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let rd = std::fs::read_dir(path).map_err(|e| RpError::io(format!("read suite dir {}", path.display()), e))?;
    let mut files: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json") && p.is_file())
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(RpError::config(format!("no *.json test cases in {}", path.display())));
    }
    Ok(files)
}

pub fn substitute(raw: &str, base_url: Option<&str>) -> String {
    match base_url {
        Some(b) => raw.replace("${BASE_URL}", b.trim_end_matches('/')),
        None => raw.to_string(),
    }
}

pub async fn run_suite(
    files: &[PathBuf],
    cfg: &SessionConfig,
    base_url: Option<&str>,
    parallel: usize,
    verbose: bool,
) -> SuiteReport {
    let start = std::time::Instant::now();
    let cases: Vec<CaseResult> = stream::iter(files.iter().cloned())
        .map(|f| async move {
            let name = f.display().to_string();
            let task = std::fs::read_to_string(&f)
                .map_err(|e| RpError::io(format!("read {name}"), e))
                .and_then(|s| Task::from_json(&substitute(&s, base_url)));
            let report = match task {
                Ok(t) => run_task(&t, cfg).await,
                Err(e) => TaskReport::failed(Some(name.clone()), cfg.role.name(), &e),
            };
            let failed_step = report.steps.iter().find(|s| !s.ok).map(|s| s.index);
            let err = report.error.clone();
            eprintln!(
                "{} {name} ({} ms){}",
                if report.ok { "PASS" } else { "FAIL" },
                report.duration_ms,
                err.as_ref().map(|e| format!("\n     {}: {}", e.code, e.message)).unwrap_or_default()
            );
            CaseResult {
                file: name,
                ok: report.ok,
                duration_ms: report.duration_ms,
                failed_step,
                code: err.as_ref().map(|e| e.code),
                message: err.map(|e| e.message),
                report: if verbose || !report.ok { Some(report) } else { None },
            }
        })
        .buffer_unordered(parallel.max(1))
        .collect()
        .await;
    let mut cases = cases;
    cases.sort_by(|a, b| a.file.cmp(&b.file));
    let passed = cases.iter().filter(|c| c.ok).count();
    let failed = cases.len() - passed;
    eprintln!("\n{passed} passed, {failed} failed");
    SuiteReport { ok: failed == 0, passed, failed, duration_ms: start.elapsed().as_millis() as u64, cases }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_base_url() {
        assert_eq!(
            substitute(r#"{"url":"${BASE_URL}/a"}"#, Some("http://127.0.0.1:9/")),
            r#"{"url":"http://127.0.0.1:9/a"}"#
        );
        assert_eq!(substitute("${BASE_URL}", None), "${BASE_URL}");
    }

    #[test]
    fn discovers_example_suite() {
        let files = discover(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/suite"))).unwrap();
        assert!(files.len() >= 3);
        for f in files {
            let s = std::fs::read_to_string(&f).unwrap();
            Task::from_json(&substitute(&s, Some("http://127.0.0.1:1")))
                .unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        }
    }
}
