# Error codes

Every error in JSON output has this shape:

```json
{ "code": "ELEMENT_OBSCURED", "message": "cannot act on `#buy`: element is covered by div#cookie-banner",
  "hint": "close the overlay (modal, cookie banner) first, or scroll the element clear", "retryable": false }
```

Codes are a stable API. `retryable: true` means an identical retry can succeed without changing the task.

| Code | Meaning | Retryable | Exit | Typical fix |
|---|---|---|---|---|
| `LAUNCH_FAILED` | Chromium did not start, exited early, or never answered the first CDP command; last stderr lines and exit status in message | no | 3 | `rustplay doctor`; install Chromium; sandbox flags |
| `SANDBOX_UNAVAILABLE` | `--isolation bwrap` requested but unusable | no | 3 | install bubblewrap or use `--isolation none` |
| `TRANSPORT_UNSUPPORTED` | `--cdp-transport pipe` on a platform without inheritable fds (non-unix) | no | 3 | `--cdp-transport tcp` (deprecated) |
| `CDP_PORT_EXPOSED` | in pipe mode, the browser tree was found listening on TCP right after launch; the session is killed | no | 3 | should never happen; look for a wrapper adding `--remote-debugging-port`; report a bug |
| `LIMIT_UNSUPPORTED` | a requested `--cpu-limit` / `--memory-limit` cannot be enforced here (non-Linux, no usable cgroup v2/v1 controller, cgroup not delegated); reason in message. Never silently ignored | no | 3 | `systemd-run --user --scope -p Delegate=yes rustplay ...`, `--cgroup-parent`, or drop the limit |
| `WALL_LIMIT_EXCEEDED` | the OS watchdog killed the browser at `--wall-limit` | no | 1 | raise the limit or split the task |
| `CPU_LIMIT_EXCEEDED` | the browser process tree used `--cpu-limit` ms of CPU (cgroup accounting, includes exited helpers) | no | 1 | heavy page or busy JS; raise the limit |
| `MEMORY_LIMIT_EXCEEDED` | the kernel OOM-killed inside the session cgroup at `--memory-limit`; peak in message | no | 1 | raise the limit or avoid heavy pages |
| `NAV_TIMEOUT` | page did not fire `load` within `--nav-timeout` | yes | 1 | raise timeout, check server |
| `NAV_NETWORK` | DNS / connection / TLS / file error (`net::ERR_*` in message) | yes | 1 | see hint; host may be blocked by `--allow-host` |
| `SELECTOR_NOT_FOUND` | nothing matched before the step timeout | no | 1 | fix selector, add `wait_for` |
| `SELECTOR_INVALID` | not valid CSS | no | 1 | fix selector syntax |
| `ELEMENT_HIDDEN` | display:none, visibility:hidden or opacity 0 | no | 1 | `wait_for` visible |
| `ELEMENT_DISABLED` | `disabled`, inside disabled fieldset, or `aria-disabled=true` | no | 1 | wait until enabled |
| `ELEMENT_DETACHED` | element left the DOM between probe and action | yes | 1 | retry |
| `ELEMENT_ZERO_SIZE` | element has no box | no | 1 | target a visible element |
| `ELEMENT_NOT_EDITABLE` | `type` target is not an editable input/textarea/contenteditable, or readonly | no | 1 | target the input |
| `ELEMENT_OBSCURED` | another element receives the click at the centre point (`by` in message) | no | 1 | dismiss the overlay |
| `JS_EVAL_FAILED` | exception in `eval` or a probe; first line + position in message | no | 1 | fix script |
| `BROWSER_DISCONNECTED` | Chromium exited or the CDP pipe/socket closed (detected on EOF, not by timeout) | yes | 1 | rerun; lower concurrency (OOM) |
| `ACTION_TIMEOUT` | a CDP call or wait exceeded `--action-timeout` / step `timeout_ms` | yes | 1 | raise timeout |
| `TASK_TIMEOUT` | total task budget `--task-timeout` exceeded | no | 1 | raise budget or split task |
| `ASSERTION_FAILED` | an `assert_*` step did not match; expected and actual in message | no | 1 | the page changed, or the test is wrong |
| `POLICY_VIOLATION` | blocked by sandbox policy (scheme, host, output path, loosening attempt) | no | 2 | use an operator flag if intended |
| `FORBIDDEN` | the role lacks the capability for a step | no | 2 | run with a higher `--role` / token |
| `CONFIG_INVALID` | bad task JSON, flag value, or unsafe setup (e.g. root without `--no-chrome-sandbox`) | no | 2 | message says what |
| `CLEANUP_FAILED` | profile dir could not be removed (reported as a warning) | no | - | delete `rustplay-*` temp dirs |
| `IO_ERROR` | local file error (task file, output) | no | 1 | message has the path |
| `CANCELLED` | Ctrl-C; browsers were killed and profiles removed | no | 130 | - |

Warnings (in the report's `warnings` array; the task still runs):

| Code | Meaning |
|---|---|
| `TCP_TRANSPORT_DEPRECATED` | the session used `--cdp-transport tcp`; DevTools was reachable on a loopback port for its lifetime |
| `CLEANUP_FAILED` | see above |

HTTP API only:

| Code | HTTP | Meaning |
|---|---|---|
| `UNAUTHORIZED` | 401 | missing or wrong bearer token |
| `BUSY` | 429 | all `--max-sessions` slots in use; retry with backoff |
| `INTERNAL` | 500 | runner panicked; its browser was still killed |

`FORBIDDEN` maps to 403 and request-level `CONFIG_INVALID` / `POLICY_VIOLATION` to 400. Step failures return 200 with `ok: false`, because the request itself was valid.
