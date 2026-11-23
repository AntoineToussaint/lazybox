import { createSdkMcpServer, tool, type McpSdkServerConfigWithInstance } from '@anthropic-ai/claude-agent-sdk';
import { z } from 'zod';
import { errorReason as reason } from './error-text.js';

/**
 * The agent browser's tools (BROWSER.md) on an in-process MCP server. Each
 * call goes to the Runner under the same name (browser_open, …), which runs it
 * on this conversation's own browser; the Runner asks the person about new
 * sites, downloads and sign-ins, so these tools are allowed without a
 * per-call prompt. The tool list mirrors agent.BrowserTools in Go.
 */
export const browserServerName = 'lazybox_browser';
/** The permission rule that covers every tool of the server. */
export const browserRule = `mcp__${browserServerName}`;

type RunnerCall = (name: string, args: unknown) => Promise<string>;
type Content = { type: 'text'; text: string } | { type: 'image'; data: string; mimeType: string };

/** The Runner's result as MCP content: its text, and a screenshot as an image. */
export function browserContent(raw: string): Content[] {
  let parsed: { text?: string; image?: { mediaType: string; data: string } } | undefined;
  try { parsed = JSON.parse(raw); } catch { /* plain text */ }
  if (!parsed || typeof parsed !== 'object' || (typeof parsed.text !== 'string' && !parsed.image)) return [{ type: 'text', text: raw }];
  const out: Content[] = [{ type: 'text', text: parsed.text || '' }];
  if (parsed.image?.data) out.push({ type: 'image', data: parsed.image.data, mimeType: parsed.image.mediaType || 'image/jpeg' });
  return out;
}


const ref = z.string().describe('The element\'s ref from the latest browser_snapshot, such as e12.');
const element = z.string().optional().describe('A few words naming the element for the person watching, such as "Sign in button".');
const site = { url: z.string().describe('An http or https URL.'), reason: z.string().optional().describe('One short sentence for the person: why you need this site.') };
const read = { readOnlyHint: true }, act = { readOnlyHint: false, destructiveHint: false, openWorldHint: true };

/** The browser tools: name, description, input shape, annotations. */
export const browserTools = [
  ['browser_open', 'Open your browser (a Chromium the person watches live beside the chat), optionally at a URL. The first visit to each site asks the person unless it is allowed. Returns the page\'s title, URL and a compact snapshot with element refs.', { url: site.url.optional(), reason: site.reason }, act],
  ['browser_navigate', 'Go to a URL in your browser. A site not visited yet asks the person first, with your reason. Returns the page\'s title, URL and a compact snapshot.', site, act],
  ['browser_snapshot', 'Read the current page as a compact accessibility tree with element refs ([ref=e12]) to click, type into or select. Refs are valid until the next snapshot.', { maxChars: z.number().int().min(500).max(40000).optional().describe('Cap on the snapshot\'s length; 8000 by default.') }, read],
  ['browser_click', 'Click an element by its ref. Returns what happened and a fresh snapshot.', { ref, element }, act],
  ['browser_type', 'Type text into a field by its ref, replacing its value; submit presses Enter. Never type passwords or other credentials: ask the person to take over the browser and type them.', { ref, element, text: z.string(), submit: z.boolean().optional().describe('Press Enter after typing.') }, act],
  ['browser_select', 'Choose one or more options of a select element by its ref.', { ref, element, values: z.array(z.string()).min(1).describe('Option values or labels.') }, act],
  ['browser_press', 'Press a key in the page, such as Enter, Escape, Tab or ArrowDown.', { key: z.string() }, act],
  ['browser_scroll', 'Scroll the page (or an element into view by its ref).', { direction: z.enum(['up', 'down', 'left', 'right']).optional(), amount: z.number().min(1).optional().describe('CSS pixels; most of a screen by default.'), ref: z.string().optional().describe('Scroll this element into view instead.') }, read],
  ['browser_screenshot', 'Take a screenshot of the page; it is attached to the conversation as an image. Prefer browser_snapshot to read a page: it is far cheaper.', { fullPage: z.boolean().optional().describe('The whole page rather than the visible part.') }, read],
  ['browser_back', 'Go back one page in your browser\'s history.', {}, act],
  ['browser_forward', 'Go forward one page in your browser\'s history.', {}, act],
  ['browser_wait_for', 'Wait until text appears, text disappears, or an element by ref becomes visible.', { text: z.string().optional(), textGone: z.string().optional(), ref: z.string().optional(), timeoutSeconds: z.number().min(1).max(60).optional() }, read],
  ['browser_close', 'Close your browser when you are done with it.', {}, act],
] as const;

/** Each tool routes its call to the Runner tool of the same name. */
export function browserToolDefinitions(call: RunnerCall) {
  return browserTools.map(([name, description, shape, annotations]) => tool(name, description, shape, async args => {
    try { return { content: browserContent(await call(name, args)) }; } catch (error) { return { content: [{ type: 'text' as const, text: reason(error) }], isError: true }; }
  }, { annotations }));
}

export function browserServer(call: RunnerCall): McpSdkServerConfigWithInstance {
  return createSdkMcpServer({ name: browserServerName, version: '0.1.0', tools: browserToolDefinitions(call) });
}
