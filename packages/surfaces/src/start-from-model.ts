/**
 * "Start from" (StartFrom.tsx), pure: which branch a repository's new
 * workspace starts from, how the chip says it, and the order the picker
 * offers origin's branches in — the default, then the ones chosen lately,
 * then the most active, then all of them by name; typing filters them, and a
 * name origin may have that is not listed can still be chosen.
 */
import { fuzzyMatch } from './fuzzy';

/** What the Runner says about one repository's branches (RepositoryBranches). */
export type BranchesInfo = {
  defaultBranch: string;
  startFrom: string;
  recent: readonly string[];
  branches: readonly { name: string; committedAtUnixMillis: bigint | number | string }[];
  unregistered?: boolean;
};

/**
 * The branch a repository starts from: the person's choice in this sheet,
 * else the one the Runner remembers for it; "" is origin's default branch.
 */
export function effectiveStart(chosen: string | undefined, info: Pick<BranchesInfo, 'startFrom'> | undefined): string {
  return chosen ?? info?.startFrom ?? '';
}

/** The branch's name as the chip shows it: the default by its name when known. */
export function startLabel(branch: string, info: Pick<BranchesInfo, 'defaultBranch'> | undefined): string {
  return branch || info?.defaultBranch || 'default branch';
}

/** Whether a start is origin's default branch (said quietly) or another (shown plainly). */
export function isDefaultStart(branch: string, info: Pick<BranchesInfo, 'defaultBranch'> | undefined): boolean {
  return !branch || branch === info?.defaultBranch;
}

export type BranchSection = 'default' | 'recent' | 'active' | 'all' | 'typed';
export type BranchOption = { name: string; section: BranchSection; positions: number[] };

/** How many of the most recently committed branches the picker shows as active. */
export const ACTIVE = 5;

const time = (value: bigint | number | string) => Number(value) || 0;

/**
 * The picker's rows. Without a query: origin's default branch, the branches
 * chosen lately, the most recently committed, then all the others by name.
 * With one: the matches, best first, and the typed name itself when no branch
 * is called that (the Runner checks it with origin when the workspace is made).
 */
export function rankBranches(info: BranchesInfo | undefined, query = '', limit = 200): BranchOption[] {
  const names = new Set<string>();
  const out: BranchOption[] = [];
  const add = (name: string, section: BranchSection, positions: number[] = []) => {
    if (!name || names.has(name)) return;
    names.add(name);
    out.push({ name, section, positions });
  };
  const branches = info?.branches || [];
  const known = new Set(branches.map(branch => branch.name));
  const q = query.trim();
  if (!q) {
    if (info?.defaultBranch) add(info.defaultBranch, 'default');
    // A remembered branch origin no longer lists is not offered again.
    for (const name of info?.recent || []) if (!branches.length || known.has(name)) add(name, 'recent');
    const byActivity = [...branches].filter(branch => time(branch.committedAtUnixMillis) > 0).sort((a, b) => time(b.committedAtUnixMillis) - time(a.committedAtUnixMillis) || a.name.localeCompare(b.name));
    for (const branch of byActivity) { if (out.filter(row => row.section === 'active').length >= ACTIVE) break; add(branch.name, 'active'); }
    for (const branch of [...branches].sort((a, b) => a.name.localeCompare(b.name))) add(branch.name, 'all');
    return out.slice(0, limit);
  }
  const rank = (name: string) => name === info?.defaultBranch ? 0 : (info?.recent || []).includes(name) ? 1 : 2;
  const all = [...new Set([...(info?.defaultBranch ? [info.defaultBranch] : []), ...(info?.recent || []), ...branches.map(branch => branch.name)])];
  all.map(name => ({ name, match: fuzzyMatch(q, name) }))
    .filter((row): row is { name: string; match: NonNullable<typeof row.match> } => !!row.match)
    .sort((a, b) => b.match.score - a.match.score || rank(a.name) - rank(b.name) || a.name.localeCompare(b.name))
    .forEach(row => add(row.name, row.name === info?.defaultBranch ? 'default' : (info?.recent || []).includes(row.name) ? 'recent' : 'all', row.match.positions));
  if (!names.has(q) && validBranchName(q)) out.push({ name: q, section: 'typed', positions: [] });
  return out.slice(0, limit);
}

/** A name Git could take as a branch (the Runner checks it for real). */
export function validBranchName(name: string): boolean {
  return /^[A-Za-z0-9._/+-]+$/.test(name) && !/(^[/.-]|[/.]$|\.\.|\/\/|@\{|\.lock$)/.test(name) && name !== 'HEAD' && name.length <= 255;
}

/**
 * The start_from map a preparation sends: only what the person chose in this
 * sheet, for the repositories it creates. A repository not named starts from
 * its remembered branch, which the Runner applies itself.
 */
export function startsFor(identities: readonly string[], chosen: Readonly<Record<string, string>>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const identity of identities) if (chosen[identity] !== undefined) out[identity] = chosen[identity];
  return out;
}
