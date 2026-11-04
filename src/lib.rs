//! rustplay: sandboxed headless Chromium automation.
//!
//! Module map (see AGENTS.md and docs/ARCHITECTURE.md):
//! - [`browser`]  spawn/kill the Chromium process (pipe or TCP CDP endpoint)
//! - [`cdp_pipe`] CDP over --remote-debugging-pipe (fds 3/4), no TCP port
//! - [`limits`]   wall/cpu/memory limits: watchdog + cgroups v2/v1
//! - [`procinfo`] /proc process tree + TCP listener scan (proves no CDP port)
//! - [`session`]  one isolated session + browser primitives
//! - [`actions`]  task step vocabulary and execution
//! - [`task`]     task runner and JSON reports
//! - [`suite`]    `rustplay test` runner for custom test suites
//! - [`sandbox`]  flags, env scrubbing, URL policy, path confinement, bwrap
//! - [`access`]   roles and capabilities (RBAC)
//! - [`guard`]    prompt-injection flags and PII redaction on output
//! - [`errors`]   typed errors with codes and hints
//! - [`config`]   session config; task options can only tighten it
//! - [`api`]      local HTTP API (feature `server`)

pub mod access;
pub mod actions;
#[cfg(feature = "server")]
pub mod api;
pub mod browser;
#[cfg(unix)]
pub mod cdp_pipe;
pub mod cli;
pub mod config;
pub mod errors;
pub mod guard;
pub mod limits;
pub mod procinfo;
pub mod sandbox;
pub mod session;
pub mod static_server;
pub mod suite;
pub mod task;

pub use config::SessionConfig;
pub use errors::{Result, RpError};
pub use session::Session;
pub use task::{run_task, Task, TaskReport};

/// JSON Schema for task files.
pub const TASK_SCHEMA: &str = include_str!("../docs/task.schema.json");
