/**
 * Cards for Settings → Process, from the effective orchestration process the
 * Runner sends as JSON (the shape of the published process.v1.json schema).
 * Read-only: the editor comes with the transition agent (ORCHESTRATION.md §7, slice 4).
 */

type Budget = { usd?: number; wall?: string; turns?: number; retries?: number };
type Trigger = { on: string | string[]; if?: string; mode?: string; every?: string; after?: string };
type Role = { provider?: string | string[]; model?: string; effort?: string; permission?: string; scopes?: string[]; budget?: Budget; triggers?: Trigger[]; concurrency?: number };
type Gate = { kind?: string; when?: string; show?: string[]; role?: string };
type ProcessJSON = { name?: string; autonomy?: number; budgets?: { mission?: Budget; task?: Budget; on_exhausted?: string }; phases?: { id: string; exit: string }[]; gates?: Record<string, Gate>; roles?: Record<string, Role>; rules?: unknown[] };

export type PhaseCard = { id: string; exit: string; gate: string; detail: string };
export type RoleCard = { name: string; providers: string; model: string; permission: string; scopes: string[]; wakes: string[]; budget: string };
export type ProcessCards = { name: string; autonomy: number; budget: string; phases: PhaseCard[]; roles: RoleCard[]; rules: number };

const list = (value?: string | string[]) => (Array.isArray(value) ? value : value ? [value] : []);

/** "$20 · 10h · 80 turns · 1 retry". */
export function formatBudget(b?: Budget): string {
  if (!b) return '';
  return [b.usd ? `$${b.usd}` : '', b.wall || '', b.turns ? `${b.turns} turns` : '', b.retries ? `${b.retries} ${b.retries === 1 ? 'retry' : 'retries'}` : ''].filter(Boolean).join(' · ');
}

const permissions: Record<string, string> = { read_only: 'Read only', ask: 'Ask', yolo: 'Full access' };

function wake(t: Trigger): string {
  const on = list(t.on).join(', ');
  const when = t.every ? ` every ${t.every}` : t.after ? ` after ${t.after}` : '';
  return `${t.mode === 'manual' ? 'offered on' : 'on'} ${on}${when}${t.if ? ' if …' : ''}`;
}

/** Phases left to right with the gate that ends each, and roles in file order. */
export function processCards(json: string): ProcessCards | undefined {
  let p: ProcessJSON;
  try { p = JSON.parse(json); } catch { return undefined; }
  if (!p || typeof p !== 'object') return undefined;
  const gates = p.gates || {};
  return {
    name: p.name || 'process',
    autonomy: p.autonomy || 0,
    budget: formatBudget(p.budgets?.mission),
    rules: p.rules?.length || 0,
    phases: (p.phases || []).map(phase => {
      const gate = gates[phase.exit] || {};
      const detail = gate.kind === 'human' ? `you check ${(gate.show || []).join(', ').replaceAll('_', ' ') || 'it'}` : gate.kind === 'agent' ? `${gate.role} judges` : gate.when || '';
      return { id: phase.id, exit: phase.exit, gate: gate.kind || '', detail };
    }),
    roles: Object.entries(p.roles || {}).map(([name, r]) => ({
      name,
      providers: list(r.provider).map(x => x.startsWith('other_than_') ? `not ${x.slice(11)}'s` : x).join(' / '),
      model: [r.model, r.effort].filter(Boolean).join(' · '),
      permission: permissions[r.permission || 'read_only'] || r.permission || '',
      scopes: r.scopes || [],
      wakes: (r.triggers || []).map(wake),
      budget: formatBudget(r.budget),
    })),
  };
}
