import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from "vite";
import react from "@vitejs/plugin-react";
import { webkit, expect } from "./fixtures/playwright.mjs";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
import { startRunnerFixture } from "./fixtures/runner-ui.mjs";
import { collectPageErrors } from "./fixtures/page-errors.mjs";
const f = await startRunnerFixture();
Object.assign(f.github, {
  state: "connected",
  authMethod: "personal_token",
  login: "fixture-user",
});
f.githubRepositories.push({
  id: "303",
  name: "module-datasource",
  fullName: "codefly-dev/module-datasource",
  defaultBranch: "main",
  htmlUrl: "https://github.com/codefly-dev/module-datasource",
});
f.preparationFailures.add("github:fixture-team/api");
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
  const errors = await collectPageErrors(page);
  await page.goto(
    `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${f.token}`,
  );
  await page
    .getByRole("button", { name: "New workspace", exact: true })
    .click();
  let d = page.getByRole("dialog", { name: "New workspace", exact: true });
  const search = d.getByRole("searchbox", { name: "Find repositories" });
  const nameField = d.getByRole("textbox", { name: "Workspace name" });
  const create = d.getByRole("button", { name: "Create workspace", exact: true });
  await expect(d.getByRole("checkbox", { name: "codefly-dev/module-datasource", exact: true })).toBeVisible();
  const bounds = () => Promise.all([d, search, nameField, create].map(locator => locator.boundingBox()));
  const unfiltered = await bounds();
  await search.fill("codefly-dev");
  await expect(d.getByRole("checkbox", { name: "codefly-dev/module-datasource", exact: true })).toHaveCount(0);
  await search.fill("no-such-repository");
  await expect(d.getByText("No matches in the current catalog")).toBeVisible();
  assert.deepEqual(await bounds(), unfiltered, "a zero-result search must leave the dialog and its controls in place");
  await search.fill("data");
  await expect(
    d.getByRole("checkbox", {
      name: "codefly-dev/module-datasource",
      exact: true,
    }),
  ).toBeVisible();
  await expect(
    d.getByText("Add repositories to library", { exact: false }),
  ).toHaveCount(0);
  assert.deepEqual(await bounds(), unfiltered, "one match uses the same space as the unfiltered catalog");
  assert.equal(f.mutations.length, 0, "discovery must not import or clone");
  await d
    .getByRole("checkbox", {
      name: "codefly-dev/module-datasource",
      exact: true,
    })
    .check();
  await d
    .getByRole("searchbox", { name: "Find repositories" })
    .fill("fixture-team/api");
  await d
    .getByRole("checkbox", { name: "fixture-team/api", exact: true })
    .check();
  await expect(d.getByLabel("Workspace name", { exact: true })).toHaveValue(
    "module-datasource + api",
  );
  assert.equal(f.mutations.length, 0, "selection is read-only");
  await d.getByLabel("Workspace name", { exact: true }).fill("   ");
  await d.getByLabel("Workspace name", { exact: true }).press("Enter");
  await page.waitForTimeout(150);
  assert.equal(f.mutations.length, 0, "Enter must respect workspace name validation");
  await d.getByLabel("Workspace name", { exact: true }).fill("Datasource work");
  await page.screenshot({ path: "/tmp/lazybox-unified-new-workspace.png" });
  await d
    .getByRole("button", { name: "Create workspace", exact: true })
    .click();
  await expect(d.getByRole("alert")).toContainText(
    "Fixture preparation failure",
  );
  await expect(
    d.getByRole("checkbox", { name: "fixture-team/api", exact: true }),
  ).toBeChecked();
  await d
    .getByRole("button", { name: "Create workspace", exact: true })
    .click();
  await expect(d).toHaveCount(0);
  assert.equal(
    f.prepared.filter((id) => id === "github:codefly-dev/module-datasource")
      .length,
    1,
  );
  const requests = f.mutations.filter((m) => m.method === "PrepareWorkspace");
  assert.equal(requests.length, 2);
  assert.equal(requests[0].body.requestKey, requests[1].body.requestKey);
  const created = f.collections.find((c) => c.name === "Datasource work");
  assert.equal(created.members.length, 2);
  await expect.poll(() => f.mutations.filter(m => m.method === "OpenSession" && m.body.collectionId === created.id).length).toBe(1);
  await page.waitForLoadState("networkidle");
  await page.reload();
  const row = page
    .locator(".workspace-row")
    .filter({ hasText: "Datasource work" });
  await row.locator("summary").click();
  await row
    .getByRole("button", { name: "Add repositories…", exact: true })
    .click();
  d = page.getByRole("dialog", {
    name: "Add repository to workspace",
    exact: true,
  });
  await expect(d).toContainText("Add repositories to Datasource work");
  await expect(
    d.getByRole("checkbox", { name: /codefly-dev\/module-datasource/ }),
  ).toBeDisabled();
  await d
    .getByRole("searchbox", { name: "Find repositories" })
    .fill("fixture-team/web");
  await d
    .getByRole("checkbox", { name: "fixture-team/web", exact: true })
    .check();
  // Start from: the added repository's branch starts from another branch of origin (⌥B, type, Enter).
  await page.keyboard.press("Alt+KeyB");
  const startPicker = d.getByRole("dialog", { name: "Start fixture-team/web from" });
  await expect(startPicker.getByRole("combobox")).toBeFocused();
  await page.keyboard.type("release/1.4");
  await expect(startPicker.getByRole("option").first()).toContainText("release/1.4");
  await page.keyboard.press("Enter");
  await expect(startPicker).toHaveCount(0);
  await expect(d).toBeVisible();
  await expect(d.locator('[data-start-from="github:fixture-team/web"]')).toHaveText(/from\s*release\/1\.4/);
  await d
    .getByRole("button", { name: "Add to workspace", exact: true })
    .click();
  await expect(d).toHaveCount(0);
  assert.equal(created.members.length, 3);
  assert.deepEqual(
    f.mutations.filter((m) => m.method === "PrepareWorkspace").at(-1).body.startFrom,
    { "github:fixture-team/web": "release/1.4" },
    "Add repository sends the branch it starts from",
  );
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await expect(
    page.getByRole("tab", { name: "GitHub", exact: true }),
  ).toHaveCount(0);
  await page.getByRole("tab", { name: "Repositories", exact: true }).click();
  await page.getByRole("searchbox", { name: "Find repositories" }).fill("data");
  await expect(page.locator('.repository-browser-results [title="codefly-dev/module-datasource"]')).toBeVisible();
  assert.equal(
    f.calls.filter((c) => c.method === "ImportGitHubRepository").length,
    0,
    "UI must not orchestrate individual imports",
  );
  await page
    .getByRole("button", { name: "Connections & folders", exact: true })
    .first()
    .click();
  await expect(
    page.getByRole("region", { name: "GitHub connection" }),
  ).toBeVisible();
  await page.setViewportSize({ width: 800, height: 700 });
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth > innerWidth,
    ),
    false,
  );
  await page.screenshot({ path: "/tmp/lazybox-unified-repositories.png" });
  // A long selection must not hide the primary action on small screens.
  // The app took the bearer out of the address bar when it read it, so the page is at
  // /?view=workspace: going there again with #token= would only change the fragment and
  // leave Settings open. A plain load signs in from the tab's session storage.
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace`);
  await page.route('**/GetRepositoryCatalog', route => route.fulfill({json:{
    entries:Array.from({length:20},(_,i)=>({identity:`github:large-team/service-with-a-long-name-${i}`,name:`large-team/service-with-a-long-name-${i}`})),
    sources:[{id:'github',state:'connected'}],refreshSeconds:300
  }}));
  await page.setViewportSize({width:390,height:640});
  await page.getByRole('button',{name:'Open workspace navigator'}).click();
  await page.getByRole('dialog',{name:'Workspace navigator'}).getByRole('button',{name:'New workspace',exact:true}).click();
  d=page.getByRole('dialog',{name:'New workspace',exact:true});
  await d.getByRole('button',{name:'Refresh',exact:true}).click();
  await expect(d.getByRole('checkbox',{name:'large-team/service-with-a-long-name-19',exact:true})).toBeVisible();
  for(let i=0;i<20;i++) await d.getByRole('checkbox',{name:`large-team/service-with-a-long-name-${i}`,exact:true}).check();
  const action=d.getByRole('button',{name:'Create workspace',exact:true});
  assert.ok(await action.evaluate(el=>{const r=el.getBoundingClientRect();return r.top>=0&&r.bottom<=innerHeight&&el.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2));}), 'primary action stays visible after selecting many repositories');
  const phoneBounds = () => Promise.all([d, action].map(locator => locator.boundingBox()));
  const phoneUnfiltered = await phoneBounds();
  await d.getByRole('searchbox',{name:'Find repositories'}).fill('no-such-repository');
  await expect(d.getByText("No matches in the current catalog")).toBeVisible();
  assert.deepEqual(await phoneBounds(), phoneUnfiltered, "the phone dialog and its primary action stay put as results disappear");
  await d.getByRole('button',{name:'Clear search',exact:true}).click();
  await expect(d.getByRole('searchbox',{name:'Find repositories'})).toHaveValue('');
  for(const width of [320,390]) {
    await page.setViewportSize({width,height:640});
    assert.ok(await action.evaluate(el=>{const r=el.getBoundingClientRect();return r.bottom<=innerHeight&&el.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2));}));
    const nameBox=await d.getByLabel('Workspace name',{exact:true}).boundingBox();
    assert.ok(nameBox.y>=0&&nameBox.y+nameBox.height<640,'name remains visible beside fixed actions');
    assert.equal(await d.evaluate(el=>el.scrollWidth>el.clientWidth),false,'no horizontal dialog overflow');
  }
  await page.screenshot({path:'/tmp/lazybox-polish-repository-picker-phone.png'});
  await d.getByRole('button',{name:'Cancel',exact:true}).click();
  await page.keyboard.press('Escape');
  await page.setViewportSize({width:1280,height:900});
  await page.unroute('**/GetRepositoryCatalog');
  await page.route('**/ListRepositories',route=>route.fulfill({status:503,json:{code:'unavailable',message:'Repository details temporarily unavailable'}}));
  const alpha=page.locator('.workspace-row').filter({hasText:'Alpha feature'});
  await alpha.locator('summary').click();
  await alpha.getByRole('button',{name:'Manage repositories…',exact:true}).click();
  const management=page.getByRole('dialog',{name:'Repositories in Alpha feature'});
  await expect(management.getByRole('alert')).toContainText('Could not load repository details');
  await page.unroute('**/ListRepositories');
  await management.getByRole('button',{name:'Retry',exact:true}).click();
  await expect(management.getByRole('alert')).toHaveCount(0);
  await page.screenshot({path:'/tmp/lazybox-polish-workspace-repositories.png'});
  await management.getByRole('button',{name:/Close/}).first().click().catch(()=>page.keyboard.press('Escape'));
  // Double-clicking a repository chooses it and creates the workspace at once.
  const before=f.mutations.filter(m=>m.method==='PrepareWorkspace').length;
  await page.getByRole('button',{name:'New workspace',exact:true}).click();
  const quick=page.getByRole('dialog',{name:'New workspace',exact:true});
  const selected = quick.getByRole('button', {name: /^Unselect /});
  while (await selected.count()) await selected.first().click();
  await quick.getByRole('searchbox',{name:'Find repositories'}).fill('fixture-team/api');
  await quick.locator('.repository-browser-row label').filter({has:page.getByRole('checkbox',{name:'fixture-team/api',exact:true})}).dblclick();
  await expect(quick).toHaveCount(0);
  const prepared=f.mutations.filter(m=>m.method==='PrepareWorkspace');
  assert.equal(prepared.length,before+1,'one double-click creates exactly one workspace');
  assert.deepEqual(prepared.at(-1).body.repositoryIdentities,['github:fixture-team/api']);
  assert.ok(f.collections.some(c=>c.name==='api'),'the new workspace is named after its repository');
  assert.deepEqual(errors, []);
  console.log(
    "PASS one catalog across new workspace, add repositories, settings and reload; remote discovery without imports; selected identities survive partial failure; idempotent retry; invalid-name guard; 320/390px long selections, fixed actions, clear search, repository-detail retry; no duplicate GitHub tab.",
  );
} finally {
  await browser?.close();
  await server.close();
  await f.close();
}
