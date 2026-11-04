# Benchmarks: rustplay vs Playwright

Reproduce with:

```bash
cargo build --release
npm i -g playwright && npx playwright install chromium chromium-headless-shell
python3 bench/run.py --chrome /path/to/chrome --shell /path/to/chrome-headless-shell
python3 bench/report.py bench/results.json
```

## Method

- **Same machine, same browser binary, same page, same steps.** Both tools launch the exact same Chromium executable (once with full Chromium `--headless=new`, once with `chrome-headless-shell`, which is Playwright's default headless mode). The page is `tests/fixtures/index.html`, served on 127.0.0.1 by a Python static server. No network.
- **Task.** `goto`, type "Ferris" with per-key events (`type` in rustplay, `pressSequentially` in Playwright), click, read the result and assert it. Both wait for `load` on navigation.
- **Interleaved.** rustplay and Playwright runs alternate, so thermal or cache drift hits both equally. The first runs of each scenario are discarded as warm-up.
- **Timing excludes memory sampling.** Memory is measured in a separate pass, because walking `/proc` every 20 ms costs real CPU on small machines.
- **What "per task" means.** Launch browser, open page, run the steps, close browser. rustplay's figure is its own `duration_ms`; Playwright's is measured with `performance.now()` around the same span.
- **One-shot CLI** includes starting the process itself (`rustplay open` vs `node script.mjs`), which is what a shell, CI step or agent tool call pays.
- **Root.** The benchmark machine runs as root, so both tools run Chromium with `--no-sandbox`. As a normal user, both would use the Chromium sandbox.

## Results

Machine: 2 vCPU, Linux 6.18.44-fc-v233, Google Chrome for Testing 153.0.8010.12, Playwright 1.63.0, rustplay 0.2.1, root with --no-sandbox for both.

### Full Chromium (`--headless=new`)

| Scenario | rustplay | Playwright | Result |
|---|---|---|---|
| One-shot CLI: start, launch, open page, read `h1`, exit (median, p95) | 550 ms, 621 | 826 ms, 946 | rustplay 1.50x faster |
| Form task, fresh browser per task, sequential (per task median, p95) | 580 ms, 670 | 655 ms, 744 | rustplay 1.13x faster |
| 20 form tasks, 4 at a time, fresh browser each (wall, median) | 10.94 s | 12.58 s | rustplay 1.15x faster |
| Driver process peak RSS | 14 MB | 156 MB | 11x less for rustplay |
| Whole process tree peak PSS (driver + browser) | 350 MB | 486 MB | 28% less for rustplay |
| Form task, **one shared browser, new context per task** (per task median) | n/a (not supported yet) | 358 ms | Playwright 1.6x faster than rustplay's fresh-process mode |

### chrome-headless-shell (Playwright's default headless)

| Scenario | rustplay | Playwright | Result |
|---|---|---|---|
| One-shot CLI: start, launch, open page, read `h1`, exit (median, p95) | 191 ms, 216 | 478 ms, 483 | rustplay 2.50x faster |
| Form task, fresh browser per task, sequential (per task median, p95) | 211 ms, 242 | 255 ms, 297 | rustplay 1.21x faster |
| 20 form tasks, 4 at a time, fresh browser each (wall, median) | 2.95 s | 3.82 s | rustplay 1.30x faster |
| Driver process peak RSS | 14 MB | 157 MB | 11x less for rustplay |
| Whole process tree peak PSS (driver + browser) | 184 MB | 324 MB | 43% less for rustplay |
| Form task, **one shared browser, new context per task** (per task median) | n/a (not supported yet) | 152 ms | Playwright 1.4x faster than rustplay's fresh-process mode |

## Reading the numbers

- **Where rustplay wins:** process start (no Node.js runtime), a much smaller driver process, and a lighter launch, because it skips the throwaway startup tab and experiment setup. That matters most for one-shot calls, which is how agents and CI usually invoke a browser.
- **Where Playwright wins:** reusing one browser and giving each task a fresh *context* is much cheaper than starting a browser per task. rustplay doesn't do that yet, on purpose: a context shares the browser process and network service, which is weaker isolation for untrusted tasks. Opt-in context pooling is on the [roadmap](ROADMAP.md).
- **The browser dominates.** Choosing `chrome-headless-shell` over full Chrome matters more than choosing the driver: per task it is about 2.6x faster for both tools (rustplay 580 → 211 ms, Playwright 655 → 255 ms), and it uses 33-47% less memory.
- **Small machine.** 2 vCPU means the parallel scenario is CPU-bound for both. Expect different ratios on bigger hosts. Run the harness on your own hardware before relying on these figures.

## History

- v0.2.1 before the startup-flag change (`about:blank` startup tab, no `--no-startup-window`): full Chromium, form task, fresh browser per task: rustplay 817 ms vs Playwright 745 ms per task, so Playwright was 1.10x faster. Comparing flags with Playwright's led to the fix.
