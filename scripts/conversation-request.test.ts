import test from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { AgentEventSchema, SessionSchema } from "@lazybox/protocol";
import { briefPreview, startRequestOf, startRequestSummary, workspaceWords } from "../packages/surfaces/src/conversation-request-model.ts";
import { deriveAttention } from "../packages/surfaces/src/attention.ts";
import { liveStatus } from "../packages/surfaces/src/live-status.ts";
import { rowPlan } from "../packages/surfaces/src/remote/row-plan.ts";

// As the Runner lists a waiting request: the request itself, in protojson, as provider data.
const event = (request: object) => create(AgentEventSchema, { type: "conversation_request", requestId: (request as any).id || "start-1", text: "Start a conversation: " + ((request as any).title || ""), providerDataJson: new TextEncoder().encode(JSON.stringify(request)) });
const request = { id: "start-1", sessionId: "s", conversationId: "c", collectionId: "alpha", fromTitle: "Plan the review", title: "Review all modules", brief: "One\nTwo\nThree\nFour", reason: "asked in chat",
  workspace: "same", provider: "claude", profileId: "claude-default", model: "claude-opus-5-5", permission: "ask", sharedWith: 3, briefArtifactId: "spec-1", briefArtifactTitle: "Spec", briefArtifactVersion: 2, state: "pending", createdAtUnixMillis: "1700000000000" };

test("a request to start a conversation reads from the pending entry the Runner lists", () => {
  const view = startRequestOf(event(request));
  assert.equal(view.id, "start-1");
  assert.equal(view.workspace, "alpha");
  assert.equal(view.from, "Plan the review");
  assert.equal(view.mode, "same");
  assert.equal(view.sharedWith, 3);
  assert.deepEqual(view.artifact, { id: "spec-1", title: "Spec", version: 2 });
  assert.equal(view.createdAt, 1700000000000);
  assert.equal(startRequestSummary(event(request)), "Start a conversation: Review all modules");
  // Malformed data still reads, from the entry's own text.
  const broken = startRequestOf(create(AgentEventSchema, { type: "conversation_request", requestId: "r", text: "Start a conversation: Docs", providerDataJson: new TextEncoder().encode("{") }));
  assert.equal(broken.title, "Docs");
  assert.equal(broken.artifact, undefined);
});

test("the card says where the conversation would work, and with how many others", () => {
  assert.deepEqual(workspaceWords({ mode: "same", sharedWith: 3 }), { label: "This one", detail: "Shared with 3 running conversations: they edit the same files" });
  assert.equal(workspaceWords({ mode: "same", sharedWith: 1 }).detail, "Shared with this conversation: they edit the same files");
  assert.equal(workspaceWords({ mode: "new", sharedWith: 3 }).label, "A new workspace");
  assert.equal(workspaceWords({ mode: "new", sharedWith: 1, startFrom: "feat/v2" }).detail, "Its own branches, started from origin’s feat/v2", "--start-from shows on the request");
  assert.deepEqual(briefPreview("One\nTwo\nThree\nFour"), { text: "One\nTwo\nThree", more: true });
  assert.deepEqual(briefPreview("Short"), { text: "Short", more: false });
  assert.equal(briefPreview("x".repeat(400)).text.length, 280);
});

test("it needs the person: the Inbox, the status line and the phone say so", () => {
  const session = create(SessionSchema, { id: "s", title: "Plan the review", pending: [event(request)] });
  const attention = deriveAttention(session);
  assert.equal(attention.state, "needs-input");
  assert.equal(attention.reason, "New conversation");
  assert.equal(attention.line, "Start a conversation: Review all modules");
  assert.equal(liveStatus(session).text, "Waiting to start a conversation");
  // A phone row opens the conversation, where its card offers Start and Deny.
  const plan = rowPlan(attention, true);
  assert.equal(plan.kind, "elsewhere");
});
