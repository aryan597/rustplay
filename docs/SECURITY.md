# Security model

## Threat model

The **task** is untrusted (written by a user, an LLM agent, or a remote API caller). The **pages** it visits are untrusted. The **operator** (whoever starts `rustplay` and sets flags) is trusted. Goals:

1. A task cannot read or write host files outside the output dir.
2. A task cannot see host environment variables, credentials or tokens.
3. A task cannot loosen the operator's policy.
4. A task cannot leave processes or profiles behind.
5. Page content reaching an agent is marked untrusted and screened.
6. No other local process can attach to a rustplay browser.
7. A task cannot exhaust the host's CPU or memory (when limits are set).

## Layers

| Layer | Default | Mechanism | Evidence |
|---|---|---|---|
| CDP over pipe | on (`--cdp-transport pipe`) | `--remote-debugging-pipe`, fds 3/4 installed in the child after re-duplication to fds >= 10 (avoids the dup2 swap bug); no socket exists; launch-time `/proc` scan of the whole tree fails closed with `CDP_PORT_EXPOSED` | `pipe_session_opens_no_tcp_port`, `l06_no_cdp_port_exposed_by_browser_tree` (with a concurrent TCP positive control) |
| Fresh profile per session | on, mandatory | temp dir 0700: profile, cache, HOME, TMPDIR; deleted on close/drop. `sandbox::ensure_session_profile` refuses any launch without exactly one `--user-data-dir` inside the session root | `close_kills_process_and_removes_profile`, `drop_without_close_still_kills` |
| Env scrubbing | on | `env_clear()` + minimal PATH/HOME/TMPDIR/LANG/TZ | `l06_browser_env_has_no_host_secrets` reads `/proc/<pid>/environ` |
| Hardened Chromium flags | on | no extensions, sync, translate, background networking, component updates, crash reporting, pings, notifications, permission prompts, new windows | `hardened_flags_present` |
| Chromium sandbox | on | refuses to start as root unless `--no-chrome-sandbox` is explicit | `doctor`, `BrowserProcess::launch` |
| URL policy | on | http/https only; `file:` needs `--allow-file-urls`; no `chrome:`, `javascript:`, `data:`; no credentials in URLs | `url_policy` |
| Host allowlist | opt-in `--allow-host` | checked at navigation **and** enforced in Chromium's resolver (`--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE ...`) for every subresource, fetch and redirect | `l06_host_allowlist_applies_to_subresources` |
| Downloads | denied | `Browser.setDownloadBehavior(deny)`; with `--downloads-dir D` they land only in `D/<session-id>/` | `l06_downloads_denied_by_default_and_confined_when_enabled` |
| Output confinement | on | relative only, no `..`, symlink-escape check, extension allowlist, O_EXCL | `output_confinement` |
| Timeouts | on | launch, navigation, action, total task | integration tests |
| Resource limits | opt-in | wall: OS-thread watchdog; cpu: cgroup v2 `cpu.stat` / v1 `cpuacct`; memory: cgroup v2 `memory.max` + `oom.group` + `swap.max=0` / v1 `limit_in_bytes`; cgroup joined before exec; `LIMIT_UNSUPPORTED` instead of running unlimited | `tests/limits.rs` (real OOM kill, real CPU kill, wall kill, cleanup of cgroups) |
| Process cleanup | on | process group SIGKILL, PDEATHSIG, `kill_on_drop`, Drop on panic/cancel | `cancelled_task_cleans_up` |
| RBAC | operator by default | viewer / operator / admin; checked before launch and per step | `l05_*` |
| Output guard | flags always; redaction opt-in | injection pattern flags, Luhn-checked card / email / phone masking | `l06_*` |
| bubblewrap | opt-in `--isolation bwrap` | new user/pid/ipc/uts namespaces, read-only system dirs, tmpfs /tmp, only the session dir writable, host /home and /tmp invisible | `l06_bwrap_hides_host_filesystem` |
| API exposure | loopback | non-loopback bind refused without tokens; constant-time token compare; 1 MiB body limit | `l05_api_auth_and_roles` |

## Optional OS sandboxing: tradeoffs

| Option | Gives | Costs | Status |
|---|---|---|---|
| Chromium's own sandbox | strong renderer isolation (namespaces + seccomp-bpf) | needs unprivileged userns or setuid helper; not as root | default |
| bubblewrap | filesystem view, pid/ipc isolation around the whole browser | Linux only; nested userns may block Chromium's sandbox, so usually paired with `--no-chrome-sandbox` (bwrap becomes the outer boundary, renderers lose per-process seccomp) | implemented, opt-in |
| nsjail | bwrap plus cgroup memory/CPU limits and seccomp policy files | heavier, more config, not packaged everywhere | roadmap |
| Landlock | unprivileged filesystem allowlist from inside the process | kernel >= 5.13; Chromium needs many paths; no network control before ABI 4 | roadmap |
| seccomp (own filter) | syscall reduction | Chromium already filters renderers; a browser-wide filter is fragile | not planned |

Best practical setup on Linux: run as a non-root user with unprivileged user namespaces enabled (Chromium sandbox on), `--allow-host` set, `--role viewer` for read-only agents, and `--isolation bwrap` where host files matter.

## Known limits (be honest about these)

- **DevTools port (resolved by default).** Pipe mode opens none. The deprecated `--cdp-transport tcp` still listens on a random 127.0.0.1 port for the session's lifetime, reachable by local processes; the report carries `TCP_TRANSPORT_DEPRECATED`.
- **Pipe transport relies on a vendored chromiumoxide patch** (`vendor/chromiumoxide`, see `docs/upstream/`). Security fixes in upstream chromiumoxide must be pulled into the vendored copy until the patch lands upstream.
- **IP-literal subresources** bypass the resolver allowlist (a page can embed `http://203.0.113.5/x`). Navigation to IP literals is still checked. Fix: request interception (roadmap).
- **Injection flags are heuristics.** They catch common phrasing, not adversarial paraphrase. Treat `untrusted_content: true` as the real signal and keep an agent-side guard.
- **PII redaction is regex-based.** It over-redacts long digit runs as phone numbers and misses names and addresses.
- **`eval` is full page JS.** It cannot touch the host, but it can do anything the page can (including requests to allowed hosts). That is why it is admin-only.
- **macOS:** no PDEATHSIG and no bwrap. Cleanup relies on process-group kill and Drop; a SIGKILLed rustplay can orphan Chromium there.
- **Limits need Linux and a usable cgroup.** CPU and memory limits are Linux-only; non-root users need a delegated cgroup in which rustplay is the only process (`systemd-run --user --scope -p Delegate=yes`, where rustplay moves itself into a leaf) or an empty `--cgroup-parent`. Unsupported means a hard error, never a silent skip.
- **AppArmor userns restriction (Ubuntu 23.10+).** `apparmor_restrict_unprivileged_userns=1` does not affect the Google Chrome .deb (it ships its own AppArmor profile and setuid sandbox helper), but can block `--isolation bwrap`. `rustplay doctor` runs a real bwrap probe instead of guessing.
- **CPU limit granularity.** CPU is sampled every 25 ms, so a tree can overshoot by roughly 25 ms times the number of busy cores. Memory is enforced by the kernel, with no overshoot.
- **cgroup v1 OOM kills one process.** On v1 hosts the kernel kills the largest process (usually a renderer); the watchdog then kills the rest. v2 uses `memory.oom.group` to kill the tree atomically.
- **No disk / pid limits yet.** A page cannot write outside the session dir, but can fill it; `pids.max` and a tmpfs size cap are on the roadmap.
