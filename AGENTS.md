# AGENTS.md

Guide for coding agents (and humans) working on rustplay. Read this first; it is short on purpose and points to the deeper docs.

## What this is

A single Rust binary that drives headless Chromium over CDP for untrusted automation tasks. Three entry points share one engine:

| Entry | Code | Purpose |
|---|---|---|
| `rustplay open URL` | `main.rs` -> `task::run_task` | one-shot navigate / extract / screenshot |
| `rustplay run task.json...` | `main.rs` -> `task::run_task` | run task files, each in its own session |
| `rustplay test DIR` | `suite.rs` | custom test suites (task files with `assert_*` steps) |
| `rustplay serve` | `api.rs` (feature `server`) | local HTTP API, `POST /v1/tasks` |

## Repo map

```
src/
  main.rs          CLI dispatch, Ctrl-C handling, exit codes, `doctor`
  cli.rs           clap definitions -> SessionConfig
  config.rs        SessionConfig, TaskOptions (tighten-only merge)
  browser.rs       spawn Chromium ourselves: env_clear, process group, PDEATHSIG, cgroup join +
                   CDP pipe fds in pre_exec, stderr tail, kill
  cdp_pipe.rs      CDP over --remote-debugging-pipe: fd 3/4 install, NUL framing, PipeTransport
  limits.rs        wall/cpu/memory limits: cgroup v2/v1 discovery, SessionCgroups, Watchdog thread
  procinfo.rs      /proc process tree + TCP listener scan (proves pipe mode exposes no port)
  session.rs       one isolated session + primitives (goto, click, type, ...), JS probes
  actions.rs       Step enum (task vocabulary), RBAC check, output guard
  task.rs          runner: budget, fail-fast, always close, TaskReport
  suite.rs         `rustplay test`: discover, ${BASE_URL}, PASS/FAIL summary
  sandbox.rs       Chromium flags, env scrub, URL policy, output-path confinement, bwrap argv
  access.rs        Role/Capability matrix, API token table
  guard.rs         prompt-injection flags + PII redaction on page-derived output
  errors.rs        RpError: code(), hint(), retryable(), exit_code()
  static_server.rs loopback static server for fixtures and suites
  api.rs           axum router, auth, 429 burst control
tests/
  integration.rs   end-to-end against real Chromium + tests/fixtures
  evals.rs         one test per pipeline failure mode (merge gate)
  transport.rs     pipe vs tcp: no-port proof with positive control, crash/launch-failure paths
  limits.rs        real OOM / CPU / wall kills against the kernel, cleanup of cgroups
  docs_sync.rs     fails if docs drift from code (codes, schema, flags, vendored patch)
  fixtures/        index.html + data.txt (no external sites, ever)
examples/
  task.json        sample task
  suite/*.json     sample custom test suite
docs/              ARCHITECTURE, SECURITY, ERRORS, PIPELINE_STACK, ROADMAP, task.schema.json
docs/upstream/     chromiumoxide transport patch + rationale (applies to vendored crate)
vendor/chromiumoxide  chromiumoxide 0.9.1 + that patch, via [patch.crates-io]
scripts/gate.sh    the merge gate: fmt, clippy -D warnings, all tests, CLI-only build
scripts/setup-wsl.sh  one-time Ubuntu/WSL2 setup (build tools, rustup, Chrome .deb, bwrap)
bench/             rustplay vs Playwright benchmark (run.py + playwright_bench.mjs); results in docs/BENCHMARKS.md
```

## Step vocabulary

Source of truth: `actions::Step` and `actions::ACTIONS`. Schema: `docs/task.schema.json` (`rustplay schema`).

| Action | Capability | Notes |
|---|---|---|
| `goto` | navigate | URL policy in `sandbox::check_url`; output `{url,title}` |
| `wait_for` | read | states: attached, visible (default), hidden, detached |
| `click` | interact | auto-waits: visible, enabled, not covered; real mouse events |
| `type` | interact | key events; non-US chars via Input.insertText; `clear` optional |
| `press` | interact | `Enter`, `Tab`, `Shift+Tab`, `Control+a`; optional `selector` to focus |
| `scroll` | interact | into view (`selector`) or by `x`/`y` |
| `text` | read | innerText, `all` for every match; untrusted |
| `attribute` | read | `name`, `all`; untrusted |
| `screenshot` | read (+write_files with `path`) | path confined to `--out-dir`; else inline base64 |
| `eval` | eval (admin only) | page context, promises awaited |
| `observe` | read | compact page outline, ref-addressable |
| `sleep` | read | max 60000 ms; prefer `wait_for` |
| `assert_text` | read | `equals` / `contains` / `matches` (regex) |
| `assert_visible` | read | |
| `assert_hidden` | read | |
| `assert_count` | read | |
| `assert_url` | read | |
| `assert_attribute` | read | |

## Invariants (do not break)

1. **Operator config is a ceiling.** `TaskOptions` may only tighten (`config::with_task_options`). Never add a task option that loosens policy (enable eval, file URLs, downloads, more hosts, longer timeouts).
2. **Every Chromium process dies.** On `close()`, `Drop`, panic, timeout and Ctrl-C. Process group kill + `PR_SET_PDEATHSIG` + `kill_on_drop`. Never signal a pid after the child is reaped (`BrowserProcess::kill_now` guards this).
3. **No host env reaches the browser.** `env_clear()` then `sandbox::child_env`. Tested via `/proc/<pid>/environ` in `evals.rs`.
4. **Task output paths are confined.** Only via `sandbox::open_output` (relative, no `..`, no symlink escape, extension allowlist, O_EXCL unless `--overwrite`).
5. **RBAC is checked before launch and per step.** New steps must declare `capabilities()`.
6. **Page-derived output is untrusted.** New read steps must go through the guard in `actions::execute` (`page_derived`).
7. **Errors are typed.** Add a variant to `RpError` with `code()` and `hint()`, then document the code in `docs/ERRORS.md` (enforced by `docs_sync.rs`).
8. **Tests never touch the internet.** Use `tests/fixtures` via `static_server`.
9. **Chromium's sandbox stays on by default.** `--no-chrome-sandbox` is explicit and refused as root without it.
10. **No DevTools port by default.** `--cdp-transport pipe` is the default; never add `--remote-debugging-port` in pipe mode. The launch-time `/proc` scan (`CDP_PORT_EXPOSED`) and `l06_no_cdp_port_exposed_by_browser_tree` enforce it. TCP stays explicit, deprecated, and warned in every report.
11. **The per-session `--user-data-dir` is mandatory.** `sandbox::ensure_session_profile` runs before every spawn; do not add an option to reuse or omit a profile.
12. **Limits fail closed.** A requested cpu/memory limit that cannot be enforced is `LIMIT_UNSUPPORTED`, never a warning. Join cgroups in `pre_exec` (before exec), not after spawn. Stop the watchdog before reaping the browser. cgroup self-relocation (`limits::enable_controller`) only ever moves rustplay's own pid, never anyone else's.
13. **`pre_exec` code is async-signal-safe only**: raw syscalls (`dup2`, `fcntl`, `write`, `prctl`), no allocation, no locks, no logging.
14. **No snap browsers.** `sandbox::find_chrome` refuses snap-packaged Chromium (private `/tmp`, rebuilt env). CI uses Chrome for Testing; never `apt install chromium-browser` on Ubuntu.
15. **`vendor/chromiumoxide` changes go through the patch.** Edit the vendored source, then regenerate `docs/upstream/chromiumoxide-cdp-transport.patch` against pristine 0.9.1; `vendored_chromiumoxide_matches_patch` checks they agree.

## Adding a step (checklist)

1. Variant in `actions::Step` (+ `deny_unknown_fields` is inherited), `name()`, `ACTIONS`, `capabilities()`, `validate()`.
2. Primitive in `session.rs` if it needs new browser behaviour; respect `action_ms` via `with_timeout`.
3. Arm in `actions::execute`; decide if it is `page_derived`.
4. Variant in `docs/task.schema.json` and a row in the table above.
5. Fixture element in `tests/fixtures/index.html`, integration test, and an eval if it adds risk.
6. `scripts/gate.sh` must pass.

## Commands

```bash
scripts/gate.sh                       # full gate (what CI runs)
cargo test --lib                      # fast unit tests, no browser
cargo test --test integration         # needs Chrome/Chromium
cargo test --test evals               # pipeline failure-mode evals
cargo test --test transport           # pipe/tcp transport proofs
cargo test --test limits              # cgroup + watchdog limits (real kernel)
RUSTPLAY_CHROME=/path/to/chrome cargo test
cargo build --release --no-default-features   # CLI-only binary (no HTTP server)
```

As root (containers/CI), tests disable Chromium's own sandbox automatically; the CLI requires `--no-chrome-sandbox`.

Limit tests assert real enforcement where the host can enforce, and a clear `LIMIT_UNSUPPORTED` otherwise; they never pass by skipping silently. `rustplay doctor` shows which mechanism this host uses (this dev VM: cpu via cgroup2, memory via cgroup1).

## Output contract

- stdout: exactly one JSON document (object, or array for `run` with several files). Logs go to stderr.
- Every error: `{code, message, hint?, retryable}`. Codes are stable API; see `docs/ERRORS.md`.
- Exit: 0 ok, 1 task failed, 2 config/policy/forbidden, 3 launch/sandbox/transport/limit-unsupported, 130 cancelled.
- Reports carry `transport` ("pipe" or "tcp") and, when limits are set, `resources` {limits, enforcement, cpu_used_ms, peak_memory_bytes}.
