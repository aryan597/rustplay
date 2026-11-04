# Changelog

All notable changes. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning: [SemVer](https://semver.org/); before 1.0, minor versions may break the CLI or JSON output.

## [0.2.2] - 2026-11-04

### Fixed
- CI on Ubuntu 24.04 installed the snap build of Chromium (`apt install chromium-browser`), which broke the downloads, bwrap and environment evals. CI now uses Chrome for Testing via `browser-actions/setup-chrome`.
- The root cgroup v2 CI step piped `cargo test` into `tee`, which hid test failures. Its exit status is now checked.
- macOS: two cgroup unit tests ran there and failed by design. They are Linux-only now, with a macOS test asserting limits fail closed.

### Changed
- **Snap-packaged Chromium is refused** with `LAUNCH_FAILED` and an install hint. snapd gives it a private `/tmp` and a rebuilt environment, so rustplay's profile cleanup, env scrubbing and download confinement can't be guaranteed. Autodetection skips snap builds (including Ubuntu's `/usr/bin/chromium-browser` shim), and an explicit `--chrome` / `$RUSTPLAY_CHROME` pointing at one is an error.
- Browser lookup now prefers `google-chrome` over `chromium`. A `$RUSTPLAY_CHROME` that doesn't exist is now an error instead of being silently ignored.

## [0.2.1] - 2026-11-04

### Fixed
- Memory limits on systemd desktops (Ubuntu): cgroup v2 refuses to enable controllers in a cgroup that holds processes (EBUSY). When rustplay is the only process in its cgroup it now moves itself into a `rustplay-supervisor` leaf and retries; otherwise the error names the other processes. Never moves processes it does not own.
- `rustplay doctor` no longer fails on `apparmor_restrict_unprivileged_userns=1` when Chrome launches fine. Checks now carry `"required"`; only browser, Chromium sandbox, launch and CDP exposure decide the exit code. bwrap is probed for real.
- Corrected the `LIMIT_UNSUPPORTED` hint (`systemd-run --user --scope -p Delegate=yes` with the binary, not `cargo run`).

### Changed
- Faster launch: Chromium now starts with `--no-startup-window` (no throwaway first tab), `--disable-field-trial-config`, `--no-service-autorun`, `--disable-back-forward-cache`. Full-Chromium per-task time (fresh browser, form task) dropped from 817 ms to 580 ms on the benchmark machine.

### Added
- `bench/`: reproducible benchmark against Playwright (same Chromium, same local pages, same steps), results in docs/BENCHMARKS.md.
- LICENSE-MIT, LICENSE-APACHE, NOTICE (vendored chromiumoxide), CONTRIBUTING.md, SECURITY.md, issue template, tag-triggered release workflow (Linux x86_64 binary + sha256).

## [0.2.0] - 2026-11-04

### Added
- CDP over `--remote-debugging-pipe` (fds 3/4) as the default transport. No TCP port is opened; every launch scans the browser's process tree and fails with `CDP_PORT_EXPOSED` if anything listens.
- `--cdp-transport pipe|tcp`; `tcp` is a deprecated fallback and adds a `TCP_TRANSPORT_DEPRECATED` warning to every report.
- Per-session limits: `--wall-limit` (OS watchdog), `--cpu-limit` and `--memory-limit` (cgroup v2, v1 fallback), `--cgroup-parent`. Task options can only lower them. Unenforceable limits fail with `LIMIT_UNSUPPORTED`.
- Reports include `transport` and `resources` (limits, enforcement mechanism, measured CPU and peak memory).
- Error codes `WALL_LIMIT_EXCEEDED`, `CPU_LIMIT_EXCEEDED`, `MEMORY_LIMIT_EXCEEDED`, `LIMIT_UNSUPPORTED`, `CDP_PORT_EXPOSED`, `TRANSPORT_UNSUPPORTED`.
- Mandatory per-session `--user-data-dir` check before every launch.
- `scripts/setup-wsl.sh`.

### Changed
- Vendored chromiumoxide 0.9.1 with a small patch: pluggable CDP transport, and the handler now ends when the connection closes (a dead browser is reported in under a second instead of after a 20-30 s timeout).
- MSRV 1.87.

## [0.1.0] - 2026-11-03

### Added
- CLI (`open`, `run`, `test`, `serve`, `doctor`, `schema`), JSON task format with 17 actions including assertions, typed error codes with hints.
- Sandbox: fresh 0700 profile per session, scrubbed environment, hardened Chromium flags, URL and host policy (enforced in Chromium's resolver), downloads denied by default, output-path confinement, process-group cleanup on every exit path, optional bubblewrap.
- Roles (viewer / operator / admin), prompt-injection flags and PII redaction on page-derived output.
- Local HTTP API with bearer tokens and 429 burst control.

[0.2.2]: https://github.com/aryan597/rustplay/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/aryan597/rustplay/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/aryan597/rustplay/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/aryan597/rustplay/releases/tag/v0.1.0
