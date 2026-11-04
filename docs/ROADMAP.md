# Roadmap

Ordered by security value first, then capability.

## Done

- **CDP over a pipe** (v0.2): default transport, no TCP port; TCP kept as deprecated fallback. Needs the vendored chromiumoxide patch until upstream merges it (`docs/upstream/`).
- **Per-session resource limits** (v0.2): wall watchdog, CPU and memory via cgroups v2 (v1 fallback on hybrid hosts), fail closed.

## Next

1. **Upstream the chromiumoxide patch** (handler EOF fix first, then `CdpTransport`), then drop `vendor/`.
2. **Network interception** (`Fetch.enable` with patterns): enforce the host allowlist on IP-literal subresources, block by resource type (media, fonts), cap response sizes, record a HAR-lite log per task for layer-08 traces.
3. **Remove `--cdp-transport tcp`** one release after the pipe default ships, unless a platform needs it.
4. **MCP server mode** (`rustplay mcp`): expose `run_task` and the schema as MCP tools over stdio, with the same RBAC and guard.
5. **Browser-context isolation**: one long-lived browser, one `Target.createBrowserContext` per session. Cuts per-session cost from a full process tree (~366 MB PSS measured) to a renderer, and launch from ~500 ms to tens of ms. Trade-off: weaker isolation than separate processes (shared browser process, shared network service), so keep process-per-session as the default and make contexts opt-in for trusted multi-tenant pools.

## Later

6. **Proxy support**: `--proxy` per operator and an allowlisted per-task proxy choice; proxy credentials via `Fetch.authRequired`, never in task files or argv.
7. **PDF generation**: `pdf` step via `Page.printToPDF`, confined like screenshots (`write_files` capability).
8. **More limits**: `pids.max` (fork bombs), a size-capped tmpfs for the session dir, `cpu.max` throttling as an alternative to kill-on-budget, automatic `systemd-run --user --scope` when no delegated cgroup exists.
9. **Landlock** for the rustplay process itself (read-only except out-dir and temp root), and nsjail as a third `--isolation` mode.
10. **Distributed workers**: stateless `serve` workers behind a queue (Redis streams or NATS); the API becomes enqueue + poll/webhook, results to object storage with signed URLs. Per-tenant quotas replace the single semaphore. Each worker keeps the same sandbox; nothing about isolation changes when scaling out.
11. **More steps**: `select` (dropdown), `upload` (from an operator-approved dir only), `hover`, `frame` scoping for iframes, `wait_for_network_idle`.
12. **Session reuse in `run`**: opt-in `--reuse-session` for long suites, with a context reset between tasks.
13. **Structured traces**: OpenTelemetry exporter behind a feature flag, spans already exist via `tracing`.
