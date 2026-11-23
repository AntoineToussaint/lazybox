import { createSdkMcpServer, tool, type McpSdkServerConfigWithInstance } from '@anthropic-ai/claude-agent-sdk';
import { z } from 'zod';
import { errorReason } from './error-text.js';

/** The help assistant's MCP server: Lazybox's own settings, read and changed through the Runner. */
export const settingsServerName = 'lazybox_settings';
export const readSettingsTool = `mcp__${settingsServerName}__get_settings`;
export const updateSettingsTool = `mcp__${settingsServerName}__update_settings`;
/** The documentation tools, on the same server: read-only, never asked. */
export const searchDocsTool = `mcp__${settingsServerName}__search_docs`;
export const readDocTool = `mcp__${settingsServerName}__read_doc`;
/** Runner tool names; the Runner validates and applies, the worker never touches settings files. */
const runnerRead = 'lazybox_get_settings', runnerUpdate = 'lazybox_update_settings';
const runnerSearchDocs = 'lazybox_search_docs', runnerReadDoc = 'lazybox_read_doc';

/** Reading needs no approval; every update is put to the user, whatever else the native settings allow. */
export const helpPermissions = { allow: [readSettingsTool, searchDocsTool, readDocTool], ask: [updateSettingsTool] };

const updateShape = {
  revision: z.number().int().positive().describe('The revision get_settings returned. A stale revision is refused.'),
  reason: z.string().min(1).max(2000).describe('One or two plain sentences for the user: what changes and why. Shown on the approval card.'),
  changes: z.strictObject({
    naming: z.enum(['repositories', 'generated']).optional().describe('Suggested workspace names.'),
    permission: z.enum(['', 'ask', 'read-only', 'yolo']).optional().describe('Default permissions for new coding sessions; "" keeps the current CLI settings, "yolo" is full access with no approval prompts.'),
    context: z.string().max(32768).optional().describe('Lazybox context for every new agent; replaces the current text whole.'),
    claudeContext: z.string().max(16384).optional().describe('Additional Claude context; replaces the current text whole.'),
    codexContext: z.string().max(16384).optional().describe('Additional Codex context; replaces the current text whole.'),
  }).describe('Only the settings that change.'),
};
const updateSchema = z.object(updateShape);

/** Key-sorted JSON, so an approved input and the handler's arguments compare by value. */
export function canonical(value: unknown): string {
  return JSON.stringify(value, (_key, v) => v && typeof v === 'object' && !Array.isArray(v) ? Object.fromEntries(Object.entries(v).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)) : v);
}

/**
 * The user's approvals of settings updates. The update tool runs once per
 * approval of exactly that input, so a change no one approved never reaches
 * the Runner — even if a native setting or hook would have allowed it silently.
 */
export class SettingsApprovals {
  private granted = new Map<string, number>();
  private key(input: unknown): string | undefined {
    const parsed = updateSchema.safeParse(input);
    return parsed.success ? canonical(parsed.data) : undefined;
  }
  grant(input: unknown) {
    const key = this.key(input);
    if (key) this.granted.set(key, (this.granted.get(key) || 0) + 1);
  }
  consume(input: unknown): boolean {
    const key = this.key(input), count = key ? this.granted.get(key) || 0 : 0;
    if (!key || count === 0) return false;
    if (count === 1) this.granted.delete(key); else this.granted.set(key, count - 1);
    return true;
  }
}

type RunnerCall = (name: string, args: unknown) => Promise<string>;
const text = (value: string, isError = false) => ({ content: [{ type: 'text' as const, text: value }], ...(isError ? { isError: true } : {}) });
const reason = errorReason;

export function settingsServer(call: RunnerCall, approvals: SettingsApprovals): McpSdkServerConfigWithInstance {
  const { read, update } = settingsTools(call, approvals);
  const { search, page } = docsTools(call);
  return createSdkMcpServer({ name: settingsServerName, version: '0.1.0', tools: [search, page, read, update] });
}

/** The Lazybox documentation (Help → Documentation), read through the Runner. */
export function docsTools(call: RunnerCall) {
  const search = tool('search_docs', 'Search the Lazybox documentation (the pages Help → Documentation shows) by a few words. Returns the best pages with their path, title and matching lines.', {
    query: z.string().min(1).max(500).describe('A few words about the topic, such as "switch subscription limit".'),
  }, async args => {
    try { return text(await call(runnerSearchDocs, args)); } catch (error) { return text(reason(error), true); }
  }, { annotations: { readOnlyHint: true } });
  const page = tool('read_doc', 'Read one Lazybox documentation page by the path search_docs or the page list gives, such as guides/chat.md.', {
    path: z.string().min(1).max(500).describe('The page path inside the documentation, such as guides/subscriptions.md.'),
  }, async args => {
    try { return text(await call(runnerReadDoc, args)); } catch (error) { return text(reason(error), true); }
  }, { annotations: { readOnlyHint: true } });
  return { search, page };
}

export function settingsTools(call: RunnerCall, approvals: SettingsApprovals) {
  const read = tool('get_settings', "Read Lazybox's agent settings (Settings → Agents & workspaces) and the revision an update must name. Never returns credentials.", {}, async () => {
    try { return text(await call(runnerRead, {})); } catch (error) { return text(reason(error), true); }
  }, { annotations: { readOnlyHint: true } });
  const update = tool('update_settings', "Propose a change to Lazybox's agent settings. The user sees your reason and the proposed values and must allow it; nothing changes otherwise. Changeable: naming, permission, context, claudeContext, codexContext. Prices, accounts, credentials, repositories, keyboard shortcuts and snippets cannot be changed.", updateShape, async args => {
    if (!approvals.consume(args)) return text('Not applied: the user has not approved this exact change. Propose it again so they can review it.', true);
    try { return text(await call(runnerUpdate, args)); } catch (error) { return text(reason(error), true); }
  }, { annotations: { readOnlyHint: false, destructiveHint: false, idempotentHint: true } });
  return { read, update };
}
