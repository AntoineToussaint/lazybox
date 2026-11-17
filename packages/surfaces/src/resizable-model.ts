import type { ViewStateStore } from '@lazybox/platform';
import { preferenceKey } from './view-state';

/**
 * A divider inside a panel (Resizer.tsx): a file list beside its diff, a
 * settings nav beside its page, a log under its browser. Pure, so the
 * clamping, the keys and what is remembered are tested without a page.
 */
export type SplitSpec = {
  /** Remembered under this name; the CSS variable `--split-<name>` carries the size. */
  name: string;
  /** The resized element is the one before the divider (`start`: a list on the left, a pane on top) or after it (`end`: a tray on the right, a log below). */
  side: 'start' | 'end';
  /** `x` resizes a width (a vertical divider), `y` a height. */
  axis?: 'x' | 'y';
  /** Never narrower than this, so it stays readable. */
  min: number;
  max: number;
  /** What the content beside it always keeps. */
  keep: number;
  /** Dragged below half its minimum (or ← at its minimum, or Enter) it folds to a thin rail that brings it back. */
  collapsible?: boolean;
};
/** `size: null` is the panel's own default, which its CSS sets. */
export type SplitState = { size: number | null; collapsed: boolean };

export const DEFAULT_SPLIT: SplitState = { size: null, collapsed: false };
/** Arrow keys move the divider this far; with Shift, `SPLIT_BIG_STEP`. */
export const SPLIT_STEP = 16, SPLIT_BIG_STEP = 64;
/** The width (or height) of a folded side: a rail to click back open. */
export const SPLIT_RAIL = 12;

/** The largest size: its own maximum, less what the content beside keeps of the room they share. */
export function splitMax(spec: SplitSpec, available?: number): number {
  const room = available === undefined || !Number.isFinite(available) ? spec.max : Math.min(spec.max, Math.floor(available - spec.keep));
  return Math.max(spec.min, room);
}
export function clampSplit(spec: SplitSpec, size: number, available?: number): number {
  return Math.round(Math.min(splitMax(spec, available), Math.max(spec.min, size)));
}

/** A drag that started at `start` pixels, moved `delta` along the axis (right or down is positive). */
export function dragSplit(spec: SplitSpec, from: SplitState, start: number, delta: number, available?: number): SplitState {
  const raw = start + (spec.side === 'start' ? delta : -delta);
  if (spec.collapsible && raw < spec.min / 2) return { size: from.size, collapsed: true };
  return { size: clampSplit(spec, raw, available), collapsed: false };
}

/**
 * What a key on the focused divider does, from `current` (its measured size):
 * the arrows along its axis move it (Shift: further), Home and End go to the
 * minimum and maximum, Enter folds or unfolds a collapsible side. Null: not
 * the divider's key.
 */
export function keySplit(spec: SplitSpec, state: SplitState, current: number, key: string, shift: boolean, available?: number): SplitState | null {
  if (key === 'Home') return { size: spec.min, collapsed: false };
  if (key === 'End') return { size: splitMax(spec, available), collapsed: false };
  if (key === 'Enter') return spec.collapsible ? { size: state.size, collapsed: !state.collapsed } : null;
  const [back, forward] = spec.axis === 'y' ? ['ArrowUp', 'ArrowDown'] : ['ArrowLeft', 'ArrowRight'];
  if (key !== back && key !== forward) return null;
  const moved = (key === forward ? 1 : -1) * (shift ? SPLIT_BIG_STEP : SPLIT_STEP);
  const change = spec.side === 'start' ? moved : -moved;
  if (state.collapsed) return change > 0 ? { size: state.size, collapsed: false } : state;
  // At its minimum a further step folds it away, when it can fold.
  if (change < 0 && spec.collapsible && current <= spec.min) return { size: state.size, collapsed: true };
  return { size: clampSplit(spec, current + change, available), collapsed: false };
}

/** The CSS value for `--split-<name>`; null leaves the panel's own default. */
export function splitCss(spec: SplitSpec, state: SplitState, available?: number): string | null {
  if (spec.collapsible && state.collapsed) return `${SPLIT_RAIL}px`;
  return state.size === null ? null : `${clampSplit(spec, state.size, available)}px`;
}

/** The window's own scope: a panel's store adds `:workspace:<id>` to it, and a divider keeps its size in every workspace. */
export function windowScope(store: ViewStateStore): string {
  const scope = store.scope || store.key;
  const at = scope.indexOf(':workspace:');
  return at < 0 ? scope : scope.slice(0, at);
}
/** Where a divider's size is kept: this window's, and the last one set in any window, which a new window starts from. */
export function splitKeys(store: ViewStateStore, name: string): { window: string; shared: string } {
  return { window: `${windowScope(store)}:split:${name}`, shared: preferenceKey(store, `split.${name}`) };
}

export function parseSplit(spec: SplitSpec, text: string | null): SplitState | null {
  if (!text || text.length > 256) return null;
  try {
    const value = JSON.parse(text);
    if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
    const size = typeof value.size === 'number' && Number.isFinite(value.size) ? clampSplit(spec, value.size) : null;
    return { size, collapsed: !!spec.collapsible && value.collapsed === true };
  } catch { return null; }
}
export function readSplit(store: ViewStateStore | undefined, spec: SplitSpec): SplitState {
  try {
    if (!store) return parseSplit(spec, localStorage.getItem(`lazybox:split.${spec.name}`)) || DEFAULT_SPLIT;
    const keys = splitKeys(store, spec.name);
    return parseSplit(spec, store.getItem(keys.window)) || parseSplit(spec, store.getItem(keys.shared)) || DEFAULT_SPLIT;
  } catch { return DEFAULT_SPLIT; }
}
export function writeSplit(store: ViewStateStore | undefined, spec: SplitSpec, state: SplitState): void {
  const text = JSON.stringify({ size: state.size, collapsed: state.collapsed });
  try {
    if (!store) { localStorage.setItem(`lazybox:split.${spec.name}`, text); return; }
    const keys = splitKeys(store, spec.name);
    store.setItem(keys.window, text);
    store.setItem(keys.shared, text);
  } catch { /* a convenience */ }
}

// Every divider inside a panel, in one place: their limits are read together.
export const SPLITS = {
  /** Changes: the changed files (and, narrow, the Files / Review column) beside the diff. */
  changesFiles: { name: 'changes-files', side: 'start', min: 180, max: 640, keep: 320, collapsible: true },
  /** Changes: the review tray beside the diff. */
  changesReview: { name: 'changes-review', side: 'end', min: 240, max: 560, keep: 320 },
  artifactsList: { name: 'artifacts-list', side: 'start', min: 200, max: 560, keep: 360, collapsible: true },
  artifactHistory: { name: 'artifact-history', side: 'end', min: 240, max: 520, keep: 360 },
  /** A chat in its own window: the artifacts shown beside it. */
  detachedArtifacts: { name: 'detached-artifacts', side: 'end', min: 320, max: 1400, keep: 320 },
  settingsNav: { name: 'settings-nav', side: 'start', min: 160, max: 360, keep: 420 },
  docsNav: { name: 'docs-nav', side: 'start', min: 160, max: 440, keep: 360, collapsible: true },
  snippetList: { name: 'snippet-list', side: 'start', min: 160, max: 420, keep: 240 },
  browserLog: { name: 'browser-log', side: 'end', axis: 'y', min: 60, max: 640, keep: 120 },
} as const satisfies Record<string, SplitSpec>;
