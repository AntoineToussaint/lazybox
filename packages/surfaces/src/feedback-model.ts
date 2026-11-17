/**
 * Help → Send feedback: the report, built in one place so the preview the
 * person reads is exactly what is saved, copied or put in the GitHub issue.
 * Lazybox uploads nothing: the report becomes a local file, the clipboard,
 * or a pre-filled issue the person reads and posts themselves.
 */
import type { FeedbackDiagnostics } from '@lazybox/platform';

export const FEEDBACK_REPOSITORY = 'AntoineToussaint/lazybox';
/** Lines of the Runner's log a report carries at most. */
export const LOG_LINES = 200;
/** GitHub refuses very long new-issue URLs; the body is cut well before. */
export const ISSUE_URL_LIMIT = 7000;

const escape = (text: string) => text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/**
 * Removes what must never leave this Mac in a report: credentials (bearer
 * tokens, GitHub, Anthropic, OpenAI, Slack and AWS keys, JWTs, any
 * `token=`/`secret:`/`password=`-like value, but not a plain count like
 * `input_tokens=1200`) and the person's identity in
 * paths and addresses (the home directory reads `~`, another user's home
 * `/Users/[user]`, e-mail addresses `[email]`).
 */
export function redact(text: string, { home = '' }: { home?: string } = {}): string {
  let out = text;
  const secret = '[redacted]';
  out = out.replace(/\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]{8,}/g, `$1 ${secret}`);
  out = out.replace(/\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})/g, secret);
  out = out.replace(/\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{16,}/g, secret);
  out = out.replace(/\bxox[abposr]-[A-Za-z0-9-]{10,}/g, secret);
  out = out.replace(/\b(?:AKIA|ASIA)[0-9A-Z]{16}\b/g, secret);
  out = out.replace(/\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}/g, secret);
  // name=value, name: value, "name":"value" where the name says what it is.
  out = out.replace(/((?:[A-Za-z0-9_-]*(?:token|secret|password|passwd|apikey|api_key|api-key|credential|authorization|cookie|session_key|private_key)[A-Za-z0-9_-]*)["']?\s*[:=]\s*["']?)([^\s"'&,;}]{4,})/gi, (match: string, name: string, value: string) => /^\d{1,12}$/.test(value) || /^(Bearer|Basic|\[redacted\])$/i.test(value) ? match : `${name}${secret}`);
  out = out.replace(/([?&](?:code|key|sig|signature|access_token|refresh_token|state)=)[^&\s"'#]+/gi, `$1${secret}`);
  out = out.replace(/(:\/\/)[^/\s:@]+:[^/\s@]+@/g, `$1${secret}@`);
  const trimmed = home.replace(/\/+$/, '');
  if (trimmed && trimmed !== '/') out = out.replace(new RegExp(`${escape(trimmed)}(?=/|\\b|$)`, 'g'), '~');
  out = out.replace(/\/Users\/(?!Shared\b)[^/\s"'`:]+/g, '/Users/[user]');
  out = out.replace(/\/home\/[^/\s"'`:]+/g, '/home/[user]');
  out = out.replace(/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/g, '[email]');
  return out;
}

/** The last `count` lines of a log. */
export const tailLines = (text: string, count = LOG_LINES) => text.replace(/\s+$/, '').split('\n').slice(-count).join('\n');

export type FeedbackConversation = { title: string; agent: string; model: string; state: string; error: string };
export type FeedbackInput = {
  what: string;
  app: { version: string; channel: string };
  runnerVersion: string;
  /** Where the window runs: macOS and its architecture in the app, the browser otherwise (part of the diagnostics). */
  platform: string;
  screenshot: boolean;
  diagnostics?: FeedbackDiagnostics;
  includeDiagnostics: boolean;
  conversation?: FeedbackConversation;
  includeConversation: boolean;
};

/** The report, Markdown, already redacted: what the preview shows and every destination gets. */
export function feedbackReport(input: FeedbackInput): string {
  const home = input.diagnostics?.home || '';
  const clean = (text: string) => redact(text, { home });
  const lines = ['# Lazybox feedback', '', '## What happened', '', clean(input.what.trim()) || '_(not filled in yet)_', ''];
  lines.push('## Versions', '');
  lines.push(`- Lazybox ${input.app.version || 'unknown'}${input.app.channel ? ` (${input.app.channel})` : ''}`);
  lines.push(`- Runner ${input.runnerVersion || 'not answering'}`);
  lines.push('');
  if (input.includeConversation && input.conversation) {
    const c = input.conversation;
    lines.push('## Conversation', '', `- Title: ${clean(c.title) || '(untitled)'}`, `- Agent: ${[c.agent, c.model].filter(Boolean).join(' · ') || 'unknown'}`, `- State: ${c.state || 'unknown'}`);
    if (c.error) lines.push(`- Last error: ${clean(c.error)}`);
    lines.push('', '_Its messages are not included._', '');
  }
  if (input.includeDiagnostics) {
    const d = input.diagnostics;
    lines.push('## Diagnostics', '', `- ${clean(input.platform)}`);
    if (d?.runnerLog?.tail.trim()) {
      const tail = tailLines(d.runnerLog.tail);
      lines.push(`- Runner log: last ${tail.split('\n').length} lines of ${clean(d.runnerLog.path)}, credentials removed`, '', '```text', clean(tail), '```');
    } else lines.push('- Runner log: none available in this build');
    lines.push('');
  }
  if (input.screenshot) lines.push('Screenshot: `screenshot.png`, saved next to this report.', '');
  return lines.join('\n');
}

/** The issue's title: the first line of what happened, redacted like the report, short. */
export function issueTitle(what: string, home = ''): string {
  const first = redact(what.trim().split('\n')[0].trim().replace(/\s+/g, ' '), { home });
  const short = first.length > 72 ? `${first.slice(0, 71).trimEnd()}…` : first;
  return `Feedback: ${short || 'Lazybox'}`;
}

/**
 * A new-issue URL with the report as its body, for the person to read and
 * post. A report too long for a URL keeps its beginning and says where the
 * whole one is (the saved file to attach).
 */
export function issueUrl(report: string, what: string, { savedAt = '', home = '' }: { savedAt?: string; home?: string } = {}): string {
  const base = `https://github.com/${FEEDBACK_REPOSITORY}/issues/new`;
  const make = (body: string) => `${base}?${new URLSearchParams({ title: issueTitle(what, home), body }).toString()}`;
  const attach = savedAt ? `\n\n_The full report${report.includes('screenshot.png') ? ' and its screenshot are' : ' is'} saved at \`${savedAt}\`: drag ${report.includes('screenshot.png') ? 'them' : 'it'} here to attach._` : '';
  let url = make(report + attach);
  if (url.length <= ISSUE_URL_LIMIT) return url;
  const note = `\n\n_…cut to fit a link._${attach || '\n\n_Use Save to file in Lazybox to keep the whole report, then attach it here._'}`;
  let keep = report.length;
  while (keep > 0 && url.length > ISSUE_URL_LIMIT) {
    keep = Math.floor(keep * 0.85);
    let body = report.slice(0, keep);
    // A cut inside the log's code block closes it.
    if ((body.match(/```/g) || []).length % 2) body += '\n```';
    url = make(body + note);
  }
  return url;
}
