#!/usr/bin/env python3
"""rustplay vs Playwright benchmark.

Same machine, same Chromium binary, same local fixture page, same steps.
Runs are interleaved (rustplay, playwright, rustplay, ...) to cancel drift,
and the first WARMUP runs of each are discarded.

Usage:
  python3 bench/run.py --rustplay target/release/rustplay \
      --chrome /path/to/chrome [--shell /path/to/chrome-headless-shell] \
      [--reps 15] [--tasks 10] [--out bench/results.json]
"""
import argparse, json, os, statistics, subprocess, sys, tempfile, threading, time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
WARMUP = 2


def serve_fixtures():
    class Quiet(SimpleHTTPRequestHandler):
        def __init__(self, *a, **k):
            super().__init__(*a, directory=str(ROOT / "tests" / "fixtures"), **k)

        def log_message(self, *a):
            pass

    srv = ThreadingHTTPServer(("127.0.0.1", 0), Quiet)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, f"http://127.0.0.1:{srv.server_address[1]}"


def tree_pss_kb(root):
    """Sum PSS of `root` and all its descendants (Linux)."""
    kids = {}
    for p in os.listdir("/proc"):
        if not p.isdigit():
            continue
        try:
            with open(f"/proc/{p}/stat") as f:
                ppid = int(f.read().rsplit(")", 1)[1].split()[1])
            kids.setdefault(ppid, []).append(int(p))
        except (OSError, ValueError, IndexError):
            pass
    total, stack = 0, [root]
    while stack:
        pid = stack.pop()
        stack.extend(kids.get(pid, []))
        try:
            with open(f"/proc/{pid}/smaps_rollup") as f:
                for line in f:
                    if line.startswith("Pss:"):
                        total += int(line.split()[1])
                        break
        except OSError:
            pass
    return total


def run(cmd, env=None, sample_mem=False):
    """Run cmd; return (wall_ms, stdout, peak_driver_rss_kb, peak_tree_pss_kb)."""
    t0 = time.perf_counter()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
    peak_rss = peak_pss = 0
    if sample_mem:
        while p.poll() is None:
            try:
                with open(f"/proc/{p.pid}/status") as f:
                    for line in f:
                        if line.startswith("VmHWM:"):
                            peak_rss = max(peak_rss, int(line.split()[1]))
            except OSError:
                pass
            peak_pss = max(peak_pss, tree_pss_kb(p.pid))
            time.sleep(0.02)
    out, _ = p.communicate()
    wall = (time.perf_counter() - t0) * 1000
    if p.returncode != 0:
        sys.exit(f"command failed ({p.returncode}): {' '.join(cmd)}\n{out.decode()[:2000]}")
    return wall, out.decode(), peak_rss, peak_pss


def stats(xs):
    xs = sorted(xs)
    return {
        "n": len(xs),
        "median": round(statistics.median(xs), 1),
        "p95": round(xs[min(len(xs) - 1, int(round(0.95 * (len(xs) - 1))))], 1),
        "min": round(xs[0], 1),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rustplay", default=str(ROOT / "target" / "release" / "rustplay"))
    ap.add_argument("--chrome", required=True, help="full Chromium/Chrome binary used by both")
    ap.add_argument("--shell", help="chrome-headless-shell binary (Playwright's default headless)")
    ap.add_argument("--reps", type=int, default=15)
    ap.add_argument("--tasks", type=int, default=10)
    ap.add_argument("--parallel", type=int, default=4)
    ap.add_argument("--out", default=str(HERE / "results.json"))
    a = ap.parse_args()

    srv, base = serve_fixtures()
    tmp = Path(tempfile.mkdtemp(prefix="rpbench-"))
    task = tmp / "form.json"
    task.write_text(json.dumps({"steps": [
        {"action": "goto", "url": f"{base}/"},
        {"action": "type", "selector": "#name", "text": "Ferris"},
        {"action": "click", "selector": "#submit"},
        {"action": "assert_text", "selector": "#out", "equals": "Hello, Ferris"},
    ]}))
    root = os.geteuid() == 0
    rp_common = ["--allow-host", "127.0.0.1", "--out-dir", str(tmp)] + (["--no-chrome-sandbox"] if root else [])
    pw = ["node", str(HERE / "playwright_bench.mjs")]

    browsers = {"chrome": a.chrome}
    if a.shell:
        browsers["headless-shell"] = a.shell

    results = {"machine": {}, "scenarios": {}}
    try:
        results["machine"] = {
            "cpus": os.cpu_count(),
            "kernel": os.uname().release,
            "chrome": subprocess.run([a.chrome, "--version"], capture_output=True, text=True).stdout.strip(),
            "playwright": subprocess.run(["node", "-e", "console.log(require('playwright/package.json').version)"],
                                         capture_output=True, text=True).stdout.strip(),
            "rustplay": subprocess.run([a.rustplay, "--version"], capture_output=True, text=True).stdout.strip(),
            "root_no_sandbox": root,
        }
    except OSError as e:
        print("warn: version probe failed:", e)

    for bname, bpath in browsers.items():
        rp = [a.rustplay, "--chrome", bpath] + rp_common
        env = dict(os.environ, CHROME=a.chrome)
        if bname == "headless-shell":
            env["PW_SHELL"] = "1"
        print(f"\n### browser: {bname}", flush=True)

        # 1. Cold one-shot CLI: process start -> launch -> goto -> text -> close -> exit.
        rp_w, pw_w = [], []
        for i in range(a.reps + WARMUP):
            w1, *_ = run(rp + ["open", f"{base}/", "--text", "h1"])
            w2, *_ = run(pw + ["open", base, "1"], env=env)
            if i >= WARMUP:
                rp_w.append(w1)
                pw_w.append(w2)
        results["scenarios"][f"{bname}/cold_open_cli"] = {"rustplay": stats(rp_w), "playwright": stats(pw_w)}
        print("cold_open_cli", results["scenarios"][f"{bname}/cold_open_cli"], flush=True)

        # 2. N form tasks, sequential, fresh browser each, one driver process.
        rp_per, pw_per, rp_tot, pw_tot, mem = [], [], [], [], {}
        # Timing runs are not memory-sampled: walking /proc every 20 ms costs
        # real CPU on small machines and would skew both sides.
        for i in range(max(3, a.reps // 3) + 1):
            w1, out1, *_ = run(rp + ["run", "-j", "1"] + [str(task)] * a.tasks)
            w2, out2, *_ = run(pw + ["form_fresh", base, str(a.tasks)], env=env)
            if i == 0:
                continue
            rp_per += [r["duration_ms"] for r in json.loads(out1)]
            pw_per += json.loads(out2)["per_task_ms"]
            rp_tot.append(w1)
            pw_tot.append(w2)
        # Separate memory pass.
        for _ in range(2):
            _, _, rss1, pss1 = run(rp + ["run", "-j", "1"] + [str(task)] * a.tasks, sample_mem=True)
            _, _, rss2, pss2 = run(pw + ["form_fresh", base, str(a.tasks)], env=env, sample_mem=True)
            mem.setdefault("rp_rss", []).append(rss1)
            mem.setdefault("pw_rss", []).append(rss2)
            mem.setdefault("rp_pss", []).append(pss1)
            mem.setdefault("pw_pss", []).append(pss2)
        results["scenarios"][f"{bname}/form_fresh_browser"] = {
            "tasks_per_run": a.tasks,
            "rustplay": {"per_task": stats(rp_per), "run_total": stats(rp_tot)},
            "playwright": {"per_task": stats(pw_per), "run_total": stats(pw_tot)},
            "memory_kb": {
                "rustplay_driver_peak_rss": max(mem["rp_rss"]),
                "playwright_driver_peak_rss": max(mem["pw_rss"]),
                "rustplay_tree_peak_pss": max(mem["rp_pss"]),
                "playwright_tree_peak_pss": max(mem["pw_pss"]),
            },
        }
        print("form_fresh_browser", results["scenarios"][f"{bname}/form_fresh_browser"], flush=True)

        # 3. Parallel: 2*N tasks, `parallel` at a time, fresh browser each.
        n = a.tasks * 2
        rp_tot, pw_tot = [], []
        for i in range(max(3, a.reps // 3) + 1):
            w1, *_ = run(rp + ["run", "-j", str(a.parallel)] + [str(task)] * n)
            w2, *_ = run(pw + ["form_parallel", base, str(n), str(a.parallel)], env=env)
            if i:
                rp_tot.append(w1)
                pw_tot.append(w2)
        results["scenarios"][f"{bname}/form_parallel_{a.parallel}"] = {
            "tasks_per_run": n,
            "rustplay": {"run_total": stats(rp_tot)},
            "playwright": {"run_total": stats(pw_tot)},
        }
        print(f"form_parallel_{a.parallel}", results["scenarios"][f"{bname}/form_parallel_{a.parallel}"], flush=True)

        # 4. Playwright's idiomatic reuse: one browser, new context per task (no rustplay equivalent yet).
        pw_per = []
        for i in range(max(3, a.reps // 3) + 1):
            _, out, *_ = run(pw + ["form_context", base, str(a.tasks)], env=env)
            if i:
                pw_per += json.loads(out)["per_task_ms"]
        results["scenarios"][f"{bname}/form_shared_browser_new_context"] = {"playwright": {"per_task": stats(pw_per)}}
        print("form_shared_browser_new_context", results["scenarios"][f"{bname}/form_shared_browser_new_context"], flush=True)

    srv.shutdown()
    Path(a.out).write_text(json.dumps(results, indent=2) + "\n")
    print(f"\nwrote {a.out}")


if __name__ == "__main__":
    main()
