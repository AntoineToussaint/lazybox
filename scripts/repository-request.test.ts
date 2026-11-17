import test from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { AgentEventSchema, SessionSchema } from "@lazybox/protocol";
import { additionKey, pickerQuery, repositoryRequestOf, repositoryRequestSummary, sourceLabel } from "../packages/surfaces/src/repository-request-model.ts";
import { deriveAttention } from "../packages/surfaces/src/attention.ts";
import { liveStatus } from "../packages/surfaces/src/live-status.ts";

// As the Runner lists a waiting request: the request itself, in protojson, as provider data.
const event = (request: object) => create(AgentEventSchema, { type: "repository_request", requestId: (request as any).id || "repo-1", text: "Add a repository to this workspace", providerDataJson: new TextEncoder().encode(JSON.stringify(request)) });
const request = { id: "repo-1", collectionId: "alpha", query: "module_directory", reason: "The parser imports its tokenizer", branch: "feature/parser", state: "pending", createdAtUnixMillis: "1700000000000",
  candidates: [{ identity: "github:acme/module-directory", name: "acme/module-directory", source: "github", score: 85 }, { identity: "github:other/module-directory", name: "other/module-directory", source: "github", score: 60 }] };

test("a repository request reads from the pending entry the Runner lists", () => {
  const view = repositoryRequestOf(event(request));
  assert.equal(view.id, "repo-1");
  assert.equal(view.workspace, "alpha");
  assert.equal(view.branch, "feature/parser");
  assert.equal(view.createdAt, 1700000000000);
  assert.deepEqual(view.candidates.map(c => c.name), ["acme/module-directory", "other/module-directory"]);
  assert.equal(sourceLabel(view.candidates[0]), "GitHub · cloned when added");
  // A folder outside the catalog is named by its path.
  const folder = repositoryRequestOf(event({ id: "repo-2", query: "/src/fork", candidates: [{ source: "folder", localPath: "/src/fork", name: "fork" }] }));
  assert.equal(folder.candidates[0].identity, "");
  assert.equal(folder.candidates[0].localPath, "/src/fork");
  // Malformed data reads as a request with nothing matched, never a crash.
  assert.deepEqual(repositoryRequestOf(create(AgentEventSchema, { type: "repository_request", requestId: "r", providerDataJson: new TextEncoder().encode("{") })).candidates, []);
});

test("the picker opens on the words that show the likely repositories together", () => {
  // The agent's spelling differs from the match: search the match's name.
  assert.equal(pickerQuery(repositoryRequestOf(event(request))), "module-directory");
  // The agent's word is in the match: search the word, so its siblings show too.
  assert.equal(pickerQuery(repositoryRequestOf(event({ ...request, query: "tool", candidates: [{ identity: "github:acme/tools-legacy", name: "acme/tools-legacy" }] }))), "tool");
  // Nothing matched: the words, without owner, URL or .git.
  assert.equal(pickerQuery(repositoryRequestOf(event({ ...request, query: "https://github.com/acme/Parser.git", candidates: [] }))), "parser");
});

test("a retried Add is the same addition; another repository is another", () => {
  assert.equal(additionKey("repo-1", "github:acme/a"), additionKey("repo-1", "github:acme/a"));
  assert.notEqual(additionKey("repo-1", "github:acme/a"), additionKey("repo-1", "github:acme/b"));
  assert.ok(additionKey("repo-" + "x".repeat(26), "github:" + "y".repeat(300)).length <= 128, "a request key fits the Runner's limit");
});

test("a waiting repository request needs the person, in the inbox and the status", () => {
  const session = create(SessionSchema, { id: "s", status: "running", busy: true, pending: [event(request)] });
  const attention = deriveAttention(session);
  assert.equal(attention.state, "needs-input");
  assert.equal(attention.reason, "Repository request");
  assert.equal(attention.line, "Add acme/module-directory to this workspace");
  assert.equal(liveStatus(session, Date.now()).text, "Waiting for a repository");
  assert.equal(repositoryRequestSummary(event({ ...request, candidates: [] })), "Add module_directory to this workspace");
});
