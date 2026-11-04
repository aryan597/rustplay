#!/usr/bin/env python3
"""Render bench/results.json as the markdown tables used in docs/BENCHMARKS.md."""
import json, sys
from pathlib import Path

r = json.loads(Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).with_name("results.json")).read_text())
S = r["scenarios"]


def ratio(rp, pw):
    if rp <= pw:
        return f"rustplay {pw / rp:.2f}x faster"
    return f"Playwright {rp / pw:.2f}x faster"


def mb(kb):
    return f"{kb / 1024:.0f} MB"


m = r["machine"]
print(f"Machine: {m.get('cpus')} vCPU, Linux {m.get('kernel')}, {m.get('chrome')}, "
      f"Playwright {m.get('playwright')}, {m.get('rustplay')}"
      f"{', root with --no-sandbox for both' if m.get('root_no_sandbox') else ''}.\n")

for b in ["chrome", "headless-shell"]:
    if f"{b}/cold_open_cli" not in S:
        continue
    title = "Full Chromium (`--headless=new`)" if b == "chrome" else "chrome-headless-shell (Playwright's default headless)"
    print(f"### {title}\n")
    print("| Scenario | rustplay | Playwright | Result |")
    print("|---|---|---|---|")
    c = S[f"{b}/cold_open_cli"]
    print(f"| One-shot CLI: start, launch, open page, read `h1`, exit (median, p95) | "
          f"{c['rustplay']['median']:.0f} ms, {c['rustplay']['p95']:.0f} | "
          f"{c['playwright']['median']:.0f} ms, {c['playwright']['p95']:.0f} | "
          f"{ratio(c['rustplay']['median'], c['playwright']['median'])} |")
    f = S[f"{b}/form_fresh_browser"]
    print(f"| Form task, fresh browser per task, sequential (per task median, p95) | "
          f"{f['rustplay']['per_task']['median']:.0f} ms, {f['rustplay']['per_task']['p95']:.0f} | "
          f"{f['playwright']['per_task']['median']:.0f} ms, {f['playwright']['per_task']['p95']:.0f} | "
          f"{ratio(f['rustplay']['per_task']['median'], f['playwright']['per_task']['median'])} |")
    pk = next(k for k in S if k.startswith(f"{b}/form_parallel_"))
    p = S[pk]
    width = pk.rsplit("_", 1)[1]
    print(f"| {p['tasks_per_run']} form tasks, {width} at a time, fresh browser each (wall, median) | "
          f"{p['rustplay']['run_total']['median'] / 1000:.2f} s | {p['playwright']['run_total']['median'] / 1000:.2f} s | "
          f"{ratio(p['rustplay']['run_total']['median'], p['playwright']['run_total']['median'])} |")
    mem = f["memory_kb"]
    print(f"| Driver process peak RSS | {mb(mem['rustplay_driver_peak_rss'])} | {mb(mem['playwright_driver_peak_rss'])} | "
          f"{mem['playwright_driver_peak_rss'] / mem['rustplay_driver_peak_rss']:.0f}x less for rustplay |")
    print(f"| Whole process tree peak PSS (driver + browser) | {mb(mem['rustplay_tree_peak_pss'])} | "
          f"{mb(mem['playwright_tree_peak_pss'])} | "
          f"{(1 - mem['rustplay_tree_peak_pss'] / mem['playwright_tree_peak_pss']) * 100:.0f}% less for rustplay |")
    ctx = S[f"{b}/form_shared_browser_new_context"]["playwright"]["per_task"]
    print(f"| Form task, **one shared browser, new context per task** (per task median) | n/a (not supported yet) | "
          f"{ctx['median']:.0f} ms | Playwright {f['rustplay']['per_task']['median'] / ctx['median']:.1f}x faster than rustplay's fresh-process mode |")
    print()
