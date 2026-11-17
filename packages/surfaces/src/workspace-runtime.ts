import { useEffect, useState, useSyncExternalStore } from 'react';
import { Code, ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { WorkspaceAction, WorkspaceActionResult, WorkspaceRunState, WorkspaceRuntime, WorkspaceScriptSource, WorkspaceSetup } from '@lazybox/protocol';

// The workspace runtime (WORKSPACE_RUNTIME.md) as the UI sees it: one poll of
// GetWorkspaceRuntime per workspace and Runner, shared by the header's Run
// control, the Actions panel and the Preview panel; pure labels beside it.

/** A run that is going: its output is still arriving. */
export const isLive = (state?: WorkspaceRunState) => state?.state === 'running' || state?.state === 'stopping';

/** Elapsed time the way a status line reads it: 12s, 4m, 1h 5m. */
export function elapsed(startedAtMillis: bigint | number, now = Date.now()): string {
  const start = Number(startedAtMillis);
  if (!start) return '';
  const seconds = Math.max(0, Math.floor((now - start) / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  return minutes % 60 ? `${hours}h ${minutes % 60}m` : `${hours}h`;
}

export type Tone = 'ok' | 'busy' | 'warn' | 'bad' | 'idle';
/** What a run's state reads as, and its colour. */
export function runLabel(state?: WorkspaceRunState): { text: string; tone: Tone } {
  switch (state?.state) {
    case 'running': return { text: 'Running', tone: 'ok' };
    case 'stopping': return { text: 'Stopping…', tone: 'busy' };
    case 'succeeded': return { text: 'Finished', tone: 'idle' };
    case 'failed': return { text: state.error ? `Failed: ${state.error}` : state.exitCode !== undefined ? `Failed · exit ${state.exitCode}` : state.signal ? `Ended by ${state.signal}` : 'Failed', tone: 'bad' };
    case 'stopped': return { text: 'Stopped', tone: 'idle' };
    default: return { text: 'Not running', tone: 'idle' };
  }
}
/** A setup run reads differently: it is meant to finish. */
export function setupLabel(setup: WorkspaceSetup): { text: string; tone: Tone } {
  if (setup.pendingApproval) return { text: 'Waiting for your approval', tone: 'warn' };
  const state = setup.runState;
  switch (state?.state) {
    case 'running': return { text: 'Running', tone: 'busy' };
    case 'stopping': return { text: 'Stopping…', tone: 'busy' };
    case 'succeeded': return { text: 'Done', tone: 'ok' };
    case 'failed': return { text: state.exitCode !== undefined ? `Failed · exit ${state.exitCode}` : state.error ? `Failed: ${state.error}` : 'Failed', tone: 'bad' };
    case 'stopped': return { text: 'Stopped', tone: 'warn' };
    default: return { text: 'Not run yet', tone: 'idle' };
  }
}

/** An action's name, with its repository when another repository has one by the same name. */
export function actionName(action: Pick<WorkspaceAction, 'member' | 'name'>, all: readonly Pick<WorkspaceAction, 'member' | 'name'>[] = []): string {
  const shared = all.some(other => other.name === action.name && other.member !== action.member);
  return shared ? `${action.member} · ${action.name}` : action.name;
}
export const portLabel = (port: number) => port ? `:${port}` : '';

/** The action Run starts. */
export function defaultAction(runtime?: WorkspaceRuntime): WorkspaceAction | undefined {
  if (!runtime) return undefined;
  return runtime.actions.find(a => a.key === runtime.defaultAction) || runtime.actions.find(a => a.default) || runtime.actions[0];
}
/** Sources whose scripts wait for the person: never approved, or changed since. */
export const needsApproval = (runtime?: WorkspaceRuntime): WorkspaceScriptSource[] => runtime?.sources.filter(s => !s.error && (s.approval === 'required' || s.approval === 'changed')) || [];
export const setupsRunning = (runtime?: WorkspaceRuntime) => runtime?.setups.filter(s => isLive(s.runState)) || [];
/** Running actions a person can open: those that serve a URL. */
export const previewable = (runtime?: WorkspaceRuntime) => runtime?.actions.filter(a => a.url && isLive(a.runState)) || [];

/** "action:dev" → "Action “dev”": what changed since the last approval, in words. */
export function changedLabel(entry: string): string {
  if (entry.startsWith('action:')) return `Action “${entry.slice(7)}”`;
  return ({ setup: 'Setup script', archive: 'Archive script', files: 'Files to copy', env: 'Environment' } as Record<string, string>)[entry] || entry;
}

/** "Stopped dev in Beta release." for a non-concurrent start. */
export function stoppedElsewhereNotice(result: Pick<WorkspaceActionResult, 'stoppedElsewhere'>, name: string): string {
  const places = [...new Set(result.stoppedElsewhere.map(s => s.workspaceName || s.collectionId))];
  if (!places.length) return '';
  const list = places.length === 1 ? places[0] : `${places.slice(0, -1).join(', ')} and ${places.at(-1)}`;
  return `Stopped ${name} in ${list} to run it here.`;
}

/**
 * The 1-based lines of a workspace.yaml that hold the changed entries, so the
 * review can mark them. Top-level keys (setup, archive, files, env) span to the
 * next top-level key; "action:<name>" spans its entry under `actions:`.
 */
export function changedLines(content: string, changed: readonly string[]): Set<number> {
  const lines = content.split('\n'), marked = new Set<number>();
  const indent = (line: string) => line.length - line.trimStart().length;
  const meaningful = (line: string) => line.trim() !== '' && !line.trimStart().startsWith('#');
  const keyOf = (line: string) => /^\s*["']?([^"':#\s][^"':#]*?)["']?\s*:(\s|$)/.exec(line)?.[1];
  const span = (start: number, depth: number) => {
    marked.add(start + 1);
    for (let i = start + 1; i < lines.length; i++) {
      if (meaningful(lines[i]) && indent(lines[i]) <= depth) break;
      if (lines[i].trim() !== '') marked.add(i + 1);
    }
  };
  for (const entry of changed) {
    if (entry.startsWith('action:')) {
      const name = entry.slice(7);
      const block = lines.findIndex(line => indent(line) === 0 && keyOf(line) === 'actions');
      if (block < 0) continue;
      let child = -1;
      for (let i = block + 1; i < lines.length; i++) {
        if (meaningful(lines[i]) && indent(lines[i]) === 0) break;
        if (meaningful(lines[i]) && child < 0) child = indent(lines[i]);
        if (meaningful(lines[i]) && indent(lines[i]) === child && keyOf(lines[i]) === name) { span(i, child); break; }
      }
      continue;
    }
    const at = lines.findIndex(line => indent(line) === 0 && keyOf(line) === entry);
    if (at >= 0) span(at, 0);
  }
  return marked;
}

/** A Runner without the workspace runtime answers Unimplemented: the UI stays out of the way. */
export const unsupported = (error: unknown) => error instanceof ConnectError && error.code === Code.Unimplemented;
/** A refused approval because the file changed while the person read it. */
export const staleApproval = (error: unknown) => error instanceof ConnectError && error.code === Code.FailedPrecondition;
export const errorText = (error: unknown) => error instanceof ConnectError ? error.rawMessage : String(error);

export type Notice = { text: string; tone: 'ok' | 'warn'; at: number };
type Snapshot = { runtime?: WorkspaceRuntime; error: string; unsupported: boolean; notice?: Notice; busy: ReadonlySet<string> };

/** One workspace's runtime on one Runner: polled while anything shows it. */
class RuntimeStore {
  private snapshot: Snapshot = { error: '', unsupported: false, busy: new Set() };
  private listeners = new Set<() => void>();
  private timer: ReturnType<typeof setInterval> | undefined;
  private reading = false;
  private control = new AbortController();
  constructor(private api: RunnerClient, readonly workspaceId: string) {}
  get = () => this.snapshot;
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    if (this.listeners.size === 1) this.start();
    return () => { this.listeners.delete(listener); if (!this.listeners.size) this.stop(); };
  };
  private emit(next: Partial<Snapshot>) { this.snapshot = { ...this.snapshot, ...next }; for (const listener of this.listeners) listener(); }
  private start() {
    this.control = new AbortController();
    void this.refresh();
    this.timer = setInterval(() => void this.refresh(), 1500);
  }
  private stop() { clearInterval(this.timer); this.timer = undefined; this.control.abort(); }
  refresh = async () => {
    if (this.reading || this.snapshot.unsupported) return;
    this.reading = true;
    const signal = this.control.signal;
    try {
      const runtime = await this.api.getWorkspaceRuntime({ collectionId: this.workspaceId }, { signal, timeoutMs: 8000 });
      if (!signal.aborted) this.emit({ runtime, error: '' });
    } catch (error) {
      if (signal.aborted) return;
      if (unsupported(error)) this.emit({ unsupported: true, error: '' });
      else this.emit({ error: errorText(error) });
    } finally { this.reading = false; }
  };
  notify(text: string, tone: Notice['tone'] = 'ok') { this.emit({ notice: text ? { text, tone, at: Date.now() } : undefined }); }
  /** Runs one change, marking `key` busy meanwhile; the Runner's answer replaces the snapshot. */
  async run(key: string, change: () => Promise<WorkspaceRuntime | undefined>): Promise<{ ok: boolean; error?: unknown }> {
    if (this.snapshot.busy.has(key)) return { ok: false };
    this.emit({ busy: new Set([...this.snapshot.busy, key]) });
    try {
      const runtime = await change();
      if (runtime) this.emit({ runtime, error: '' });
      return { ok: true };
    } catch (error) {
      this.notify(errorText(error), 'warn');
      return { ok: false, error };
    } finally {
      const busy = new Set(this.snapshot.busy); busy.delete(key); this.emit({ busy });
      void this.refresh();
    }
  }
  startAction = async (action: WorkspaceAction, restart = false, all: readonly WorkspaceAction[] = []) => this.run(action.key, async () => {
    const result = await this.api.startWorkspaceAction({ collectionId: this.workspaceId, action: action.key, restart });
    const name = actionName(action, all);
    const elsewhere = stoppedElsewhereNotice(result, name);
    if (elsewhere) this.notify(elsewhere);
    return result.runtime;
  });
  stopAction = async (action: WorkspaceAction) => this.run(action.key, async () => {
    const result = await this.api.stopWorkspaceAction({ collectionId: this.workspaceId, action: action.key });
    return result.runtime;
  });
  runSetup = async (member: string) => this.run(`setup:${member}`, async () => {
    const runtime = await this.api.runWorkspaceSetup({ collectionId: this.workspaceId, member });
    this.notify(`Running ${member}’s setup.`);
    return runtime;
  });
  approve = async (source: WorkspaceScriptSource, runSetup: boolean) => this.run(`approve:${source.member}`, async () => {
    const runtime = await this.api.approveWorkspaceScripts({ collectionId: this.workspaceId, member: source.member, hash: source.hash, runSetup });
    this.notify(runSetup ? `Approved ${source.member}’s scripts; setup is running.` : `Approved ${source.member}’s scripts.`);
    return runtime;
  });
}

const stores = new WeakMap<RunnerClient, Map<string, RuntimeStore>>();
export function runtimeStore(api: RunnerClient, workspaceId: string): RuntimeStore {
  let byWorkspace = stores.get(api);
  if (!byWorkspace) { byWorkspace = new Map(); stores.set(api, byWorkspace); }
  let store = byWorkspace.get(workspaceId);
  if (!store) { store = new RuntimeStore(api, workspaceId); byWorkspace.set(workspaceId, store); }
  return store;
}
export type { RuntimeStore };

const empty: Snapshot = { error: '', unsupported: false, busy: new Set() };
const noop = () => () => {};
/** The workspace's runtime, shared by every component that shows it. */
export function useWorkspaceRuntime(api: RunnerClient, workspaceId: string) {
  const store = workspaceId ? runtimeStore(api, workspaceId) : undefined;
  const snapshot = useSyncExternalStore(store?.subscribe || noop, store?.get || (() => empty));
  return { ...snapshot, store };
}

/** The store's latest notice while it is fresh: a status, not a log. */
export function useRecentNotice(notice?: Notice, forMs = 7000): Notice | undefined {
  const [shown, setShown] = useState(0);
  useEffect(() => {
    if (!notice) return;
    const left = notice.at + forMs - Date.now();
    if (left <= 0) return;
    setShown(notice.at);
    const timer = setTimeout(() => setShown(0), left);
    return () => clearTimeout(timer);
  }, [notice?.at]);
  return notice && shown === notice.at ? notice : undefined;
}

/** Re-renders every second while something is live, for elapsed times. */
export function useTicker(active: boolean) {
  const subscribe = (listener: () => void) => { if (!active) return () => {}; const timer = setInterval(listener, 1000); return () => clearInterval(timer); };
  const now = useSyncExternalStore(subscribe, () => active ? Math.floor(Date.now() / 1000) : 0);
  return now ? now * 1000 : Date.now();
}

// Requests between the header, the Actions panel and the Preview panel, which
// mount independently: the latest request per workspace waits for its panel.
type Request = { kind: 'preview'; action: string } | { kind: 'focus'; target: string };
const requests = new Map<string, Request[]>();
const requestListeners = new Set<() => void>();
function request(workspaceId: string, value: Request) {
  requests.set(workspaceId, [...(requests.get(workspaceId) || []).filter(r => r.kind !== value.kind), value]);
  for (const listener of requestListeners) listener();
}
/** Ask the Preview panel to show this action (key). */
export const requestPreview = (workspaceId: string, action: string) => request(workspaceId, { kind: 'preview', action });
/** Ask the Actions panel to show and open the log of an action key or "setup:<member>". */
export const requestActionFocus = (workspaceId: string, target: string) => request(workspaceId, { kind: 'focus', target });
/** Calls onRequest with each request of this kind for the workspace, now or when it arrives. */
export function useRuntimeRequest(workspaceId: string, kind: Request['kind'], onRequest: (value: string) => void) {
  useEffect(() => {
    const take = () => {
      const pending = requests.get(workspaceId) || [];
      const found = pending.find(r => r.kind === kind);
      if (!found) return;
      requests.set(workspaceId, pending.filter(r => r !== found));
      onRequest(found.kind === 'preview' ? found.action : found.target);
    };
    take(); requestListeners.add(take);
    return () => { requestListeners.delete(take); };
  });
}

