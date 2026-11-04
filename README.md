# rustplay

[![ci](https://github.com/aryan597/rustplay/actions/workflows/ci.yml/badge.svg)](https://github.com/aryan597/rustplay/actions/workflows/ci.yml)
[![license: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![rust 1.87+](https://img.shields.io/badge/rust-1.87%2B-orange.svg)

**Sandboxed headless Chromium automation in one small Rust binary.** A CLI, a JSON test runner and a local HTTP API, built for running *untrusted* browser tasks (written by an LLM agent, a user, or a remote caller) without handing them your filesystem, your environment variables, or a long-lived browser.

```bash
rustplay open https://example.com --text h1 --screenshot out.png
rustplay run task.json --memory-limit 1G --wall-limit 30000
rustplay test e2e/ --base-url https://staging.example.com
rustplay serve --port 8787
```

- **One fresh browser per task.** Own 0700 temp profile, empty environment, killed and deleted afterwards on every exit path: success, error, timeout, panic, Ctrl-C.
- **No DevTools port.** CDP runs over `--remote-debugging-pipe`; every launch scans the browser's process tree and refuses to run if anything listens on TCP.
- **Hard limits.** Wall clock, total CPU and memory per session (Linux cgroups), enforced by the kernel. If a limit can't be enforced, the task fails instead of running unlimited.
- **The task can't widen policy.** Host allowlist enforced inside Chromium's resolver, http/https only, downloads off, output confined to one directory, viewer / operator / admin roles. Task options can only tighten what the operator set.
- **Agent-friendly output.** One JSON document per run, stable error codes with a hint and a retryable flag, prompt-injection flags and optional PII masking on anything read from the page.
- **Small.** About 11 MB binary and about 14 MB peak memory for the driver process, against 156 MB for Playwright's Node driver. You still need Chrome or Chromium installed.

## How fast is it?

Against Playwright 1.63, same machine (2 vCPU), same Chromium 153 binary, same local page, same steps, one fresh browser per task. Medians from [docs/BENCHMARKS.md](docs/BENCHMARKS.md):

| | rustplay | Playwright |
|---|---|---|
| One-shot CLI, headless shell (start, open page, read text, exit) | **191 ms** | 478 ms |
| One-shot CLI, full Chrome | **550 ms** | 826 ms |
| Form task, fresh browser, headless shell | **211 ms** | 255 ms |
| 20 tasks, 4 at a time, headless shell | **2.95 s** | 3.82 s |
| Driver process memory | **14 MB** | 156 MB |
| Whole tree memory (driver + browser), headless shell | **184 MB** | 324 MB |
| Form task, one shared browser, new context each | not supported yet | **152 ms** |

Playwright wins the last row: reusing one browser is cheaper than a fresh process, and rustplay doesn't offer that yet because a shared browser is weaker isolation for untrusted tasks. Run `bench/run.py` on your own hardware before relying on these numbers.

## Is this a Playwright replacement?

For one job, yes: running short, untrusted, JSON-described browser tasks in isolation, from a shell, a CI job or an agent. It is not a general Playwright replacement:

| | rustplay | Playwright |
|---|---|---|
| Scripting | JSON task files (17 actions) | full TypeScript / Python / Java / .NET API |
| Browsers | Chromium only | Chromium, Firefox, WebKit |
| Isolation default | fresh browser process + profile per task, scrubbed env, optional cgroups / bwrap | shared browser, fresh context per test |
| Resource limits | wall / CPU / memory per task | none built in |
| Untrusted task policy | roles, tighten-only task options, host allowlist in the resolver, path confinement | not a goal |
| Runtime | one static-ish binary | Node.js (or another language runtime) + driver |
| Network interception, tracing, codegen, video, mobile emulation | not yet | yes |

Both talk to Chromium over a pipe, not a TCP port. If you write tests in code, iterate locally, or need Firefox / WebKit, use Playwright.

## Quick start

Linux or macOS, Rust 1.87+, and Chrome or Chromium. **Windows: use WSL2.** The sandbox depends on fd passing, process groups, cgroups and bubblewrap, which have no Windows equivalent. On Ubuntu or WSL2, `scripts/setup-wsl.sh` installs everything.

```bash
git clone https://github.com/aryan597/rustplay && cd rustplay
cargo build --release                       # or: cargo install --path .
./target/release/rustplay doctor --pretty   # real launch; checks sandbox, CDP exposure, limits
```

Prebuilt Linux x86_64 binaries are attached to [releases](https://github.com/aryan597/rustplay/releases).

Browser lookup: `--chrome PATH`, then `$RUSTPLAY_CHROME`, then `google-chrome(-stable)`, `chromium`, `chromium-browser`, `chrome`, `chrome-headless-shell` on `PATH`, then the macOS app bundles. **Snap-packaged Chromium is refused**: snapd gives it a private `/tmp` and its own environment, so rustplay can't guarantee cleanup or confinement. On Ubuntu that includes `apt install chromium-browser`, so install Google's `.deb` or `chrome-headless-shell` instead. `chrome-headless-shell` runs a task about 2.6x faster than full Chrome and uses less memory (see [benchmarks](docs/BENCHMARKS.md)).

**Running as root** (Docker, CI): Chromium's sandbox will not start as root. Use a normal user. Otherwise pass `--no-chrome-sandbox`, ideally with `--isolation bwrap`; rustplay never drops the sandbox silently.

## Task files

```json
{
  "name": "example-domain-smoke",
  "options": { "navigation_timeout_ms": 15000, "allowed_hosts": ["example.com"], "redact_pii": true },
  "steps": [
    { "action": "goto", "url": "https://example.com" },
    { "action": "wait_for", "selector": "h1", "state": "visible" },
    { "action": "assert_text", "selector": "h1", "contains": "Example Domain" },
    { "action": "text", "selector": "p", "all": true },
    { "action": "attribute", "selector": "a", "name": "href" },
    { "action": "scroll", "y": 400 },
    { "action": "screenshot", "path": "shots/example.png", "full_page": true }
  ]
}
```

Actions: `goto`, `wait_for`, `click`, `type`, `press`, `scroll`, `text`, `attribute`, `screenshot`, `eval`, `sleep`, plus the assertions `assert_text`, `assert_visible`, `assert_hidden`, `assert_count`, `assert_url`, `assert_attribute`. `click` and `type` auto-wait until the element is visible, enabled and not covered, and if it never is, they say which. Full schema: `rustplay schema` or [docs/task.schema.json](docs/task.schema.json).

`options` can only **tighten** what the operator allowed: shorter timeouts, fewer hosts, lower limits, redaction on. A task cannot enable `eval`, file URLs, downloads or new hosts.

### Result

```json
{
  "ok": false,
  "trace_id": "3f1c9a0b7d2e4c11",
  "session_id": "84aa5b507404",
  "role": "operator",
  "transport": "pipe",
  "duration_ms": 1873,
  "untrusted_content": true,
  "steps": [
    { "index": 0, "action": "goto", "ok": true, "duration_ms": 412, "output": { "url": "http://127.0.0.1:41733/", "title": "Shop" } },
    { "index": 1, "action": "click", "ok": false, "duration_ms": 1203,
      "error": { "code": "ELEMENT_OBSCURED", "message": "cannot act on `#buy`: element is covered by div#cookie-banner",
                 "hint": "close the overlay (modal, cookie banner) first, or scroll the element clear", "retryable": false } }
  ],
  "error": { "code": "ELEMENT_OBSCURED", "...": "..." }
}
```

JSON goes to stdout, logs to stderr. Exit codes: `0` ok, `1` task failed, `2` config / policy / permission, `3` launch / sandbox / limit unsupported, `130` cancelled. All error codes: [docs/ERRORS.md](docs/ERRORS.md).

## Commands

| Command | What it does |
|---|---|
| `open URL [--wait-for SEL] [--text SEL] [--screenshot PATH] [--full-page]` | one-shot |
| `run TASK.json... [-j N \| --parallel N]` | each file in its own session, N at a time |
| `test DIR\|FILE [--serve DIR \| --base-url URL] [-j N \| --parallel N] [--full-report]` | test suite: `${BASE_URL}` substitution, PASS/FAIL on stderr, JSON on stdout; `--full-report` keeps step details for passing cases too |
| `serve [--port 8787] [--bind 127.0.0.1] [--max-sessions 4] [--tokens-file F \| --token-env VAR]` | HTTP API |
| `doctor` | environment checks and a real launch |
| `schema` | JSON Schema for task files |

Flags that work on every command:

| Area | Flags |
|---|---|
| Policy | `--allow-host H` (repeatable, `*.example.com` ok), `--role viewer\|operator\|admin`, `--allow-file-urls`, `--redact-pii` |
| Files | `--out-dir DIR`, `--overwrite`, `--downloads-dir DIR`, `--temp-root DIR` |
| Isolation | `--cdp-transport pipe\|tcp` (default `pipe`; `tcp` is deprecated), `--isolation none\|bwrap`, `--no-chrome-sandbox` |
| Timeouts | `--nav-timeout`, `--action-timeout`, `--task-timeout`, `--launch-timeout` (ms) |
| Limits | `--wall-limit MS`, `--cpu-limit MS`, `--memory-limit 512M`, `--cgroup-parent DIR` |
| Browser | `--chrome PATH`, `--viewport 1280x720`, `--headed` (debugging only) |
| Output | `--pretty`, `--log-json`, `-v` / `--verbose` (repeat for more) |

## Test suites

Any task file with `assert_*` steps is a test. Put them in a directory:

```bash
rustplay test examples/suite --serve tests/fixtures     # serves the dir on 127.0.0.1, sets ${BASE_URL}
rustplay test e2e/ --base-url https://staging.example.com --allow-host staging.example.com -j 4
```

```
PASS examples/suite/01_form.json (2227 ms)
FAIL examples/suite/05_checkout.json (1874 ms)
     ASSERTION_FAILED: assertion failed: text of `#total`: expected "£12.00", got "£0.00"
4 passed, 1 failed
```

Exits `1` if any case fails, so it drops straight into CI.

## HTTP API

```bash
RUSTPLAY_TOKEN=$(openssl rand -hex 24) rustplay serve --token-env RUSTPLAY_TOKEN --role viewer
curl -s localhost:8787/v1/tasks -H "Authorization: Bearer $RUSTPLAY_TOKEN" -d @task.json
```

| Route | |
|---|---|
| `POST /v1/tasks` | body = task JSON, response = result JSON. 400 bad task, 401 auth, 403 role, 429 `BUSY` when all browser slots are taken, otherwise 200 (check `ok`) |
| `GET /v1/schema` | task JSON Schema |
| `GET /healthz` | version, active / max sessions, served and rejected counters |

Several principals: `--tokens-file tokens.json` with `[{"token":"...","name":"ci","role":"viewer"}]` (tokens at least 16 chars). Binding a non-loopback address without tokens is refused. Build without the server: `cargo build --release --no-default-features`.

## CDP transport

| | `pipe` (default) | `tcp` (deprecated) |
|---|---|---|
| How | `--remote-debugging-pipe`: NUL-framed JSON on fds 3 and 4, inherited at spawn | `--remote-debugging-port=0` on 127.0.0.1 |
| Who can attach | only rustplay, which holds the other pipe ends | any local process that finds the port |
| Proof | `cdp_exposure` in `doctor`; a launch-time `/proc` scan fails with `CDP_PORT_EXPOSED` if anything in the tree listens | every report carries a `TCP_TRANSPORT_DEPRECATED` warning |

The pipe needs a small patch to chromiumoxide 0.9.1, vendored in `vendor/chromiumoxide`. The patch also fixes a hang when the browser dies. See [docs/upstream](docs/upstream/README.md).

## Resource limits

All opt-in and per session. A task file can only lower them.

| Flag | Task option | Enforced by | On breach |
|---|---|---|---|
| `--wall-limit MS` | `wall_limit_ms` | an OS-thread watchdog that SIGKILLs the browser's process group, even if the async runtime is stuck | `WALL_LIMIT_EXCEEDED` |
| `--cpu-limit MS` | `cpu_limit_ms` | total CPU of the browser tree from cgroup v2 `cpu.stat` (or v1 `cpuacct`), checked every 25 ms | `CPU_LIMIT_EXCEEDED` |
| `--memory-limit SIZE` | `memory_limit_mb` | cgroup v2 `memory.max` + `memory.oom.group` + `memory.swap.max=0` (or v1); the kernel enforces it | `MEMORY_LIMIT_EXCEEDED` |

The browser joins its cgroup in the forked child before `exec`, so every helper process is capped from its first instruction. Each report's `resources` field shows the limits, how each was enforced, and the measured CPU and peak memory.

On a systemd desktop a terminal's cgroup also holds your shell, and cgroup v2 won't enable controllers there. Start the binary alone in its own delegated scope; rustplay then moves itself into a leaf cgroup and caps each browser in a sibling:

```bash
systemd-run --user --scope -p Delegate=yes ./target/release/rustplay run task.json --memory-limit 1G
```

Run the binary directly, not `cargo run`, which would share the scope. Or point `--cgroup-parent` at an empty delegated cgroup v2 directory.

## Security

Read [docs/SECURITY.md](docs/SECURITY.md) before exposing rustplay to untrusted input: it covers the threat model, every layer and the test that proves it, and the known limits. The main gaps are that IP-literal subresources bypass `--allow-host`, injection flags and PII masking are heuristics, CPU and memory limits are Linux-only, and on macOS a SIGKILLed rustplay can orphan Chromium.

Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).

## Development

```bash
scripts/gate.sh   # fmt, clippy -D warnings, unit, docs drift, integration, evals, transport, limits, example suite
```

87 tests run against a real Chromium and local fixture pages; none touch the internet. The `docs_sync` tests fail when error codes, CLI flags, the task schema or the vendored patch drift from the docs.

| Doc | |
|---|---|
| [AGENTS.md](AGENTS.md) | repo map, invariants, how to add a step (start here, human or agent) |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | data flow, pipe transport, limits, why chromiumoxide |
| [docs/BENCHMARKS.md](docs/BENCHMARKS.md) | method and full results against Playwright |
| [docs/PIPELINE_STACK.md](docs/PIPELINE_STACK.md) | how rustplay covers the tool, access, guardrail and observability layers of an agent pipeline, with an eval per failure mode |
| [docs/ROADMAP.md](docs/ROADMAP.md) | browser-context pooling, network interception, MCP server, proxies, PDF, distributed workers |
| [CHANGELOG.md](CHANGELOG.md) · [CONTRIBUTING.md](CONTRIBUTING.md) | |

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

`vendor/chromiumoxide` is chromiumoxide 0.9.1 by Matthias Seitz and contributors (MIT OR Apache-2.0), with rustplay's patch; see [NOTICE](NOTICE).
