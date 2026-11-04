# Contributing

Thanks for helping. Start with [AGENTS.md](AGENTS.md): repo map, the invariants that must not break, and the checklist for adding a step.

## Before opening a PR

```bash
scripts/gate.sh   # fmt, clippy -D warnings, unit, docs drift, integration, evals, transport, limits, example suite
```

The gate needs Chrome or Chromium; browser tests and evals are not optional. On a machine without one, set `RUSTPLAY_CHROME=/path/to/chrome`.

## Ground rules

- Security defaults only get stricter. A change that loosens one (opens a port, reuses a profile, lets a task widen operator policy) needs an issue first.
- New error codes go in `RpError` with `code()` and `hint()`, and in `docs/ERRORS.md` (the drift test enforces this).
- Tests never touch the internet; serve `tests/fixtures` locally.
- Changes to `vendor/chromiumoxide` must be regenerated into `docs/upstream/chromiumoxide-cdp-transport.patch`.

## Reporting security issues

See [SECURITY.md](SECURITY.md). Please do not open public issues for sandbox escapes.

## License

By contributing you agree your work is dual-licensed under MIT and Apache-2.0, as the project is.
