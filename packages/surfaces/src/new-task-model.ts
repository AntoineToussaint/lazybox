/**
 * The New task sheet (⌘N, NewTaskSheet.tsx), pure: what a task is called,
 * which repositories the picker offers first, what the sheet remembers, and
 * whether what is filled in can start. One task is one workspace on its own
 * branch, one conversation, and the first message.
 */
import { fuzzyMatch } from './fuzzy';
import { branchForName, suggestWorkspaceName } from './repository-model';

/** How the agent works: its permissions and whether it plans first. */
export const TASK_MODES = [
  { id: 'default', label: 'Runner default', permission: '', workflow: 'execute' },
  { id: 'ask', label: 'Ask for approval', permission: 'ask', workflow: 'execute' },
  { id: 'plan', label: 'Plan first', permission: 'ask', workflow: 'plan' },
  { id: 'read-only', label: 'Read only', permission: 'read-only', workflow: 'execute' },
  { id: 'yolo', label: 'YOLO · full access', permission: 'yolo', workflow: 'execute' },
] as const;
export type TaskMode = (typeof TASK_MODES)[number]['id'];
export const modeOf = (id: string) => TASK_MODES.find(mode => mode.id === id) || TASK_MODES[0];

/** What the sheet starts with next time: the last repositories, agent, account and mode. */
export type TaskChoices = { identities: string[]; recent: string[]; provider: string; profile: string; mode: TaskMode };
export const NO_CHOICES: TaskChoices = { identities: [], recent: [], provider: '', profile: '', mode: 'default' };
const RECENT = 12;

export function parseTaskChoices(text: string | null | undefined): TaskChoices {
  let value: Record<string, unknown>;
  try { value = JSON.parse(text || 'null') || {}; } catch { return NO_CHOICES; }
  if (typeof value !== 'object' || Array.isArray(value)) return NO_CHOICES;
  const list = (raw: unknown, limit: number) => Array.isArray(raw) ? [...new Set(raw.filter((v): v is string => typeof v === 'string' && v.length > 0 && v.length < 512))].slice(0, limit) : [];
  const text_ = (raw: unknown) => typeof raw === 'string' && raw.length < 256 ? raw : '';
  return {
    identities: list(value.identities, 32), recent: list(value.recent, RECENT),
    provider: text_(value.provider), profile: text_(value.profile),
    mode: TASK_MODES.some(mode => mode.id === value.mode) ? value.mode as TaskMode : 'default',
  };
}

/** The choices after starting a task: its repositories go to the front of the recent ones. */
export function rememberTask(previous: TaskChoices, used: { identities: readonly string[]; provider: string; profile: string; mode: TaskMode }): TaskChoices {
  const identities = [...used.identities];
  return { identities, recent: [...new Set([...identities, ...previous.recent])].slice(0, RECENT), provider: used.provider, profile: used.profile, mode: used.mode };
}

const TRAILING = new Set(['a', 'an', 'the', 'to', 'of', 'for', 'and', 'or', 'in', 'on', 'at', 'by', 'with', 'when', 'that', 'so', 'it', 'is', 'we', 'i', 'but', 'from', 'into', 'as', 'if', 'then']);

/**
 * A task's name from its first message: the first line, cut at a word to
 * about seven words, capitalized, ending on a word that carries meaning
 * ("fix the login redirect loop when the session expires" → "Fix the login
 * redirect loop"). Empty when the message says nothing.
 */
export function nameFromPrompt(prompt: string, limit = 48): string {
  const first = prompt.trim().split('\n').map(part => part.trim()).find(Boolean) || '';
  if (!first) return '';
  // The first sentence: "Load-test the cache. Run 200 requests…" is named "Load-test the cache".
  const line = first.split(/(?<=[.!?])\s+(?=\S)/)[0];
  const words = line.replace(/\s+/g, ' ').split(' ');
  let name = '';
  for (const word of words) {
    const next = name ? `${name} ${word}` : word;
    if (next.length > limit && name) break;
    name = next.slice(0, limit);
    if (name.split(' ').length >= 7) break;
  }
  // A name that stops on "the", "when" or "to" reads cut off: end on a word that carries meaning.
  const parts = name.replace(/[\s.,;:!?…-]+$/, '').split(' ');
  while (parts.length > 2 && TRAILING.has(parts[parts.length - 1].toLowerCase())) parts.pop();
  name = parts.join(' ').replace(/[\s.,;:!?…-]+$/, '');
  return name ? name[0].toUpperCase() + name.slice(1) : '';
}

/**
 * The workspace's name: the person's own, else the Runner's generated one
 * when its setting asks for memorable names, else from the first message,
 * else from the repositories.
 */
export function taskName({ custom, prompt, labels, generated }: { custom?: string; prompt: string; labels: readonly string[]; generated?: string }): string {
  if (custom !== undefined && custom.trim()) return custom.trim().slice(0, 128);
  if (generated) return generated.slice(0, 128);
  return (nameFromPrompt(prompt) || suggestWorkspaceName(labels)).slice(0, 128);
}

/** The branch: the person's own, else the one the name suggests. */
export function taskBranch(name: string, custom?: string): string {
  return (custom !== undefined ? custom : branchForName(name)).trim();
}

type Entry = { identity: string; name: string };
/**
 * The repositories the picker lists for a query: without one, the recent ones
 * first (in the order last used), then the rest by name; with one, the best
 * fuzzy matches, a recent one winning a tie. Already chosen ones are left out.
 */
export function rankRepositories<T extends Entry>(entries: readonly T[], { query, recent, chosen, limit = 50 }: { query: string; recent: readonly string[]; chosen: ReadonlySet<string>; limit?: number }): { entry: T; recent: boolean; positions: number[] }[] {
  const rank = new Map(recent.map((identity, index) => [identity, index]));
  const open = entries.filter(entry => !chosen.has(entry.identity));
  const q = query.trim();
  if (!q) {
    return [...open].sort((a, b) => (rank.get(a.identity) ?? Infinity) - (rank.get(b.identity) ?? Infinity) || a.name.localeCompare(b.name, undefined, { sensitivity: 'base' }))
      .slice(0, limit).map(entry => ({ entry, recent: rank.has(entry.identity), positions: [] }));
  }
  return open.map(entry => ({ entry, match: fuzzyMatch(q, entry.name) || fuzzyMatch(q, entry.identity.replace(/^github:/, '')) }))
    .filter((row): row is { entry: T; match: NonNullable<typeof row.match> } => !!row.match)
    .sort((a, b) => b.match.score - a.match.score || (rank.get(a.entry.identity) ?? Infinity) - (rank.get(b.entry.identity) ?? Infinity) || a.entry.name.localeCompare(b.entry.name))
    .slice(0, limit).map(row => ({ entry: row.entry, recent: rank.has(row.entry.identity), positions: fuzzyMatch(q, row.entry.name)?.positions || [] }));
}

/** Why the task cannot start yet, and which field to fix; undefined when it can. */
export type TaskProblem = { field: 'repositories' | 'agent' | 'name' | 'branch'; message: string };
export function taskProblem({ identities, empty, profileId, name, branch, ready }: { identities: readonly string[]; empty: boolean; profileId: string; name: string; branch: string; ready: boolean }): TaskProblem | undefined {
  if (!identities.length && !empty) return { field: 'repositories', message: 'Choose a repository, or start without one.' };
  if (!ready) return { field: 'agent', message: 'Connecting to the Runner…' };
  if (!profileId) return { field: 'agent', message: 'This agent has no account on this Runner. Add one in Settings → Subscriptions.' };
  if (!name.trim()) return { field: 'name', message: 'Name the workspace.' };
  if (!empty && branch && !/^[A-Za-z0-9._/-]+$/.test(branch)) return { field: 'branch', message: 'A branch name has letters, digits, and . _ / - only.' };
  if (!empty && /(^[/.]|[/.]$|\.\.|\/\/|@\{|\.lock$)/.test(branch)) return { field: 'branch', message: 'That is not a valid branch name.' };
  return undefined;
}
