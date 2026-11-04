# Architecture

```
 CLI (open/run/test)        HTTP API (serve, feature "server")
        |                         |  auth -> Principal{role}, 429 when full
        v                         v
   task::run_task  <-------------+        one task = one fresh session
        |  validate -> merge TaskOptions (tighten only) -> RBAC pre-check
        v
   Session::launch
        |  temp root 0700: profile/ cache/ home/ tmp/   (+ downloads/<id>/ if enabled)
        |  SessionCgroups::create (only if cpu/memory limits): v2 or v1, fail closed
        |  BrowserProcess::launch: env_clear, own process group, PDEATHSIG,
        |      join cgroups + install CDP pipe fds 3/4 in pre_exec,
        |      hardened flags (+ mandatory --user-data-dir check), optional bwrap
        |  Watchdog thread (wall / cpu / OOM) -> SIGKILL group, notify runner
        |  Browser::connect_with_transport(PipeTransport)   [tcp: connect(ws://)]
        |  first command (setDownloadBehavior) = readiness probe; /proc listener scan
        v
   for step in steps (fail fast, inside total budget):
        actions::execute: role.require(caps) -> session primitive -> output guard
        |
   Session::close (graceful) / Drop (forced): CDP Browser.close -> wait -> SIGKILL group -> rm -rf temp root
        v
   TaskReport JSON (trace_id, per-step duration/output/error, injection flags, warnings)
```

## Why chromiumoxide over headless_chrome

| | chromiumoxide 0.9 | headless_chrome |
|---|---|---|
| Async model | native async, tokio | blocking, one thread per tab |
| CDP coverage | full protocol, generated from the PDL | hand-written subset |
| Fits tokio concurrency | yes | needs `spawn_blocking` everywhere |
| Can attach to a process we spawned | `Browser::connect` | yes |

With tokio required and many concurrent sessions, chromiumoxide is the maintainable choice: one runtime, no thread-per-tab, and new CDP domains come with the generated types. The cost is binary size (the generated types are most of the ~10 MB binary) and a less stable high-level API, which is why rustplay uses only `connect`, `new_page`, `goto`, `evaluate_expression`, `click(Point)`, `screenshot` and raw `execute`.

## CDP transport: pipe by default

`--remote-debugging-pipe` makes Chromium read CDP from fd 3 and write to fd 4, one JSON message per NUL-terminated frame. rustplay:

1. creates two pipes with `std::io::pipe()` (O_CLOEXEC),
2. in `pre_exec`, duplicates Chrome's ends to fds >= 10 and then `dup2`s them onto 3 and 4 (a source already numbered 3 or 4 would otherwise be clobbered; the first manual probe hit exactly this),
3. closes Chrome's ends in the parent so EOF tracks Chrome's lifetime,
4. runs a reader and a writer tokio task (`cdp_pipe::PipeTransport`) behind channels, adapted to chromiumoxide's `CdpTransport` trait.

There is no "DevTools listening" line in pipe mode, so the first CDP command doubles as the readiness probe; on EOF or timeout the launch error carries Chromium's last stderr lines and exit status. After connecting, the browser's whole process tree is scanned (`procinfo::tcp_listeners`: walk `/proc/*/stat` by parent and process group, map `socket:[inode]` fds to `/proc/<pid>/net/tcp{,6}` LISTEN rows). Any listener fails the launch with `CDP_PORT_EXPOSED`.

### chromiumoxide limitations found

chromiumoxide 0.9.1 has no pipe support and no way to inject a transport (`Connection` holds a concrete websocket type; `Handler::new` is crate-private). Its handler also ignored `Poll::Ready(None)` from the connection, so a dead browser left callers hanging until `request_timeout`. rustplay vendors the crate with a small upstreamable patch (`docs/upstream/`). The TCP path uses the same patched crate, so both transports share one code path above the transport.

## Resource limits

`limits::CgroupHost` reads `/proc/self/mountinfo` and `/proc/self/cgroup` to find this process's cgroup v2 directory and v1 `memory` / `cpuacct` directories. Per controller: memory prefers v2 (`memory` in `cgroup.controllers`, enabled in the parent's `subtree_control`), else v1; CPU prefers v2 `cpu.stat` (present without the cpu controller), else v1 `cpuacct.usage`. Hybrid hosts can therefore mix them (this dev VM: CPU via v2, memory via v1; `rustplay doctor` reports it). cgroup v2 refuses `+memory` in a cgroup that holds processes (EBUSY, the no-internal-processes rule). If rustplay is the *only* process in its cgroup (started via `systemd-run --user --scope -p Delegate=yes`), `limits::enable_controller` moves it into a `rustplay-supervisor` leaf and retries; discovery maps that leaf back to its parent so session cgroups become its siblings. If other processes share the cgroup (a shell, `cargo`), it refuses and names them; it never moves processes it does not own. The child writes its pid into each `cgroup.procs` before exec (pre-opened fds, async-signal-safe). A `std::thread` watchdog polls every 25 ms; on a breach it records the trip, writes `cgroup.kill` (v2) and SIGKILLs the group, and notifies the runner over a `tokio::sync::watch` channel so even a step that never touches the browser (`sleep`) ends immediately. Cgroups are removed on close (reported as `CLEANUP_FAILED` if not) and from a helper thread on drop.

## Startup

Chromium starts with `--no-startup-window` and no positional URL: rustplay opens its one page over CDP, so no throwaway first tab (and renderer) is created. `--disable-field-trial-config` and `--no-service-autorun` skip experiment setup and background services. Together these took full-Chromium launch (spawn to first page ready) from about 410 ms to about 335 ms on the benchmark machine, measured as task total minus step and close time over 6 runs ([BENCHMARKS.md](BENCHMARKS.md)). None of them weaken the sandbox.

## Why rustplay spawns Chromium itself

chromiumoxide's launcher would own the process. Spawning it here gives:
- `env_clear()` and an explicit minimal env.
- `process_group(0)` so one `kill(-pgid)` reaches every helper process.
- `PR_SET_PDEATHSIG(SIGKILL)` so the browser dies even if rustplay is SIGKILLed.
- A bwrap prefix without forking the library.
- CDP pipe fds and cgroup membership set up between fork and exec.

## Actionability (auto-wait)

`click`/`type` run a JS probe every 100 ms until it reports `ok` or the step timeout ends. The probe resolves the selector, checks connected, visible (`checkVisibility` with opacity + size), enabled, editable (for `type`), scrolls to centre, then hit-tests the centre with `elementFromPoint`. On success it stores the element under `Symbol.for('rustplay.target')`; just before dispatching input, rustplay re-checks `isConnected` to report `ELEMENT_DETACHED` instead of clicking whatever re-rendered there. On timeout, the last probe state becomes the typed error, so "why" is always specific.

Selectors are embedded as JSON string literals (`serde_json::to_string`), so they can never break out into script.

## Timeouts

| Budget | Default | Scope |
|---|---|---|
| launch | 20 s | spawn to first page |
| navigation | 30 s | each `goto` until `load` |
| action | 10 s | each step / CDP call; step `timeout_ms` can only lower it |
| task | 120 s | whole task incl. launch; enforced with `tokio::time::timeout` |

## Concurrency

Each task gets its own process and profile, so sessions share nothing (verified by the localStorage isolation test). `run -j N`, `test -j N` and `serve --max-sessions N` bound concurrency. Memory, not CPU, is the limit. Measured on Chrome 153 / Linux with the fixture page: one session is 11 processes and about 366 MB PSS; rustplay itself is about 3 MB RSS. Real sites cost more. Size `--max-sessions` from your own `doctor` + PSS measurements.

## Cargo features

- `server` (default): axum HTTP API. `--no-default-features` builds the CLI only.
