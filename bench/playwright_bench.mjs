// Playwright side of the rustplay benchmark. Mirrors bench/run.py scenarios.
// Usage: node playwright_bench.mjs <scenario> <base_url> <n> [concurrency]
//   scenarios: open | form_fresh | form_parallel | form_context
// Env: CHROME (executable), PW_SHELL=1 to use Playwright's default headless shell.
// createRequire so a globally installed playwright (NODE_PATH) also resolves.
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const { chromium } = require("playwright");

const [scenario, base, nArg, concArg] = process.argv.slice(2);
const n = Number(nArg || 1);
const conc = Number(concArg || 1);

const launchOpts = () => {
  const o = { headless: true };
  if (!process.env.PW_SHELL && process.env.CHROME) {
    o.executablePath = process.env.CHROME;
    o.args = ["--headless=new"];
  }
  return o;
};

async function formOnPage(page) {
  await page.goto(`${base}/`);
  await page.locator("#name").pressSequentially("Ferris"); // per-key events, like rustplay `type`
  await page.locator("#submit").click();
  const t = await page.locator("#out").innerText();
  if (t.trim() !== "Hello, Ferris") throw new Error(`unexpected: ${t}`);
}

async function freshBrowserTask(fn) {
  const t0 = performance.now();
  const browser = await chromium.launch(launchOpts());
  try {
    const page = await browser.newPage();
    await fn(page);
  } finally {
    await browser.close();
  }
  return performance.now() - t0;
}

async function pool(count, width, job) {
  const out = [];
  let next = 0;
  await Promise.all(
    Array.from({ length: width }, async () => {
      while (next < count) {
        next++;
        out.push(await job());
      }
    }),
  );
  return out;
}

const t0 = performance.now();
let per = [];
if (scenario === "open") {
  per.push(
    await freshBrowserTask(async (page) => {
      await page.goto(`${base}/`);
      await page.locator("h1").innerText();
    }),
  );
} else if (scenario === "form_fresh") {
  for (let i = 0; i < n; i++) per.push(await freshBrowserTask(formOnPage));
} else if (scenario === "form_parallel") {
  per = await pool(n, conc, () => freshBrowserTask(formOnPage));
} else if (scenario === "form_context") {
  // Idiomatic Playwright: one browser, one fresh context per task.
  const browser = await chromium.launch(launchOpts());
  for (let i = 0; i < n; i++) {
    const s = performance.now();
    const ctx = await browser.newContext();
    await formOnPage(await ctx.newPage());
    await ctx.close();
    per.push(performance.now() - s);
  }
  await browser.close();
} else {
  throw new Error(`unknown scenario ${scenario}`);
}
const total = performance.now() - t0;
console.log(JSON.stringify({ total_ms: total, per_task_ms: per }));
