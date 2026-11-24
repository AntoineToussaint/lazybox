/**
 * What a conversation's tokens are made of and what they cost, as the
 * providers report them: uncached input, cache reads, cache writes, output.
 * A cost is an API-equivalent estimate; what has no price says which model
 * and why, never "+ unpriced".
 */
import type { TokenCosts, TokenCounts, UsageGroup } from '@lazybox/protocol';

export type UsagePartKey = 'input' | 'cacheRead' | 'cacheWrite' | 'output';
export type UsagePart = {
  key: UsagePartKey; label: string;
  /** What the part is, on hover. */
  hint: string;
  /** Tokens, all of them, priced or not; undefined when never reported. */
  tokens?: bigint;
  /** Nanodollars, of the priced tokens; undefined when not known by part. */
  nanos?: bigint;
};
/** `tokens` is undefined when the Runner did not say how many (one from before it did). */
export type Unpriced = { model: string; agent: string; reasons: string[]; tokens?: bigint; records: number };
export type UsageBreakdown = {
  parts: UsagePart[];
  /** Every token reported. */
  tokens: bigint;
  /** Nanodollars of the priced usage; undefined when nothing is priced. */
  nanos?: bigint;
  /** Every priced group said what each part cost. */
  partsKnown: boolean;
  /** Usage without a price: which model, why, how many tokens. */
  unpriced: Unpriced[];
  /** Requests whose usage was never fully reported. */
  incomplete: number;
  /** Cache reads as a share of all input (0–1), when there was input. */
  cached?: number;
};

const PARTS: { key: UsagePartKey; label: string; hint: string }[] = [
  { key: 'input', label: 'Input', hint: 'Input tokens read without the cache' },
  { key: 'cacheRead', label: 'Cache read', hint: 'Input served from the prompt cache, at a fraction of the input price' },
  { key: 'cacheWrite', label: 'Cache write', hint: 'Input written to the prompt cache (5-minute and 1-hour), priced above input' },
  { key: 'output', label: 'Output', hint: 'Tokens the model wrote, reasoning included' },
];

const add = (a: bigint | undefined, b: bigint | undefined) => b === undefined ? a : (a ?? 0n) + b;
function part(values: TokenCounts | TokenCosts | undefined, key: UsagePartKey): bigint | undefined {
  if (!values) return;
  if (key === 'cacheWrite') return values.cacheWrite5m === undefined && values.cacheWrite1h === undefined ? undefined : (values.cacheWrite5m ?? 0n) + (values.cacheWrite1h ?? 0n);
  return values[key];
}
const tokenSum = (t: TokenCounts | undefined) => PARTS.reduce((sum, p) => sum + (part(t, p.key) ?? 0n), 0n);

/** The usage of a conversation (or workspace) by part, with what is not priced and why. */
export function usageBreakdown(groups: readonly UsageGroup[]): UsageBreakdown {
  const parts: UsagePart[] = PARTS.map(p => ({ ...p }));
  let nanos: bigint | undefined, partsKnown = true, incomplete = 0;
  const unpriced: Unpriced[] = [];
  for (const g of groups) {
    for (const p of parts) p.tokens = add(p.tokens, part(g.tokens, p.key));
    if (g.nanos !== undefined) {
      nanos = add(nanos, g.nanos);
      if (!g.costs) partsKnown = false;
      else for (const p of parts) p.nanos = add(p.nanos, part(g.costs, p.key) ?? 0n);
    }
    incomplete += Number(g.incomplete);
    if (Number(g.unpriced) > 0) {
      // A group priced in part counts only its unpriced tokens; a group with no price, all of them.
      const tokens = g.nanos === undefined ? tokenSum(g.tokens) : g.unpricedTokens ? tokenSum(g.unpricedTokens) : undefined;
      const reasons = g.unpricedReasons.length ? g.unpricedReasons : ['No price'];
      const model = g.model || 'Model unavailable', agent = g.agent && g.agent !== 'root' ? g.agent : '';
      const same = unpriced.find(u => u.model === model && u.agent === agent);
      if (same) { same.tokens = same.tokens === undefined || tokens === undefined ? undefined : same.tokens + tokens; same.records += Number(g.unpriced); for (const r of reasons) if (!same.reasons.includes(r)) same.reasons.push(r); }
      else unpriced.push({ model, agent, reasons: [...reasons], tokens, records: Number(g.unpriced) });
    }
  }
  if (nanos === undefined || !partsKnown) for (const p of parts) p.nanos = undefined;
  const tokens = parts.reduce((sum, p) => sum + (p.tokens ?? 0n), 0n);
  const input = (parts[0].tokens ?? 0n) + (parts[1].tokens ?? 0n) + (parts[2].tokens ?? 0n);
  return { parts, tokens, nanos, partsKnown: nanos !== undefined && partsKnown, unpriced, incomplete, ...(input > 0n ? { cached: Number(parts[1].tokens ?? 0n) / Number(input) } : {}) };
}

/** "$0.1234", "$12.30", "<$0.0001": a cost to the precision it needs. */
export function dollars(nanos: bigint | undefined): string {
  if (nanos === undefined) return '—';
  const value = Number(nanos) / 1e9;
  if (value === 0) return '$0';
  if (value < 0.0001) return '<$0.0001';
  return `$${value >= 1 ? value.toFixed(2) : value.toFixed(4)}`;
}

/** Why some usage has no price, in a sentence: "claude-x: No exact model price". */
export function unpricedLine(u: Unpriced): string {
  return `${u.model}${u.agent ? ` (${u.agent})` : ''}: ${u.reasons.join('; ')}`;
}

/** A reason a person can act on says how. */
export function unpricedHelp(reasons: readonly string[]): string {
  if (reasons.some(r => /no exact model price|incomplete rate card|long-context/i.test(r))) return 'Add a price for the model in Settings → Pricing.';
  if (reasons.some(r => /cumulative/i.test(r))) return 'The provider reports a running total for the session, not each request, so no one model’s price applies.';
  return '';
}

/**
 * How full a context window is, for its colour: normal, a warning from 60%,
 * danger from 85%. The chat header's meter and the message box's say the same.
 */
export type ContextLevel = 'normal' | 'warn' | 'danger';
export const CONTEXT_WARN_PERCENT = 60, CONTEXT_DANGER_PERCENT = 85;
export function contextLevel(percent: number): ContextLevel {
  return percent >= CONTEXT_DANGER_PERCENT ? 'danger' : percent >= CONTEXT_WARN_PERCENT ? 'warn' : 'normal';
}

/** "40.4%", "<0.1%", "—": a context percentage as the header and the message box print it. */
export function percentLabel(n?: number): string {
  return n === undefined ? '—' : `${n > 0 && n < 0.1 ? '<0.1' : n.toFixed(1).replace(/\.0$/, '')}%`;
}

/**
 * What a usage read is about: a workspace, or an execution. The execution's
 * conversation becoming known only widens the read; it is not another subject.
 */
export const usageSubject = (workspaceId?: string, executionId?: string) => `${workspaceId || ''}\0${executionId || ''}`;

/**
 * The report to show for `subject`: the one read for it, or none. A report kept
 * from another workspace or execution is never shown or published, not even in
 * the render in which the subject changes; one read for the same execution
 * stays while a read widened to its conversation is out.
 */
export function reportFor<T>(read: { subject: string; report: T } | undefined, subject: string): T | undefined {
  return read?.subject === subject ? read.report : undefined;
}
