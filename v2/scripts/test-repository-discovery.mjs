import { createServer } from "vite";
import react from "@vitejs/plugin-react";
import { webkit, expect } from "@playwright/test";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
import { startRunnerFixture } from "./fixtures/runner-ui.mjs";
const f = await startRunnerFixture();
const server = await createServer({
  configFile: false,
  root: fileURLToPath(new URL("../apps/web", import.meta.url)),
  plugins: [react()],
  server: {
    host: "127.0.0.1",
    port: 0,
    proxy: { "/lazybox.bench.v1.RunnerService/": { target: f.url } },
  },
});
let browser;
try {
  await server.listen();
  browser = await webkit.launch();
  const page = await browser.newPage({
    viewport: { width: 1200, height: 850 },
  });
  await page.goto(
    `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${f.token}`,
  );
  await page
    .getByRole("button", { name: "New workspace", exact: true })
    .click();
  const d = page.getByRole("dialog", { name: "New workspace", exact: true });
  await d
    .getByRole("button", { name: "Connections & folders", exact: true })
    .first()
    .click();
  await d
    .getByRole("textbox", { name: "Folder to search", exact: true })
    .fill("/fixture/development");
  await d
    .getByRole("button", { name: "Find repositories in folder", exact: true })
    .click();
  await expect(
    d.getByRole("checkbox", { name: "team/api", exact: true }),
  ).toHaveCount(1);
  assert.equal(f.library.length, 2);
  await d
    .getByRole("button", { name: "Connections & folders", exact: true })
    .first()
    .click();
  await d.getByRole("checkbox", { name: "team/api", exact: true }).check();
  await expect(d.getByLabel("Workspace name", { exact: true })).toHaveValue(
    "api",
  );
  await d.getByRole("checkbox", { name: "team/api", exact: true }).uncheck();
  await d.getByRole("checkbox", { name: "team/tools", exact: true }).check();
  await expect(d.getByLabel("Workspace name", { exact: true })).toHaveValue(
    "tools",
  );
  await d.getByLabel("Workspace name", { exact: true }).fill("Tools feature");
  await d
    .getByRole("button", { name: "Create workspace", exact: true })
    .click();
  await expect(d).toHaveCount(0);
  assert.equal(f.prepared.length, 1);
  assert.equal(f.prepared[0], "github:team/tools");
  await page.reload();
  await page
    .getByRole("button", { name: "New workspace", exact: true })
    .click();
  await expect(
    d.getByRole("checkbox", { name: "team/api", exact: true }),
  ).toHaveCount(1);
  assert.equal(
    f.calls.filter((c) => c.method === "ScanRepositoryRoot").length,
    1,
  );
  await d
    .getByRole("checkbox", { name: "Start without repositories", exact: true })
    .check();
  await d.getByLabel("Workspace name", { exact: true }).fill("Planning");
  await d
    .getByRole("button", { name: "Create workspace", exact: true })
    .click();
  await expect(d).toHaveCount(0);
  assert.equal(
    f.collections.find((c) => c.name === "Planning").members.length,
    0,
  );
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await page.getByRole("tab", { name: "Repositories", exact: true }).click();
  await page
    .getByRole("button", {
      name: "Remove team/api from suggestions",
      exact: true,
    })
    .click();
  await page
    .getByRole("button", { name: "Remove from suggestions", exact: true })
    .click();
  await expect(
    page
      .locator(".repository-browser-results")
      .getByText("team/api", { exact: true }),
  ).toHaveCount(0);
  assert.equal(
    f.collections.find((c) => c.name === "Tools feature").members.length,
    1,
  );
  assert.equal(
    f.calls.filter((c) =>
      ["ImportGitHubRepository", "AddLocalRepository"].includes(c.method),
    ).length,
    0,
  );
  console.log(
    "PASS shared local discovery, deduplicated clones/worktrees, cached reload, corrected naming, explicit empty workspace, removal preserves workspaces; UI never prepares sources individually.",
  );
} finally {
  await browser?.close();
  await server.close();
  await f.close();
}
