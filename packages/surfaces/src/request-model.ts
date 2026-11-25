/**
 * An agent's question or approval request, read from its provider data into
 * one shape for Claude and Codex: the questions with their options, or what
 * will run, where, what it changes, how risky it looks, and the "always
 * allow" choices the provider supports.
 */
import { changeOf, shortPath } from './tool-lines';

export type QuestionOption = { label: string; description?: string; preview?: string };
export type Question = {
  /** The answer's key: Codex's question id, Claude's question text. */
  key: string;
  header: string;
  text: string;
  options: QuestionOption[];
  multi: boolean;
  /** A free-text answer is accepted beside the options. */
  other: boolean;
  secret: boolean;
};
export type AlwaysChoice = { id: string; scope: string; label: string; detail?: string };
export type Risk = { kind: 'delete' | 'network' | 'outside' | 'root' | 'discard' | 'remote-code' | 'publish'; text: string };
export type Approval = {
  tool: string;
  /** What the agent says it is doing ("Run the test suite"). */
  description: string;
  /** Claude's prompt sentence, when given. */
  title: string;
  command: string;
  cwd: string;
  file: string;
  /** A unified diff, for file edits. */
  patch: string;
  added: number;
  removed: number;
  risks: Risk[];
  always: AlwaysChoice[];
  /** Approving must not be one stray keystroke (Claude's defaultToNo). */
  defaultToNo: boolean;
  /** Codex permission-subset grants cannot be accepted here. */
  acceptUnsupported: boolean;
  input: unknown;
};

const object = (value: unknown): Record<string, any> => value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, any> : {};
const text = (value: unknown) => typeof value === 'string' ? value : '';

export function questionsOf(data: Record<string, any>): Question[] {
  const list: unknown[] = Array.isArray(data.questions) ? data.questions : Array.isArray(data.input?.questions) ? data.input.questions : [];
  const codex = Array.isArray(data.questions);
  return list.map((raw, index) => {
    const q = object(raw);
    const options = (Array.isArray(q.options) ? q.options : []).map((o: unknown) => {
      const option = object(o);
      return { label: text(option.label) || String(o), description: text(option.description) || undefined, preview: text(option.preview) || text(option.markdown) || undefined };
    }).filter((o: QuestionOption) => o.label);
    const question = text(q.question) || text(q.header) || `Question ${index + 1}`;
    return {
      key: text(q.id) || question,
      header: text(q.header),
      text: question,
      options,
      multi: !!(q.multiSelect ?? q.multi_select ?? q.multiple),
      // Claude always lets the person type their own answer; Codex says so per question.
      other: !options.length || (codex ? q.isOther !== false : true),
      secret: !!q.isSecret,
    };
  });
}

const NETWORK = /\b(curl|wget|ssh|scp|rsync|nc|ncat|telnet|ftp|sftp|http|https?:\/\/)\b|\bgit\s+(push|fetch|pull|clone)\b|\b(npm|pnpm|yarn|bun)\s+(install|add|i)\b|\bpip3?\s+install\b|\bbrew\s+install\b|\bcargo\s+(install|add)\b|\bgo\s+get\b|\bdocker\s+(pull|push)\b/;

/** What could make a command or edit worth a second look, in plain words. */
export function risksOf(command: string, paths: string[], root: string, extra: { network?: string; outside?: string } = {}): Risk[] {
  const risks: Risk[] = [];
  const add = (risk: Risk) => { if (!risks.some(r => r.kind === risk.kind)) risks.push(risk); };
  if (command) {
    if (/\brm\s+(-[a-z]*r[a-z]*f|-[a-z]*f[a-z]*r|-r\b|-R\b|--recursive)/i.test(command) || /\bfind\b.*\s-delete\b/.test(command)) add({ kind: 'delete', text: 'Deletes files recursively' });
    if (/(^|[;&|]\s*)sudo\b/.test(command)) add({ kind: 'root', text: 'Runs as root' });
    if (/\b(curl|wget)\b[^|]*\|\s*(ba|z)?sh\b/.test(command)) add({ kind: 'remote-code', text: 'Runs code it downloads' });
    if (/\bgit\s+push\b.*(--force|-f\b)/.test(command) || /\bgit\s+(reset\s+--hard|clean\s+-[a-z]*f|checkout\s+--\s|restore\s)/.test(command)) add({ kind: 'discard', text: 'Can discard work' });
    if (/\b(npm|pnpm|yarn)\s+publish\b|\bcargo\s+publish\b|\bgh\s+(release|pr\s+merge)\b/.test(command)) add({ kind: 'publish', text: 'Publishes' });
    if (NETWORK.test(command)) add({ kind: 'network', text: 'Uses the network' });
    // Codex wraps commands in a login shell. Inspect the script's paths;
    // the shell executable itself is not a requested file.
    const script = command.replace(/^\s*\/(?:usr\/)?bin\/(?:ba|z|da|k)?sh\s+-l?c\s+['"]?/, '');
    for (const match of script.matchAll(/(?:^|\s|=)((?:\/|~\/)[^\s'";|&)]+)/g)) paths.push(match[1]);
  }
  if (extra.network) add({ kind: 'network', text: extra.network });
  const home = /^~\//;
  const outside = [extra.outside, ...paths].filter((p): p is string => !!p && (home.test(p) || p.startsWith('/')) && !(root && (p === root || p.startsWith(root.endsWith('/') ? root : root + '/'))) && !/^\/dev\/null$/.test(p));
  if (outside.length && root) add({ kind: 'outside', text: `Outside the workspace: ${shortPath(outside[0])}` });
  return risks;
}

export function approvalOf(data: Record<string, any>, requestText: string, root: string): Approval {
  const input = data.input ?? data.arguments ?? data;
  const i = object(input);
  const tool = text(data.tool) || text(data.displayName) || (requestText.startsWith('item/') ? requestText.split('/')[1] : requestText) || 'Tool';
  const command = typeof i.command === 'string' ? i.command : Array.isArray(i.command) ? i.command.join(' ') : Array.isArray(i.argv) ? i.argv.join(' ') : text(data.command);
  const cwd = text(data.cwd) || text(i.cwd) || root;
  const file = text(i.file_path) || text(i.notebook_path) || (typeof i.path === 'string' && tool !== 'Bash' ? i.path : '');
  const change = changeOf(tool, input) || (Array.isArray(data.changes) ? changeOf('File changes', data.changes) : undefined);
  const network = data.networkApprovalContext ? `Network access${text(object(data.networkApprovalContext).host) ? ` to ${object(data.networkApprovalContext).host}` : ''}` : tool === 'WebFetch' && typeof i.url === 'string' ? `Fetches ${safeHost(i.url)}` : undefined;
  const paths = [file, ...(change?.diff.files || [])].filter(Boolean);
  return {
    tool,
    description: text(i.description) || text(data.reason) || text(data.description) || text(data.decisionReason),
    title: text(data.title),
    command, cwd, file,
    patch: change?.patch || '',
    added: change?.diff.added || 0,
    removed: change?.diff.removed || 0,
    risks: risksOf(command, paths, root, { network, outside: text(data.blockedPath) || text(data.grantRoot) }),
    always: (Array.isArray(data.always) ? data.always : []).map(object).filter(c => typeof c.id === 'string' && typeof c.label === 'string').map(c => ({ id: c.id, scope: text(c.scope) || 'conversation', label: c.label, detail: text(c.detail) || undefined })),
    defaultToNo: !!data.defaultToNo,
    // Codex asking for more access: a Runner that can grant it says so (grantable).
    acceptUnsupported: requestText === 'item/permissions/requestApproval' && data.grantable !== true,
    input,
  };
}
const safeHost = (url: string) => { try { return new URL(url).host; } catch { return url; } };

/**
 * An approval card's choices, in order, with the keys that pick them: a digit
 * (its place) and a letter (y, a for the first "always", n). A request the
 * agent marks default-to-no (Claude's defaultToNo) is never approved by one
 * key: only Deny keeps a key, so no digit or letter picks an allow row.
 */
export type DecisionRow = { id: string; label: string; detail?: string; tone: 'allow' | 'always' | 'deny'; choice?: AlwaysChoice; digit?: string; key?: string };
export function decisionRows(approval: Pick<Approval, 'always' | 'defaultToNo'>, agentName: string): DecisionRow[] {
  const careful = approval.defaultToNo;
  const rows: DecisionRow[] = [
    { id: 'once', label: 'Allow once', key: 'y', tone: 'allow' },
    ...approval.always.map((choice, i): DecisionRow => ({ id: `always:${choice.id}`, label: `Always allow ${choice.label}`, detail: choice.detail, key: i === 0 ? 'a' : undefined, tone: 'always', choice })),
    { id: 'deny', label: 'Deny', detail: `Tell ${agentName} what to do instead`, key: 'n', tone: 'deny' },
  ];
  return rows.map((row, i) => careful && row.tone !== 'deny' ? { ...row, key: undefined } : careful ? row : { ...row, digit: i < 9 ? String(i + 1) : undefined });
}
/** The row a key picks, if any: its digit or its letter. */
export function decisionForKey(rows: readonly DecisionRow[], key: string): DecisionRow | undefined {
  const lower = key.toLowerCase();
  return rows.find(row => row.digit === key || row.key === lower);
}

/** "this conversation", "this workspace": where an always choice applies. */
export function scopeWords(scope: string): string {
  return ({ conversation: 'in this conversation', workspace: 'in this workspace', prefix: 'in this conversation', host: 'in Codex’s network policy' } as Record<string, string>)[scope] || '';
}

/** What the conversation keeps of a request, to say later how it was answered. */
export type RequestNote =
  | { kind: 'question'; questions: { key: string; header: string; text: string }[] }
  | { kind: 'approval'; tool: string; target: string; always: AlwaysChoice[] };

export function requestNote(type: string, data: Record<string, any>, requestText: string): RequestNote | undefined {
  if (type === 'question') return { kind: 'question', questions: questionsOf(data).map(({ key, header, text }) => ({ key, header, text })) };
  if (type !== 'approval') return;
  const a = approvalOf(data, requestText, '');
  return { kind: 'approval', tool: a.tool, target: (a.command || a.file || a.description).slice(0, 200), always: a.always };
}

/** How a request was answered, in one quiet line of the conversation. */
export type AnswerNote =
  | { kind: 'answered'; from?: string; answers: { question: string; answer: string }[] }
  | { kind: 'always'; tool: string; target: string; label: string; scope: string }
  | { kind: 'declined'; tool: string; target: string; reason: string };

export function answerNote(note: RequestNote | undefined, response: Record<string, any>): AnswerNote | undefined {
  if (response.kind === 'answer') {
    const answers = Object.entries(object(response.answers)).map(([key, values]) => {
      const q = note?.kind === 'question' ? note.questions.find(q => q.key === key) : undefined;
      return { question: q?.header || q?.text || key, answer: (Array.isArray(values) ? values : [values]).join(', ') };
    });
    return answers.length ? { kind: 'answered', answers, ...(response.from ? {from: text(response.from)} : {}) } : undefined;
  }
  if (response.kind !== 'approve') return;
  const tool = note?.kind === 'approval' ? note.tool : 'Request', target = note?.kind === 'approval' ? note.target : '';
  if (response.decision === 'accept' && response.always) {
    const choice = note?.kind === 'approval' ? note.always.find(c => c.id === response.always) : undefined;
    return { kind: 'always', tool, target, label: choice?.label || response.always, scope: choice?.scope || '' };
  }
  if (response.decision === 'decline' || response.decision === 'cancel') return { kind: 'declined', tool, target, reason: text(response.reason) };
  return;
}
