import { spawn } from "node:child_process";
import { readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const url = "http://127.0.0.1:4321/";
const reportPath = join(tmpdir(), `lazybox-lighthouse-${process.pid}.json`);
const nodeModules = new URL("../node_modules/", import.meta.url);
const astro = new URL("astro/bin/astro.mjs", nodeModules);
const lighthouse = new URL("lighthouse/cli/index.js", nodeModules);

function run(command, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: "inherit", ...options });
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (code === 0) resolve();
      else reject(new Error(`${command} exited with ${code ?? signal}`));
    });
  });
}

async function waitForPreview() {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    try {
      const response = await fetch(url);
      if (response.ok) return;
    } catch {
      // Astro is still starting.
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`Astro preview did not become ready at ${url}`);
}

const preview = spawn(
  process.execPath,
  [astro.pathname, "preview", "--host", "127.0.0.1", "--port", "4321"],
  { stdio: "inherit" },
);

try {
  await waitForPreview();
  await run(process.execPath, [
    lighthouse.pathname,
    url,
    "--quiet",
    "--preset=desktop",
    "--chrome-flags=--headless --no-sandbox",
    "--output=json",
    `--output-path=${reportPath}`,
  ]);

  const report = JSON.parse(await readFile(reportPath, "utf8"));
  const minimumScores = {
    performance: 0.8,
    accessibility: 0.95,
    "best-practices": 0.95,
    seo: 0.95,
  };
  const failures = [];
  for (const [category, minimum] of Object.entries(minimumScores)) {
    const score = report.categories[category]?.score ?? 0;
    console.log(`${category}: ${Math.round(score * 100)} (minimum ${minimum * 100})`);
    if (score < minimum) failures.push(`${category} ${score * 100} < ${minimum * 100}`);
  }

  const transferred = report.audits["total-byte-weight"]?.numericValue ?? Infinity;
  const maxTransferred = 500 * 1024;
  console.log(`transferred: ${Math.round(transferred / 1024)} KiB (maximum 500 KiB)`);
  if (transferred > maxTransferred) {
    failures.push(`transferred ${Math.round(transferred / 1024)} KiB > 500 KiB`);
  }

  if (failures.length > 0) {
    throw new Error(`Lighthouse budgets failed: ${failures.join(", ")}`);
  }
} finally {
  preview.kill("SIGTERM");
  await rm(reportPath, { force: true });
}
