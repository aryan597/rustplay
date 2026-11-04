# Security policy

rustplay runs untrusted automation, so sandbox weaknesses matter. The threat model, every defence layer and the known limits are in [docs/SECURITY.md](docs/SECURITY.md).

## Supported versions

Only the latest release gets fixes before 1.0.

## Reporting a vulnerability

Use GitHub's private reporting: **Security > Report a vulnerability** on this repository. Please include the rustplay version, OS, Chrome version, the flags you used and a task file that reproduces it. Expect an acknowledgement within 7 days.

In scope: escaping the session directory, reading host environment or files, a CDP port exposed in pipe mode, a task loosening operator policy, processes or profiles surviving cleanup, limits not enforced when the report says they are.

Out of scope: anything that needs `--cdp-transport tcp`, `--role admin`, `--allow-file-urls` or `--no-chrome-sandbox` without `--isolation bwrap`, since those are documented trade-offs. Chromium bugs belong upstream.
