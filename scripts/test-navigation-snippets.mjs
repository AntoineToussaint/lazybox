import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from "./fixtures/vite-cache.mjs";
import react from "@vitejs/plugin-react";
import { webkit, expect } from "./fixtures/playwright.mjs";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
import { startRunnerFixture } from "./fixtures/runner-ui.mjs";
import { collectPageErrors, drainPageErrors } from "./fixtures/page-errors.mjs";
const f = await startRunnerFixture();
f.openEvents(f.sessions[0].id);
Object.assign(f.library[0], {
  name: "handbook",
  gitIdentity: "github:acme/handbook",
});
Object.assign(f.sessions[0], {
  title: "what is this repo?",
  provider: "claude",
  model: "claude-fable-5-1",
  modelReported: true,
  status: "done",
});
const server = await createServer({...testViteCache(import.meta.url),
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
  await collectPageErrors(page, errors);
  await page.route("**/ListSnippets", async route => {
    const response = await route.fetch();
    const { snippets } = await response.json();
    const sample = [
      ["clarify", "Explain this PR briefly", "Use deepclarify for more."],
      ["look-closer", "Run a deep review", "Inspect the changes."],
      ["deepclarify", "Explain this PR in depth", "Explain the problem and the proof."],
      ["deepreview", "Review design and risk", "Inspect the design and risk."],
      ["fixall", "Apply review findings", "Apply findings from deepreview."],
      ...Array.from({ length: 16 }, (_, i) => [`example-${i}`, "Example snippet", "An example."]),
    ];
    await route.fulfill({ response, json: { snippets: [...snippets, ...sample.map(([key, description, body]) =>
      ({ key, description, body, category: "Review", origin: "built-in" }))] } });
  });
  await page.goto(
    `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`,
  );
  const rows = page.locator(".workspace-row"),
    alpha = rows.filter({ hasText: "Alpha feature" });
  // A row says what the conversation is doing and where (its task and branch), not which model runs it.
  await expect(alpha).toContainText("what is this repo?");
  await expect(alpha).toContainText("lazybox/alpha/api");
  await expect(alpha).not.toContainText("claude-fable-5-1");
  await expect(page.locator(".workspace-conversations")).toHaveCount(0);
  await expect(alpha).toContainText("acme/handbook");
  await expect(
    alpha.getByRole("button", { name: "New conversation", exact: true }),
  ).toHaveCount(0);
  assert.ok((await alpha.boundingBox()).height < 100);
  await page
    .getByRole("button", { name: "New workspace", exact: true })
    .click();
  let dialog = page.getByRole("dialog", { name: "New workspace", exact: true });
  // Creating starts on the name, its suggestion selected so typing replaces it.
  await expect(dialog.getByLabel("Workspace name", { exact: true })).toBeFocused();
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
  await expect(dialog.getByRole("option").first()).toBeVisible();
  const snippetBounds = await dialog.boundingBox();
  await search.fill("zzqxyunmatched");
  await expect(dialog.getByText("No matching snippets.")).toBeVisible();
  assert.deepEqual(await dialog.boundingBox(), snippetBounds, "empty snippet results stay inside the browser");
  const list = dialog.getByRole("listbox", { name: "Available snippets" });
  await search.fill("");
  await list.evaluate(node => { node.scrollTop = node.scrollHeight; });
  assert.ok(await list.evaluate(node => node.scrollTop) > 0);
  await search.fill("Deep");
  await expect(list.getByRole("option").locator("strong")).toHaveText(["deepclarify", "deepreview", "look-closer"]);
  await expect(list.getByRole("option", { selected: true }).locator("strong")).toHaveText("deepclarify");
  assert.equal(await list.evaluate(node => node.scrollTop), 0);
  await search.press("ArrowDown");
  await expect(dialog.locator(".snippet-preview")).toContainText("Inspect the design and risk.");
  await search.fill("Deep Review");
  await expect(list.getByRole("option", { selected: true }).locator("strong")).toHaveText("deepreview");
  await search.fill("carefully");
  await expect(dialog.getByText("No matching snippets.")).toBeVisible();
  await search.fill("body: carefully");
  await dialog.getByRole("button",{name:"☆ Add to favorites",exact:true}).click();
  await expect(dialog.getByRole("button",{name:"★ Favorite",exact:true})).toHaveAttribute("aria-pressed","true");
  await search.fill("review");
  await expect(dialog.locator(".snippet-preview")).toContainText(
    "Review the current change carefully.",
  );
  await dialog.getByText('Give feedback on this snippet', { exact: true }).click();
  const suggestion = dialog.getByRole('textbox', { name: 'Suggested improvement' });
  await suggestion.fill('Also check test coverage.');
  await suggestion.press('Enter');
  await expect(dialog).toBeVisible();
  await page.route('**/ReviewWorkflow', async route => {
    const request = route.request().postDataJSON();
    assert.equal(request.snippetKey, 'review');
    assert.equal(request.feedback.trim(), 'Also check test coverage.');
    await route.fulfill({ json: { proposalTitles: ['Feedback on snippet review'] } });
  });
  await dialog.getByRole('button', { name: 'Save feedback' }).click();
  await expect(dialog.getByRole('status')).toContainText('Feedback on snippet review');
  await search.press("Shift+Enter");
  await expect(dialog).toHaveCount(0);
  await expect(message).toHaveValue("Review the current change carefully.");
  assert.equal(f.mutations.filter((m) => m.method === "Apply").length, 0);
  await message.fill("");
  await page.getByRole("button", { name: "Snippets", exact: true }).click();
  await dialog.getByRole("searchbox").fill("review");
  let firstReviewer;
  await page.route('**/StartReview', async route => {
    const response = await route.fetch();
    firstReviewer = (await response.json()).session.id;
    await route.abort();
  }, {times: 1});
  await dialog.getByRole("button", {name: "Start review", exact: true}).click();
  await expect(dialog.getByRole("alert")).toContainText(/confirm/i);
  await dialog.getByRole("button", {name: "Close snippets"}).click();
  await page.getByRole("button", {name: "Send message", exact: true}).click();
  await expect(message).toHaveValue("");
  const starts = f.mutations.filter(m => m.method === "StartReview");
  assert.equal(starts.length, 2);
  assert.equal(starts[0].body.focus, "Review the current change carefully.");
  const reviewCard = page.locator(".review-card:visible").last();
  await expect(reviewCard).toContainText("Snippet: review");
  await expect(reviewCard).not.toContainText(starts[0].body.focus);
  assert.ok(starts[0].body.requestKey);
  assert.equal(starts[0].body.requestKey, starts[1].body.requestKey);
  assert.deepEqual(f.reviewOf("alpha").runs.map(r => r.reviewerSessionId), [firstReviewer]);
  assert.equal(f.mutations.filter((m) => m.method === "Apply").length, 0);
  assert.equal(f.mutations.findLast(m => m.method === "StartReview").body.focus, "Review the current change carefully.");
  await expect(message).toHaveValue("");
  const attempts=[];let reject=true;
  await page.route('**/Apply',async route=>{attempts.push(route.request().postDataJSON());if(reject){reject=false;await route.fulfill({status:503,contentType:'application/json',body:JSON.stringify({code:'unavailable',message:'Fixture delivery interruption'})});}else await route.continue();});
  await page.getByRole('button',{name:'Snippets',exact:true}).click();await dialog.getByRole('searchbox').fill('test');await dialog.getByRole('button',{name:'Send snippet',exact:true}).click();await expect(dialog.getByRole('alert')).toContainText('not confirmed');await dialog.getByRole('button',{name:'Close snippets'}).click();await expect(message).toHaveValue('Run the relevant tests.');await page.getByRole('button',{name:'Send message',exact:true}).click();await expect(message).toHaveValue('');assert.equal(attempts.length,2);assert.equal(attempts[0].operationId,attempts[1].operationId);await page.unroute('**/Apply');
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await page.getByRole("tab", { name: "Shortcuts" }).click();
  await page.getByLabel("Preset").selectOption("tmux");
  await page.getByRole("button", { name: "Save shortcuts" }).click();
  await expect(
    page.getByRole("status").filter({ hasText: "Shortcuts saved." }),
  ).toBeVisible();
  await page
    .getByRole("navigation", { name: "Pages" })
    .getByRole("button", { name: "Work", exact: true })
    .click();
  const alphaGroup = page.locator(".collection-group").filter({has: alpha});
  const reviewerTitle = f.reviewOf("alpha").runs[0].reviewerTitle;
  await expect(alphaGroup.locator(".conversation-link").filter({hasText: reviewerTitle})).toBeVisible();
  f.sessions.push({ ...structuredClone(f.sessions[0]), id: "alpha-early", workId: "early", conversationId: "early", title: "Second look", status: "done", busy: false, pending: [], agents: [{ id: "root", status: "done" }] });
  await expect(page.getByRole("navigation", { name: "Workspace conversations" })).toBeVisible();
  await expect(alphaGroup.locator(".conversation-row").filter({hasText: "what is this repo?"}).locator("+ .conversation-row")).toContainText(reviewerTitle);
  await message.focus();
  await message.press("Control+b");
  await expect(alpha.locator(".workspace-link")).toBeFocused();
  await page.keyboard.press("ArrowDown");
  // Down from the workspace is its first conversation, whichever conversation is listed first now.
  await expect.poll(() => page.evaluate(() => {
    const first = document.querySelector('nav[aria-label="Workspace conversations"] .conversation-link');
    return !!first && document.activeElement === first;
  })).toBe(true);
  // The leader menu is configurable: add an entry, see it in the which-key card, use it.
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await page.getByRole("tab", { name: "Shortcuts" }).click();
  await page.getByRole("button", { name: "Add entry", exact: true }).click();
  // `r` is the built-in "Run the last command block" (] ] r): an entry takes a free key.
  await page.getByLabel("Key for entry 1").fill("x");
  await page.getByLabel("Snippet for entry 1").selectOption("review");
  await page.getByLabel("Action for entry 1").selectOption("insert");
  await page.getByRole("button", { name: "Save menu", exact: true }).click();
  await expect(page.getByRole("status").filter({ hasText: "Menu saved." })).toBeVisible();
  await page.getByRole("navigation", { name: "Pages" }).getByRole("button", {name:"Work", exact:true}).click();
  await message.fill("");
  await message.focus();
  await message.press("Control+b");
  const menu = page.getByRole("status", { name: "Keyboard menu" });
  await expect(menu).toContainText("Ctrl+b");
  await expect(menu.locator("li").filter({ hasText: "Review this change" })).toContainText("x");
  await expect(menu.locator("li").filter({ hasText: "Snippets" })).toContainText("s");
  await page.keyboard.press("x");
  await expect(menu).toHaveCount(0);
  await expect(message).toHaveValue("Review the current change carefully.");
  f.sessions.push({
    ...f.sessions[0],
    id: "alpha-second",
    workId: "second",
    conversationId: "second",
    title: "Second conversation",
  });
  await page.getByRole("button", { name: "Refresh workspaces" }).click();
  // Several conversations are listed under the card, one line each; folded,
  // the row names the one it shows and counts the rest ("+2"), the full count its tooltip.
  await expect(
    page.locator(".workspace-conversations .conversation-row"),
  ).toHaveCount(3);
  await expect(alpha.locator(".workspace-agent")).toHaveCount(0);
  await alpha.getByRole("button", { name: /^Fold the 3 conversations/ }).click();
  await expect(alpha.locator(".workspace-agent-more")).toHaveText("+2");
  await expect(alpha.locator(".workspace-agent-more")).toHaveAttribute("title", "3 conversations");
  await expect(page.locator(".workspace-conversations")).toHaveCount(0);
  await page.screenshot({ path: "/tmp/lazybox-navigation-multiple.png" });
  await drainPageErrors(errors);assert.deepEqual(errors, []);
  console.log(
    "PASS unified single conversation, multiple children, icon actions, initial search focus, literal bracket, Vim/tmux navigation, snippet preview/insert/send and saved shortcut settings. Fixture only.",
  );
} finally {
  await browser?.close();
  await server.close();
  await f.close();
}
