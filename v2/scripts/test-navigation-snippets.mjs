import { createServer } from "vite";
import react from "@vitejs/plugin-react";
import { webkit, expect } from "@playwright/test";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
import { startRunnerFixture } from "./fixtures/runner-ui.mjs";
const f = await startRunnerFixture();
Object.assign(f.library[0], {
  name: "handbook",
  gitIdentity: "github:obin-ai/handbook",
});
Object.assign(f.sessions[0], {
  title: "what is this repo?",
  provider: "claude",
  model: "claude-fable-5-1",
  modelReported: true,
  status: "done",
});
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
    viewport: { width: 1280, height: 900 },
  });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.goto(
    `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`,
  );
  const rows = page.locator(".workspace-row"),
    alpha = rows.filter({ hasText: "Alpha feature" });
  await expect(alpha).toContainText("claude-fable-5-1");
  await expect(page.locator(".workspace-conversations")).toHaveCount(0);
  await expect(alpha).toContainText("obin-ai/handbook");
  await expect(
    alpha.getByRole("button", { name: "New conversation", exact: true }),
  ).toHaveCount(0);
  assert.ok((await alpha.boundingBox()).height < 100);
  await page
    .getByRole("button", { name: "New workspace", exact: true })
    .click();
  let dialog = page.getByRole("dialog", { name: "New workspace", exact: true });
  await expect(dialog.getByRole("searchbox")).toBeFocused();
  await page.screenshot({ path: "/tmp/lazybox-navigation-workspace.png" });
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  const message = page.getByRole("textbox", { name: "Message", exact: true });
  await message.fill("A");
  await message.press("]");
  await expect(message).toHaveValue("A]");
  await message.press("]");
  await expect(alpha.locator(".workspace-link")).toBeFocused();
  await expect(message).toHaveValue("A");
  await page.keyboard.press("j");
  await expect(rows.nth(1).locator(".workspace-link")).toBeFocused();
  await page.keyboard.press("k");
  await expect(alpha.locator(".workspace-link")).toBeFocused();
  await page.keyboard.press("l");
  await expect(message).toBeFocused();
  await expect(message).toHaveValue("A");
  await message.fill("");
  await message.press("]");
  await message.press("]");
  await page.keyboard.press("s");
  dialog = page.getByRole("dialog", { name: "Snippets", exact: true });
  await expect(dialog).toBeVisible();
  const search = dialog.getByRole("searchbox", { name: "Find snippets" });
  await search.fill("carefully");
  await dialog.getByRole("button",{name:"☆ Add to favorites",exact:true}).click();
  await expect(dialog.getByRole("button",{name:"★ Favorite",exact:true})).toHaveAttribute("aria-pressed","true");
  await search.fill("review");
  await expect(dialog.locator(".snippet-preview")).toContainText(
    "Review the current change carefully.",
  );
  await search.press("Shift+Enter");
  await expect(dialog).toHaveCount(0);
  await expect(message).toHaveValue("Review the current change carefully.");
  assert.equal(f.mutations.filter((m) => m.method === "Apply").length, 0);
  await message.fill("");
  await page.getByRole("button", { name: "Snippets", exact: true }).click();
  await dialog.getByRole("searchbox").fill("review");
  await dialog
    .getByRole("button", { name: "Send snippet", exact: true })
    .click();
  await expect(dialog).toHaveCount(0);
  assert.equal(f.mutations.filter((m) => m.method === "Apply").length, 1);
  await expect(message).toHaveValue("");
  const attempts=[];let reject=true;
  await page.route('**/Apply',async route=>{attempts.push(route.request().postDataJSON());if(reject){reject=false;await route.fulfill({status:503,contentType:'application/json',body:JSON.stringify({code:'unavailable',message:'Fixture delivery interruption'})});}else await route.continue();});
  await page.getByRole('button',{name:'Snippets',exact:true}).click();await dialog.getByRole('searchbox').fill('review');await dialog.getByRole('button',{name:'Send snippet',exact:true}).click();await expect(dialog.getByRole('alert')).toContainText('not confirmed');await dialog.getByRole('button',{name:'Close snippets'}).click();await expect(message).toHaveValue('Review the current change carefully.');await page.getByRole('button',{name:'Send message',exact:true}).click();await expect(message).toHaveValue('');assert.equal(attempts.length,2);assert.equal(attempts[0].operationId,attempts[1].operationId);await page.unroute('**/Apply');
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await page.getByRole("tab", { name: "Shortcuts" }).click();
  await page.getByLabel("Preset").selectOption("tmux");
  await page.getByRole("button", { name: "Save shortcuts" }).click();
  await expect(
    page.getByRole("status").filter({ hasText: "Shortcuts saved." }),
  ).toBeVisible();
  await page
    .getByRole("navigation", { name: "Pages" })
    .getByRole("button")
    .first()
    .click();
  await message.focus();
  await message.press("Control+b");
  await expect(alpha.locator(".workspace-link")).toBeFocused();
  await page.keyboard.press("ArrowDown");
  await expect(rows.nth(1).locator(".workspace-link")).toBeFocused();
  f.sessions.push({
    ...f.sessions[0],
    id: "alpha-second",
    workId: "second",
    conversationId: "second",
    title: "Second conversation",
  });
  await page.getByRole("button", { name: "Refresh workspaces" }).click();
  await expect(alpha).toContainText("2 conversations");
  await expect(
    page.locator(".workspace-conversations .conversation-row"),
  ).toHaveCount(2);
  await page.screenshot({ path: "/tmp/lazybox-navigation-multiple.png" });
  assert.deepEqual(errors, []);
  console.log(
    "PASS unified single conversation, multiple children, icon actions, initial search focus, literal bracket, Vim/tmux navigation, snippet preview/insert/send and saved shortcut settings. Fixture only.",
  );
} finally {
  await browser?.close();
  await server.close();
  await f.close();
}
