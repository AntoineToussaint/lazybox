/**
 * "Run in <repo> home": the commands an agent hands back to the person — a
 * login, a long-lived dev server, a manual test — and where to run them.
 * A repository's home checkout is the person's own clone outside every
 * workspace (WORKSPACES.md, "Home checkouts"); a login that belongs to no
 * repository runs in their home folder.
 *
 * Pure: `code-runs.tsx` offers the buttons, `home-shell.ts` opens the shell,
 * `scripts/home-run.test.ts` pins it. Best effort and quiet: the agent's
 * briefing asks it to write "run in the home checkout of api: `make
 * playground`" or "run in your home folder: `gcloud auth login`", which is
 * read for certain; otherwise only a command the message plainly hands to
 * the person ("run … yourself", "you'll need to run …", a login, something
 * that needs a terminal) is offered.
 *
 * The command is kept exactly as written, environment changes included
 * (`env -u VAR ./bin/tool login prod`). Nothing here runs anything: the
 * window runs it in a shell of the Terminal panel (or types it at the
 * prompt for review, `shell-input.ts`), a real terminal, so logins can prompt. Only a
 * leading `cd` into another absolute folder (`cd /tmp/api-main && …`, a
 * temporary checkout) is left out: the home checkout is what it stood for.
 */
import { isShellLanguage, shellScript, standaloneCommand } from './code-run';

/** Where an offer runs: a repository's home (by the name the message used), the person's home folder, or wherever fits. */
export type HomeTarget = { kind: 'repository'; repository: string } | { kind: 'user' } | { kind: 'any' };
export type HomeOffer = {
  command: string; target: HomeTarget;
  /** The message named the place in the briefing's words. */
  explicit: boolean;
  /** A repository the command's own `cd` names (`cd /tmp/api-main` → api), to place it when the message does not. */
  hint?: string;
};

const REPOSITORY_HOME = /\brun\b[^.\n`]{0,40}?\bin (?:the )?home(?: checkout)? (?:of|for) \*{0,2}`?([\w.-]+(?:\/[\w.-]+)?)`?\*{0,2}\s*:?\s*$/i;
const NAMED_HOME = /\brun\b[^.\n`]{0,40}?\bin (?:the )?\*{0,2}`?([\w.-]+(?:\/[\w.-]+)?)`?\*{0,2} home(?: checkout)?\s*:?\s*$/i;
const USER_HOME = /\brun\b[^.\n`]{0,40}?\bin your home (?:folder|directory)\s*:?\s*$/i;
/** Words that hand a command to the person. */
const YOURSELF = /\b(yourself|you(?:'ll| will)? (?:need|have|want) to run|you (?:can|should|must) run|please run|on your (?:machine|computer|side)|in (?:your (?:own )?|a (?:real )?)terminal|needs? (?:a )?(?:real )?(?:TTY|tty|terminal)|browser sign-?in|run (?:it|this|these) (?:yourself|locally|manually)|to run it yourself)\b/i;
/** A follow-up for later ("`make stop` when done"), not a command to run now. */
const LATER = /^[^.\n]{0,12}?\b(?:when|once|after) (?:you(?:'re| are) )?(?:done|finished)\b/i;
/** Logins and credential setup: the person's to run, whatever the wording. */
const LOGIN = /(?:^|\s)(?:login|logout|log-in|auth(?:\s+\S+)*\s+login|auth\s+application-default\s+login|sso\s+login|signin|sign-in|configure\s+sso)(?:\s|$)/i;
/** Tools whose login is the person's, not a repository's: their home folder fits. */
const GLOBAL_TOOLS = new Set(['gcloud', 'aws', 'az', 'gh', 'docker', 'npm', 'firebase', 'vercel', 'flyctl', 'fly', 'heroku', 'netlify', 'wrangler', 'doctl', 'op', 'vault', 'kubectl', 'claude', 'codex']);

/** A leading `cd DIR &&` (or `;`): DIR, then the rest. */
const LEADING_CD = /^cd\s+("[^"]+"|'[^']+'|\S+)\s*(?:&&|;)\s*/;
/**
 * The command a line runs, without what only sets it up: a leading `cd DIR
 * &&`, `env` and its options (`-u VAR`, `-i`, `VAR=value`) and plain
 * assignments. `env -u TOKEN ./bin/tool login prod` runs `./bin/tool login prod`.
 */
export function commandCore(command: string): string {
  let text = command.trim().replace(/\s+/g, ' ');
  for (;;) {
    const next = text.replace(LEADING_CD, '')
      .replace(/^env(?:\s+(?:-u\s+\S+|--unset[= ]\S+|-i|--ignore-environment|-|[A-Za-z_]\w*=\S*))*\s+/, '')
      .replace(/^(?:[A-Za-z_]\w*=\S*\s+)+/, '');
    if (next === text) return text;
    text = next;
  }
}
/** A login or credential setup (`acme login`, `gcloud auth application-default login`). */
export const isLogin = (command: string) => LOGIN.test(' ' + commandCore(command) + ' ');
/** Prompts and sign-ins that only a real terminal can answer, beside a plain login. */
const INTERACTIVE = /(?:^|\s)(?:auth|sign-?in|sign\s+in|--no-launch-browser|--device-code|--interactive|passwd)(?:\s|=|$)/i;
/**
 * A script that needs a terminal: a login, a sign-in, a prompt. It cannot
 * run through the agent (its commands have no TTY) or as captured command
 * output; only in the shell panel or the person's own terminal.
 */
export function needsTerminal(script: string): boolean {
  return script.split('\n').some(line => { const core = commandCore(line); return !!core && (isLogin(line) || INTERACTIVE.test(' ' + core + ' ')); });
}
/** A login of a tool that belongs to no repository: it runs in the home folder unless the message says where. */
export function globalLogin(command: string): boolean {
  const program = commandCore(command).split(/\s+/)[0] || '';
  return isLogin(command) && GLOBAL_TOOLS.has(program.split('/').pop() || '');
}

/** An inline span the person could type, kept exactly: a known command, a repository's own tool (`./bin/tool …`) or `<tool> login|auth|setup …`, after its setup (`cd`, `env -u VAR`). */
function inlineCommandText(code: string): string {
  const text = code.trim().replace(/\s+/g, ' ');
  if (!text || text.length > 300 || /[\n<>]/.test(text) && !/2>&1/.test(text)) return '';
  const core = commandCore(text);
  if (!core) return '';
  if (standaloneCommand(core)) return text;
  if (/^[a-z][\w.-]{1,30} (?:login|logout|auth|setup|init|configure|signin)\b/.test(core)) return text;
  return '';
}

/**
 * A command as it runs in a home: the same, less a leading `cd` into another
 * absolute folder (a temporary checkout the home replaces). A `cd` into a
 * folder of the checkout itself (`cd parser && …`) is kept.
 */
export function inHome(command: string): { command: string; dropped: string } {
  const match = LEADING_CD.exec(command.trimStart());
  if (!match) return { command, dropped: '' };
  const dir = match[1].replace(/^["']|["']$/g, '');
  if (!dir.startsWith('/') && !dir.startsWith('~')) return { command, dropped: '' };
  return { command: command.trimStart().slice(match[0].length), dropped: dir };
}
/** The repository a temporary checkout's folder is named after: `/tmp/api-main` → `api`. */
function cdHint(command: string): string {
  const match = LEADING_CD.exec(command.trimStart());
  if (!match) return '';
  const base = match[1].replace(/^["']|["']$/g, '').replace(/\/+$/, '').split('/').pop() || '';
  return base.replace(/[-_.](?:main|master|tmp|temp|latest|head|checkout|clone)$/i, '');
}

/** What a phrase before a command says about where it runs, or undefined when it hands nothing to the person. */
function targetOf(before: string, command: string): { target: HomeTarget; explicit: boolean } | undefined {
  const repository = REPOSITORY_HOME.exec(before) || NAMED_HOME.exec(before);
  if (repository && !/^(?:your|the|my|a)$/i.test(repository[1])) return { target: { kind: 'repository', repository: repository[1] }, explicit: true };
  if (USER_HOME.test(before)) return { target: { kind: 'user' }, explicit: true };
  if (YOURSELF.test(before) || isLogin(command)) return { target: globalLogin(command) ? { kind: 'user' } : { kind: 'any' }, explicit: false };
  return undefined;
}

const FENCE = /^([ \t]*)(`{3,}|~{3,})[ \t]*([\w-]*)[^\n]*\n([\s\S]*?)\n[ \t]*\2[`~]*[ \t]*$/gm;

/**
 * The commands a message hands to the person, at most `limit`, in order: an
 * inline command with the phrase before it in its line or sentence, and a
 * fenced shell block with the paragraph before it.
 */
export function homeOffers(markdown: string, limit = 3, standaloneInline = false): HomeOffer[] {
  const out: HomeOffer[] = [];
  const seen = new Set<string>();
  const add = (command: string, found: { target: HomeTarget; explicit: boolean } | undefined) => {
    if (!command || !found || seen.has(command) || out.length >= limit) return;
    seen.add(command);
    const hint = cdHint(command);
    out.push({ command, ...found, ...(hint ? { hint } : {}) });
  };
  // Where the fenced blocks are, so inline code inside them is skipped.
  const fences: [number, number][] = [...markdown.matchAll(FENCE)].map(match => [match.index!, match.index! + match[0].length]);
  const items: { at: number; run(): void }[] = [];
  for (const match of markdown.matchAll(FENCE)) {
    const language = match[3];
    if (!isShellLanguage(language)) continue;
    const script = shellScript(match[4].replace(new RegExp(`^[ \\t]{0,${match[1].length}}`, 'gm'), ''), language);
    if (!script) continue;
    // The paragraph before the fence, and its last line.
    const before = markdown.slice(0, match.index!).replace(/\s+$/, '').split(/\n\s*\n/).pop() || '';
    const lastLine = before.split('\n').pop() || '';
    items.push({ at: match.index!, run: () => add(script, targetOf(lastLine, script) || (YOURSELF.test(before) ? { target: globalLogin(script) ? { kind: 'user' } : { kind: 'any' }, explicit: false } : undefined)) });
  }
  for (const match of markdown.matchAll(/(?<!`)`([^`\n]+)`(?!`)/g)) {
    const at = match.index!;
    if (fences.some(([from, to]) => at >= from && at < to)) continue;
    const command = inlineCommandText(match[1]);
    if (!command || LATER.test(markdown.slice(at + match[0].length, at + match[0].length + 40))) continue;
    // The text before it on its line, from the sentence it is in.
    const line = markdown.slice(markdown.lastIndexOf('\n', at - 1) + 1, at);
    const sentence = line.split(/(?<=[.!?])\s+(?=[A-Z])/).pop() || line;
    const previous = standaloneInline && !line.trim() && /^\s*$/.test(markdown.slice(at + match[0].length).split('\n')[0])
      ? markdown.slice(0, at).trimEnd().split('\n').at(-1) || '' : '';
    items.push({ at, run: () => add(command, targetOf(sentence || previous, command)) });
  }
  for (const item of items.sort((a, b) => a.at - b.at)) item.run();
  return out;
}

/** An explicit destination also applies to the command's own Run button. */
export function explicitHomeTarget(markdown: string, command: string): HomeTarget | undefined {
  if (!/\bhome\b/i.test(markdown)) return undefined;
  return homeOffers(markdown, Infinity).find(offer => offer.explicit && offer.command === command)?.target;
}

/** Paste compound sign-in commands for review before any step can run. */
export const reviewTerminalCommand = (command: string): boolean =>
  needsTerminal(command) && /&&|\|\||;/.test(command);

/** A workspace repository and its home, as the chat knows it. */
export type HomeRepository = { repositoryId: string; name: string; identity: string; home: string };

/** The repository a name the agent wrote stands for: its folder name, `repo` or `owner/repo`, any case. */
export function matchRepository(repositories: readonly HomeRepository[], name: string): HomeRepository | undefined {
  const wanted = name.trim().toLowerCase().replace(/\.git$/, '');
  return repositories.find(r => r.name.toLowerCase() === wanted)
    || repositories.find(r => { const full = r.identity.replace(/^github:/, '').toLowerCase(); return full === wanted || full.split('/').pop() === wanted; });
}

/** Where an offer runs: `user` (the home folder), a repository (with or without a home yet), or nowhere this workspace knows. */
export type HomePlace = { kind: 'user' } | { kind: 'repository'; repository: HomeRepository } | { kind: 'unknown'; name: string };
export function placeFor(target: HomeTarget, repositories: readonly HomeRepository[], hint = ''): HomePlace {
  if (target.kind === 'user') return { kind: 'user' };
  if (target.kind === 'repository') {
    const found = matchRepository(repositories, target.repository);
    return found ? { kind: 'repository', repository: found } : { kind: 'unknown', name: target.repository };
  }
  const hinted = hint ? matchRepository(repositories, hint) : undefined;
  if (hinted) return { kind: 'repository', repository: hinted };
  const homed = repositories.filter(r => r.home);
  if (homed.length) return { kind: 'repository', repository: homed[0] };
  if (repositories.length) return { kind: 'repository', repository: repositories[0] };
  return { kind: 'user' };
}

/** The button's words: "Run in api home", "Run in your home folder". */
export function placeLabel(place: HomePlace): string {
  return place.kind === 'user' ? 'Run in your home folder' : place.kind === 'repository' ? `Run in ${place.repository.name} home` : `Run in ${place.name} home`;
}

/** What "Tell <agent> it's done" sends: the command, and its exit status when the shell said. */
export function doneMessage(command: string, exitCode?: number): string {
  const how = exitCode === 0 ? 'completed successfully' : exitCode === undefined ? 'finished' : `failed (exit ${exitCode})`;
  if (!command.includes('\n') && !command.includes('`')) return `\`${command}\` ${how}.`;
  const fence = command.includes('```') ? '````' : '```';
  return `The command ${how}.\n\n${fence}sh\n${command}\n${fence}`;
}