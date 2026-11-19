/**
 * Run what the agent wrote. A shell code block in an agent's message gets a
 * Run split button beside Copy:
 *
 * - **Run** runs it in a shell of the workspace's Terminal panel (below);
 *   its ▾ also pastes it there without running, or asks the conversation's
 *   agent to run it ("Run this and tell me the result:" and the block): the
 *   agent runs it with `lazybox run`, sees a failure and reacts. Sent at
 *   once, or queued behind the turn in progress. The block then says how it
 *   went, from the agent's matching tool call.
 * - **Run as command output…** opens a confirm card under the block (the script,
 *   editable; the folder; Run/Cancel), then runs it as a command of this
 *   conversation in the Command output console. The block shows the outcome,
 *   and a failure offers to hand the end of its output to the agent.
 *
 * Inline code that reads as a command offers the same two in its right-click
 * menu only. Nothing ever runs without a click or key, and Run myself always
 * confirms. `code-run.ts` holds the pure rules.
 *
 * A command the agent hands back to the person (a login, a dev server to try
 * by hand) also runs in a home checkout: the menus offer "Run in <repo> home"
 * and the conversation's last message, once its turn is over, shows one
 * button per such command (`home-run.ts`, `HomeRuns`). It runs the command
 * in a shell of the workspace's Terminal panel there (`home-shell.ts`): at
 * once when it is one line with nothing to fill in, else typed at the prompt
 * for the person to review and run with Return.
 */
import { createContext, useContext, useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { Collection, CommandStatus, Session, WorkspaceAction, WorkspaceRuntime } from '@lazybox/protocol';
import { agentRun, fixRequestMessage, matchingAction, placeholdersIn, requestedScript, runArgv, runRequestMessage, runTitle, scriptOf, serverLike, shellScript, isShellLanguage } from './code-run';
import { openInBrowser, openInViewer } from './web-links';
import { copyText, openContextMenu, showMenuNotice } from './ContextMenu';
import type { MenuItem } from './context-menu-model';
import { Icon } from './Icons';
import { shortPath } from './tool-lines';
import type { TranscriptMessage } from './transcript-model';
import './code-run.css';
import { doneMessage, explicitHomeTarget, homeOffers, inHome, needsTerminal, placeFor, placeLabel, reviewTerminalCommand, type HomeOffer, type HomeTarget, type HomePlace, type HomeRepository } from './home-run';
import { homesChanged, libraryOf, subscribeLibrary } from './home-checkouts';
import { openHomeShell, type ShellPlace } from './home-shell';
import { runsAtOnce } from './shell-input';
import { hasTerminalHost, openInTerminal } from './handoff';
import { cdCommand } from './handoff-model';

export type RunActions = {
  /** Sends a message to the agent now (queued behind a turn in progress); `drafted` when it went to the message box instead. */
  send(text: string, reportId?: string): Promise<'sent' | 'drafted'>;
  /** Adds text to the message box for the person to review and send. */
  draft(text: string, notice: string): void;
  /** Shows a command of this conversation in the Command output console. */
  openOutput?(commandId: string): void;
};
/** start: Run asked to start a server or watcher in the background. */
type AgentRecord = { messageId: string; script: string; inline: boolean; queued: boolean; sawQueue: boolean; at: number; start?: boolean };
/** action: the workspace action (key) Run myself started instead of the script. */
type SelfRecord = { messageId: string; script: string; inline: boolean; commandId: string; cwd: string; action?: string };
/** A command in a shell of the Terminal panel: waiting for the shell, then running (or typed, waiting for Return), then finished (the shell's mark), then told to the agent. */
type TerminalState = 'starting' | 'running' | 'typed' | 'finished';
type TerminalRecord = { reportId?: string; reporting?: boolean; messageId: string; script: string; typed: string; where: string; state: TerminalState; exitCode?: number; told?: boolean };
type Memory = { agent: AgentRecord[]; self: SelfRecord[]; terminal: TerminalRecord[] };
/** What was run from each conversation, kept while the app runs so a remounted chat still shows it. */
const memory = new Map<string, Memory>();
const remembered = (sessionId: string) => { let entry = memory.get(sessionId); if (!entry) { entry = { agent: [], self: [], terminal: [] }; memory.set(sessionId, entry); } return entry; };

/** Where a person's run goes: here (the conversation's folder in the workspace), a repository's home, the home folder. */
export type RunPlace = HomePlace | { kind: 'here' };

/** start: the request asked to start a server or watcher in the background; `background`: it runs there now, on `port` when known. */
export type AgentRunView = { state: 'queued' | 'sent' | 'running' | 'ok' | 'failed' | 'background'; exitCode?: number; start?: boolean; port?: number };
/** server: it keeps running (a server or a watcher), with its port and address once it listens; action: the workspace action it is. */
export type SelfRunView = { commandId: string; state: 'running' | 'ok' | 'failed' | 'stopped'; exitCode?: number; signal?: string; folder: string; server?: boolean; port?: number; url?: string; action?: string };
/** A run in the Terminal panel: where, whether it runs or waits for Return, and whether it ended (and how). */
export type TerminalRunView = { reporting?: boolean; where: string; state: TerminalState; exitCode?: number; told?: boolean };
export type RunView = { agent?: AgentRunView; self?: SelfRunView; terminal?: TerminalRunView; confirming: boolean };
export type Folder = { label: string; path: string };

/** What blocks read of the session (folders, read-only, why Run myself cannot run, queued): a poll that changes none of it is no change. */
const sessionFacts = (session?: Session) => session ? JSON.stringify([session.permission, session.workflow, session.closed, !!session.fault, session.workingDirectory, session.collectionId, session.busy, session.queued]) : '';

/**
 * One conversation's runs. The chat that shows the conversation creates it and
 * keeps it current (`update`); code blocks subscribe to their own view.
 */
export class CodeRuns {
  readonly sessionId: string;
  agentName = 'Claude';
  private api: RunnerClient;
  private session?: Session;
  private workspace?: Collection;
  private messages: readonly TranscriptMessage[] = [];
  private actions?: RunActions;
  private commands = new Map<string, CommandStatus>();
  /** The workspace's actions, to start the one a block stands for. */
  private runtime?: WorkspaceRuntime;
  private runtimeFor = '';
  private confirm?: { messageId: string; script: string; inline: boolean; at: number };
  private listeners = new Set<() => void>();
  private timer?: ReturnType<typeof setInterval>;
  private listing = false;
  private attached = false;
  private views = new Map<string, { revision: number; signature: string }>();
  private revision = 0;

  constructor(api: RunnerClient, sessionId: string) { this.api = api; this.sessionId = sessionId; }

  // ---- Inputs, from the chat ------------------------------------------------

  /**
   * The chat calls this on every render. Blocks are told only when something
   * they show may have changed: a session poll (every 1.5 s) or a batch of
   * streamed text that changes none of it re-renders no block, and no block
   * rescans the conversation.
   */
  update({ session, workspace, messages, agentName, actions }: { session?: Session; workspace?: Collection; messages: readonly TranscriptMessage[]; agentName: string; actions: RunActions }) {
    this.actions = actions;
    let changed = false;
    if (messages !== this.messages) { this.messages = messages; if (this.readsMessages()) changed = true; }
    if (workspace !== this.workspace) {
      this.workspace = workspace; changed = true;
      // A workspace known now: read its actions, to match blocks to them.
      if (this.attached && workspace?.id && this.runtimeFor !== workspace.id) queueMicrotask(() => void this.refresh());
    }
    if (agentName !== this.agentName) { this.agentName = agentName; changed = true; }
    if (session !== this.session) {
      const before = sessionFacts(this.session);
      this.session = session;
      if (sessionFacts(session) !== before) changed = true;
      // A request sent during a turn waits in the Runner's queue: it runs once that queue empties, or the turn ends.
      for (const run of this.memory.agent) if (run.queued && session) {
        if (session.queued > 0) run.sawQueue = true;
        else if (run.sawQueue || !session.busy) { run.queued = false; changed = true; }
      }
    }
    if (changed) this.emit();
  }
  /** A Run request was seen in this conversation: from then on its messages say how runs went. */
  private requested = false;
  /**
   * Whether the conversation's messages can change what a block shows: only
   * once something was run from it (asked of the agent, a command of the
   * conversation, a Run request in it). Until then every block shows Run.
   */
  private readsMessages(): boolean {
    if (this.memory.agent.length || this.commands.size) return true;
    this.requested ||= this.messages.some(message => message.type === 'operation_accepted' && requestedScript(message.text) !== undefined);
    return this.requested;
  }
  /** Reads this conversation's earlier person runs once, and follows running ones. */
  attach() {
    this.attached = true; void this.refresh();
    // A home created or forgotten anywhere changes what Run in … home offers.
    const unsubscribe = subscribeLibrary(() => this.emit());
    return () => { this.attached = false; clearInterval(this.timer); this.timer = undefined; unsubscribe(); };
  }

  // ---- Subscription ---------------------------------------------------------

  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
  private emit() { this.revision++; for (const listener of this.listeners) listener(); }
  private get memory() { return remembered(this.sessionId); }

  // ---- What a block shows -----------------------------------------------------

  /** A block's view as a string that changes only when what it shows does, for `useSyncExternalStore`. */
  signature(messageId: string, script: string): string {
    const key = messageId + '\0' + script, cached = this.views.get(key);
    if (cached?.revision === this.revision) return cached.signature;
    const signature = JSON.stringify(this.view(messageId, script));
    this.views.set(key, { revision: this.revision, signature });
    return signature;
  }
  view(messageId: string, script: string): RunView {
    const confirming = this.confirm?.messageId === messageId && this.confirm.script === script;
    const local = this.memory.agent.filter(run => run.messageId === messageId && run.script === script).at(-1);
    const derived = agentRun(this.messages, messageId, script);
    let agent: AgentRunView | undefined;
    const start = derived ? derived.kind === 'start' : !!local?.start;
    if (local?.queued && !derived?.outcome) agent = { state: 'queued', start };
    else if (derived?.outcome) agent = { state: derived.outcome.state, exitCode: derived.outcome.exitCode, start, ...(derived.outcome.port ? { port: derived.outcome.port } : {}) };
    else if (derived || local) agent = { state: 'sent', start };
    const terminal = this.terminalView(messageId, script);
    return { agent, self: this.selfView(messageId, script), ...(terminal ? { terminal } : {}), confirming };
  }
  private selfView(messageId: string, script: string): SelfRunView | undefined {
    const local = this.memory.self.filter(run => run.messageId === messageId && run.script === script).at(-1);
    if (local?.action) return this.actionView(local);
    let command = local && this.commands.get(local.commandId);
    if (!local) {
      // After a reload: the conversation's latest run of this script belongs to the latest message that has it.
      command = [...this.commands.values()].filter(value => scriptOf(value.argv) === script).at(-1);
      if (!command || this.lastMessageWith(script) !== messageId) return undefined;
    }
    const folder = this.folderLabel(command?.cwd || local?.cwd || '');
    if (!command || !command.done) {
      // A server never ends: it says where it listens instead of waiting for an exit.
      const port = command?.ports[0], server = serverLike(script).server || !!port;
      return { commandId: local?.commandId || command!.id, state: 'running', folder, ...(server ? { server } : {}), ...(port ? { port, url: `http://localhost:${port}` } : {}) };
    }
    const exitCode = command.exitCode;
    return { commandId: command.id, folder, state: command.signal ? 'stopped' : exitCode === 0 ? 'ok' : 'failed', exitCode, signal: command.signal || undefined };
  }
  /** A workspace action Run myself started: its state, port and address, as the workspace runtime says. */
  private actionView(local: SelfRecord): SelfRunView {
    const action = this.runtime?.actions.find(entry => entry.key === local.action);
    const run = action?.runState;
    const base = { commandId: run?.commandId || local.commandId, folder: action?.member || this.folderLabel(local.cwd), server: true, action: local.action, ...(action?.port ? { port: action.port } : {}), ...(action?.url ? { url: action.url } : {}) };
    switch (run?.state) {
      case 'succeeded': return { ...base, state: 'ok', exitCode: run.exitCode ?? 0 };
      case 'failed': return { ...base, state: 'failed', exitCode: run.exitCode };
      case 'stopped': return { ...base, state: 'stopped', signal: run.signal || undefined };
      default: return { ...base, state: 'running' };
    }
  }
  /** The workspace action a script stands for: Run myself starts it instead (it gets the workspace's port, and shows in Run and Preview). */
  actionFor(script: string): WorkspaceAction | undefined { return this.runtime ? matchingAction(this.runtime.actions, script) : undefined; }
  private lastMessageWith(script: string): string {
    for (let i = this.messages.length - 1; i >= 0; i--) if (this.messages[i].type === 'message_delta' && this.messages[i].text.includes(script)) return this.messages[i].id;
    return '';
  }
  /** The inline commands of a message someone ran, oldest first. */
  inlineScripts(messageId: string): string[] {
    const scripts = [...this.memory.agent, ...this.memory.self].filter(run => run.inline && run.messageId === messageId).map(run => run.script);
    if (this.confirm?.inline && this.confirm.messageId === messageId) scripts.push(this.confirm.script);
    return [...new Set(scripts)];
  }
  inlineSignature(messageId: string): string { return this.inlineScripts(messageId).join('\0'); }

  // ---- Conversation facts ---------------------------------------------------

  /** '' when this conversation may change things; else the mode that says it may not. */
  readOnlyMode(): '' | 'read-only' | 'plan' {
    return this.session?.permission === 'read-only' ? 'read-only' : this.session?.workflow === 'plan' ? 'plan' : '';
  }
  /** Why the person cannot run a command in this conversation now, or ''. */
  whyNotRunMyself(): string {
    const session = this.session;
    if (!session) return 'Still connecting to this conversation…';
    if (session.closed) return 'This conversation has ended: resume it to run commands in it.';
    if (session.fault) return 'This conversation stopped with an error: continue it to run commands in it.';
    return '';
  }
  /** Where a command can run: the workspace root and each of its repositories. */
  folders(): Folder[] {
    const root = this.workspace?.root || this.session?.workingDirectory || '';
    const out: Folder[] = root ? [{ label: this.workspace ? 'Workspace root' : 'Conversation folder', path: root }] : [];
    for (const member of this.workspace?.members || []) if (member.path && member.path !== root) out.push({ label: member.spec?.name || member.path.split('/').pop() || member.path, path: member.path });
    const own = this.session?.workingDirectory;
    if (own && !out.some(folder => folder.path === own)) out.push({ label: 'Conversation folder', path: own });
    return out;
  }
  defaultFolder(): string { return this.session?.workingDirectory || this.folders()[0]?.path || ''; }
  private folderLabel(path: string): string { return this.folders().find(folder => folder.path === path)?.label || (path ? shortPath(path, this.workspace?.root || '') : ''); }
  canOpenOutput() { return !!this.actions?.openOutput; }

  // ---- Home checkouts -------------------------------------------------------

  /** Resolve an explicit home when clicked, using the current repository list. */
  runPlace(target: HomeTarget): HomePlace;
  runPlace(target?: HomeTarget): RunPlace;
  runPlace(target?: HomeTarget): RunPlace {
    if (!target) return { kind: 'here' };
    const members = this.homeRepositories();
    const place = placeFor(target, members);
    if (place.kind !== 'unknown') return place;
    const registeredHomes = libraryOf(this.api).filter(repo => repo.homePath)
      .map(repo => ({ repositoryId: repo.id, name: repo.name, identity: repo.gitIdentity, home: repo.homePath }));
    return placeFor(target, registeredHomes);
  }

  runLastTerminal(messageId: string, script: string) {
    const message = this.messages.find(value => value.id === messageId);
    const target = explicitHomeTarget(message?.text || '', script);
    return this.runIn(messageId, script, this.runPlace(target), 'run', !!target);
  }

  /** The workspace's repositories and their homes, as the library records them. */
  homeRepositories(): HomeRepository[] {
    const library = libraryOf(this.api);
    const out: HomeRepository[] = [];
    for (const member of this.workspace?.members || []) {
      if (!member.spec || out.some(r => r.repositoryId === member.spec!.repositoryId)) continue;
      const repo = library.find(r => r.id === member.spec!.repositoryId);
      out.push({ repositoryId: member.spec.repositoryId, name: member.spec.name, identity: repo?.gitIdentity || '', home: repo?.homePath || '' });
    }
    return out;
  }
  /** What the conversation's last message hands back to the person, once its turn is over: nothing for any other message. */
  homeOffers(messageId: string): { offer: HomeOffer; place: HomePlace }[] {
    if (this.session?.busy || !this.session) return [];
    let last: TranscriptMessage | undefined;
    for (let i = this.messages.length - 1; i >= 0 && !last; i--) if (this.messages[i].type === 'message_delta') last = this.messages[i];
    if (!last || last.id !== messageId) return [];
    const repositories = this.homeRepositories();
    return homeOffers(last.text).map(offer => ({
      offer, place: offer.explicit ? this.runPlace(offer.target) : placeFor(offer.target, repositories, offer.hint),
    })).filter(entry => entry.place.kind !== 'unknown');
  }
  homeSignature(messageId: string): string { return JSON.stringify(this.homeOffers(messageId)); }
  /** Where a block or command can run outside the workspace: each repository's home that exists, then the home folder. */
  homePlaces(): HomePlace[] {
    return [...this.homeRepositories().filter(r => r.home).slice(0, 3).map(repository => ({ kind: 'repository' as const, repository })), { kind: 'user' as const }];
  }
  /** The conversation's folder as a place in its workspace: relative to the root ('' is the root), or undefined outside a workspace. */
  here(): { directory: string; label: string; path: string } | undefined {
    const root = this.workspace?.root, own = this.session?.workingDirectory || root;
    if (!this.workspaceId() || !root || !own) return undefined;
    const directory = own === root || !own.startsWith(root + '/') ? '' : own.slice(root.length + 1);
    return { directory, label: directory || 'the workspace', path: directory ? own : root };
  }
  /** What a place is called in a sentence: "the workspace", "api home", "your home folder". */
  placeName(place: RunPlace): string {
    return place.kind === 'here' ? this.here()?.label || 'the workspace' : place.kind === 'user' ? 'your home folder' : place.kind === 'repository' ? `${place.repository.name} home` : `${place.name} home`;
  }
  /**
   * Run: a shell of the Terminal panel (a real terminal, so a login can
   * prompt), in `place`: one already open there and idle, else a new one.
   * Once the shell is ready, a one-line command with nothing to fill in
   * runs; a longer script, a placeholder, or `paste` is typed at the prompt
   * for the person to review and run with Return. In a home, a leading `cd`
   * into a temporary checkout is left out (the home stands for it). A
   * repository without a home gets one first.
   */
  async runIn(messageId: string, script: string, place: RunPlace, mode: 'run' | 'paste' = 'run', report = false) {
    const workspace = this.workspaceId();
    if (place.kind === 'unknown') { showMenuNotice(`No checkout for ${place.name} is available on this Runner.`); return; }
    let shell: ShellPlace, typed = script;
    if (place.kind === 'here') {
      const here = this.here();
      if (!here) { await copyText(script, 'The terminal needs a workspace: the command is copied, to paste in yours'); return; }
      shell = { directory: here.directory, label: here.label };
    } else if (place.kind === 'user') shell = { userHome: true, label: 'your home folder' };
    else {
      const { repository } = place;
      if (!repository.home) {
        showMenuNotice(`Cloning ${repository.name}’s home checkout…`);
        try { await this.api.setRepositoryHome({ repositoryId: repository.repositoryId, action: 'create', path: '' }, { timeoutMs: 10 * 60_000 }); homesChanged(this.api); }
        catch (error) { showMenuNotice(`Could not create ${repository.name}’s home: ${String(error instanceof Error ? error.message : error).replace(/^\[\w+\]\s*/, '')}`); return; }
      }
      shell = { repositoryId: repository.repositoryId, label: `${repository.name} home` };
      typed = inHome(script).command;
    }
    const record: TerminalRecord = { messageId, script, typed, where: this.placeName(place), state: 'starting' };
    this.memory.terminal = [...this.memory.terminal.filter(run => run.messageId !== messageId || run.script !== script), record];
    this.emit();
    const opened = await openHomeShell(this.api, workspace, shell, typed, {
      mode: reviewTerminalCommand(typed) ? 'paste' : mode,
      onTyped: how => {
        record.state = how === 'run' ? 'running' : 'typed';
        if (report && how === 'run') record.reportId = crypto.randomUUID();
        this.emit();
      },
      onEnd: status => {
        record.state = 'finished'; record.exitCode = status;
        if (record.reportId) void this.reportTerminalRun(record);
        else this.emit();
      },
    });
    if (opened.error) { this.memory.terminal = this.memory.terminal.filter(run => run !== record); this.emit(); }
  }
  private terminalView(messageId: string, script: string): TerminalRunView | undefined {
    const run = this.memory.terminal.find(entry => entry.messageId === messageId && entry.script === script);
    return run && { where: run.where, state: run.state, ...(run.exitCode !== undefined ? { exitCode: run.exitCode } : {}), ...(run.told ? { told: true } : {}), ...(run.reporting ? { reporting: true } : {}) };
  }
  /** "Tell Claude it's done": the command and how it ended, sent to the conversation (queued behind a turn), so the agent goes on. */
  async tellDone(messageId: string, script: string) {
    const run = this.memory.terminal.find(entry => entry.messageId === messageId && entry.script === script);
    if (run) await this.reportTerminalRun(run);
  }
  private async reportTerminalRun(run: TerminalRecord) {
    if (run.state !== 'finished' || run.told || run.reporting || !this.actions) return;
    run.reporting = true; this.emit();
    try {
      const how = await this.actions.send(doneMessage(run.typed, run.exitCode), run.reportId);
      if (how === 'sent') { run.told = true; showMenuNotice(`Told ${this.agentName}`); }
    } catch (error) { showMenuNotice(`Could not send: ${String(error instanceof Error ? error.message : error)}`); }
    finally { run.reporting = false; this.emit(); }
  }

  /** The folder a place is on this computer, for the person's own terminal app: '' when unknown (the home folder). */
  private placePath(place: RunPlace): string {
    return place.kind === 'here' ? this.here()?.path || '' : place.kind === 'repository' ? place.repository.home : '';
  }
  /** Open in Terminal: the person's own terminal app at the place, the command copied to paste there. */
  async openOwnTerminal(script: string, place: RunPlace) {
    const path = this.placePath(place), command = place.kind === 'repository' ? inHome(script).command : script;
    if (!path || !hasTerminalHost()) {
      // No app to open (a browser), or no folder to open it at: one line to paste.
      await copyText(path ? `${cdCommand(path)} && ${command}` : command, 'Copied — paste it in your terminal');
      return;
    }
    await copyText(command, 'Copied the command');
    await openInTerminal(path);
    showMenuNotice(`Opened your terminal at ${shortPath(path)}: paste the command there`);
  }

  // ---- Doing ----------------------------------------------------------------

  /** Run: ask the agent. */
  async askAgent(messageId: string, script: string, language: string, inline = false) {
    if (!this.actions) return;
    const busy = !!this.session?.busy;
    // A server or watcher never ends: the agent starts it in the background, by its workspace action when there is one.
    const action = this.actionFor(script), server = serverLike(script).server || !!action?.url;
    try {
      const how = await this.actions.send(runRequestMessage(script, language, { server, action: server ? action?.key : undefined }));
      if (how === 'drafted') return;
      this.memory.agent.push({ messageId, script, inline, queued: busy, sawQueue: false, at: Date.now(), start: server });
      this.emit();
      showMenuNotice(busy ? (server ? 'Queued — starts after the current turn' : 'Queued — runs after the current turn') : `Sent to ${this.agentName}`);
    } catch (error) { showMenuNotice(`Could not send: ${String(error instanceof Error ? error.message : error)}`); }
  }
  openConfirm(messageId: string, script: string, inline = false) {
    const message = this.messages.find(value => value.id === messageId);
    if (needsTerminal(script) || explicitHomeTarget(message?.text || '', script)) {
      showMenuNotice('Use the terminal button for this command');
      return;
    }
    this.confirm = { messageId, script, inline, at: Date.now() };
    this.emit();
  }
  closeConfirm() { if (!this.confirm) return; this.confirm = undefined; this.emit(); }
  confirmOpenedAt(messageId: string, script: string) { return this.confirm?.messageId === messageId && this.confirm.script === script ? this.confirm.at : 0; }
  /** Run myself, once confirmed: a command of this conversation, shown in its console. Resolves to '' or why it did not start. */
  async runMyself(messageId: string, original: string, script: string, cwd: string): Promise<string> {
    const inline = !!this.confirm?.inline;
    const reason = (error: unknown) => String(error instanceof Error ? error.message : error).replace(/^\[\w+\]\s*/, '');
    const action = this.actionFor(script), workspace = this.workspaceId();
    if (action && workspace) {
      try {
        const result = await this.api.startWorkspaceAction({ collectionId: workspace, action: action.key });
        if (result.runtime) this.runtime = result.runtime;
        const started = result.runtime?.actions.find(entry => entry.key === action.key)?.runState;
        if (started?.error) return started.error;
        this.memory.self.push({ messageId, script: original, inline, commandId: started?.commandId || '', cwd, action: action.key });
        this.confirm = undefined;
        this.emit();
        this.follow();
        return '';
      } catch (error) { return reason(error); }
    }
    try {
      const status = await this.api.startCommand({ sessionId: this.sessionId, title: runTitle(script), argv: runArgv(script), cwd });
      this.commands.set(status.id, status);
      // Edited before running: the block still owns the run.
      this.memory.self.push({ messageId, script: original, inline, commandId: status.id, cwd: status.cwd || cwd });
      this.confirm = undefined;
      this.emit();
      this.actions?.openOutput?.(status.id);
      this.follow();
      return '';
    } catch (error) { return reason(error); }
  }
  /** Stops a run: a workspace action through the runtime, else its command. */
  stop(commandId: string, action?: string) {
    const workspace = this.workspaceId();
    if (action && workspace) {
      void this.api.stopWorkspaceAction({ collectionId: workspace, action }).then(result => { if (result.runtime) { this.runtime = result.runtime; this.emit(); } showMenuNotice('Stopping…'); }, error => showMenuNotice(`Could not stop: ${String(error)}`));
      return;
    }
    void this.api.cancelCommand({ commandId }).then(() => showMenuNotice('Stopping…'), error => showMenuNotice(`Could not stop: ${String(error)}`));
  }
  openOutput(commandId: string) { this.actions?.openOutput?.(commandId); }
  /** Shows what a server serves: in this window's Web panel when it has one, else in the browser. */
  openPreview(url: string) { if (!openInViewer(url)) void openInBrowser(url); }
  private workspaceId() { return this.workspace?.id || this.session?.collectionId || ''; }
  /** Ask the agent to fix a failed run: the script, where it ran, how it ended and the end of its output join the message box. */
  async askToFix(commandId: string, script: string) {
    const command = this.commands.get(commandId);
    if (!command || !this.actions) return;
    try {
      const limit = 16384, end = Number(command.offset), from = Math.max(0, end - limit);
      const read = await this.api.readCommand({ commandId, offset: BigInt(from), limit });
      const output = new TextDecoder().decode(read.data);
      // Where it ran, as the agent reads paths: a repository's name, nothing for the workspace root.
      const root = this.workspace?.root || this.session?.workingDirectory || '';
      const folder = command.cwd && command.cwd !== root ? shortPath(command.cwd, root) : '';
      this.actions.draft(fixRequestMessage({ script, folder, exitCode: command.exitCode, signal: command.signal || undefined, output, truncated: from > 0 }),
        `Added the failed run and the end of its output. Review it and send to ${this.agentName}.`);
    } catch (error) { showMenuNotice(`Could not read the output: ${String(error)}`); }
  }

  private follow() {
    if (this.timer || !this.attached) return;
    this.timer = setInterval(() => void this.refresh(), 1500);
  }
  private async refresh() {
    if (this.listing || !this.attached) return;
    this.listing = true;
    // The workspace's actions: once, to match blocks to them, then while an action someone started from a block runs.
    const workspace = this.workspaceId();
    if (workspace && (this.runtimeFor !== workspace || this.memory.self.some(run => run.action))) {
      this.runtimeFor = workspace;
      try {
        const runtime = await this.api.getWorkspaceRuntime({ collectionId: workspace });
        const states = (value?: WorkspaceRuntime) => JSON.stringify(value?.actions.map(a => [a.key, a.runState?.state, a.runState?.commandId]) || null);
        const changed = states(runtime) !== states(this.runtime);
        this.runtime = runtime;
        if (changed) this.emit();
      } catch { /* No workspace actions (or an older Runner): blocks run as commands. */ }
    }
    try {
      const { commands } = await this.api.listCommands({ sessionId: this.sessionId });
      let changed = false;
      for (const command of commands) {
        if (command.sessionId !== this.sessionId || !scriptOf(command.argv)) continue;
        const known = this.commands.get(command.id);
        if (!known || known.done !== command.done || known.exitCode !== command.exitCode || known.signal !== command.signal || known.offset !== command.offset || known.ports.join() !== command.ports.join()) { this.commands.set(command.id, command); changed = true; }
      }
      if (changed) this.emit();
    } catch { /* The next poll retries; a block keeps its last state. */ }
    finally { this.listing = false; }
    const running = [...this.commands.values()].some(command => !command.done) || this.memory.self.some(run => run.action && this.actionView(run).state === 'running');
    if (running) this.follow();
    else if (this.timer) { clearInterval(this.timer); this.timer = undefined; }
  }
}

/** The conversation's runs, for code blocks under a chat. */
export const CodeRunContext = createContext<CodeRuns | null>(null);
/** The agent message a block belongs to; '' where code is not runnable (your own messages, previews). */
export const RunMessageContext = createContext<TranscriptMessage | null>(null);
const noSubscribe = () => () => {};

/** The runs of the conversation and message this code is in, when it can run. */
export function useRunScope(): { host: CodeRuns; messageId: string; text: string } | undefined {
  const host = useContext(CodeRunContext), message = useContext(RunMessageContext);
  return host && message ? { host, messageId: message.id, text: message.text } : undefined;
}

/**
 * The ways to run a command an agent wrote, best first. Run opens the
 * Terminal panel's shell (a real terminal) and runs the command there (a
 * script or a placeholder is typed for review); Paste types it without
 * running; then each home, the person's own terminal app, Copy. Asking the agent or
 * running it as captured output come last, marked "no terminal", and are
 * not offered at all for a command that needs one (a login, a sign-in).
 */
function runItems(host: CodeRuns, messageId: string, script: string, { inline, language = 'sh', target }: { inline: boolean; language?: string; target?: HomeTarget }): MenuItem[] {
  const terminalOnly = needsTerminal(script), here = host.here(), server = serverLike(script).server;
  const why = host.whyNotRunMyself(), place = host.runPlace(target);
  const unavailable = place.kind === 'unknown' ? `No checkout for ${place.name} is available on this Runner` : place.kind === 'here' && !here ? 'The terminal opens in a workspace: copy it instead' : undefined;
  return [
    { label: place.kind === 'here' ? inline ? `Run \`${clip(script)}\` in the terminal` : 'Run in the terminal' : placeLabel(place), icon: 'terminal', shortcut: inline ? undefined : 'Enter',
      disabled: unavailable, run: () => void host.runIn(messageId, script, place, 'run', !!target) },
    { label: PASTE_LABEL, icon: 'paste', disabled: unavailable, run: () => void host.runIn(messageId, script, place, 'paste') },
    ...host.homePlaces().filter(other => place.kind === 'here' || placeLabel(other) !== placeLabel(place)).map(other => ({ label: placeLabel(other), icon: 'home', run: () => void host.runIn(messageId, script, other) })),
    { label: 'Open in Terminal (your app)', icon: 'external', disabled: place.kind === 'unknown' ? unavailable : undefined, run: () => void host.openOwnTerminal(script, place) },
    { label: 'Copy command', icon: 'copy', run: () => void copyText(script, 'Copied the command') },
    ...(terminalOnly || target ? [] : ['separator' as const,
      { label: `Ask ${host.agentName} to ${server ? 'start it in the background' : 'run it'} (no terminal)`, icon: 'send', run: () => void host.askAgent(messageId, script, language, inline) },
      { label: 'Run as command output (no terminal)…', icon: 'output', shortcut: inline ? undefined : 'Alt+Enter', disabled: why || undefined, run: () => host.openConfirm(messageId, script, inline) }]),
  ];
}
/** Items to run an inline command: in its right-click menu. */
export function inlineRunItems(host: CodeRuns, messageId: string, command: string, text: string): MenuItem[] {
  return runItems(host, messageId, command, { inline: true, target: explicitHomeTarget(text, command) });
}
const clip = (text: string) => text.length > 48 ? text.slice(0, 47) + '…' : text;
/** Types the command at the prompt without Return: to edit it before it runs. */
const PASTE_LABEL = 'Paste in terminal (don’t run)';
/** What Run does with a command, in words for its tooltip. */
const terminalRunTitle = (script: string, where: string) => runsAtOnce(script) && !reviewTerminalCommand(script)
  ? `Runs this in a shell of the Terminal panel (${where}): one already open there when it is idle, else a new one`
  : `Types this in a shell of the Terminal panel (${where}): review it, then press Return`;
const NEEDS_TERMINAL = 'It signs in or asks for input: only a real terminal can answer, so it runs in the Terminal panel or your own terminal, never through the agent';

/**
 * A code block's run controls. Undefined unless the block is a shell block
 * in an agent's message of a conversation that can run things.
 */
export function useRunnableBlock(text: string, language: string, runnable: boolean): undefined | {
  script: string;
  header: ReactNode;
  footer: ReactNode;
  /** Built when a menu opens, so it says what is true then. */
  menu(): MenuItem[];
  onKeyDown(event: ReactKeyboardEvent<HTMLElement>): void;
} {
  const scope = useRunScope();
  const script = runnable && scope && isShellLanguage(language) ? shellScript(text, language) : '';
  const host = script ? scope!.host : undefined, messageId = scope?.messageId || '';
  const signature = useSyncExternalStore(host ? host.subscribe : noSubscribe, () => host ? host.signature(messageId, script) : '');
  if (!host || !script) return undefined;
  const view = JSON.parse(signature) as RunView;
  const terminalOnly = needsTerminal(script);
  // Run is the person's: a shell of the Terminal panel runs it (or holds it at the prompt for review), never a message to the agent.
  const target = explicitHomeTarget(scope?.text || '', script);
  const place = host.runPlace(target);
  const typed = place.kind === 'repository' ? inHome(script).command : script;
  const review = reviewTerminalCommand(typed);
  const label = review && place.kind !== 'unknown'
    ? `Review in ${host.placeName(place)}`
    : place.kind === 'here' ? 'Run in the terminal' : placeLabel(place);
  const run = () => void host.runIn(messageId, script, host.runPlace(target), 'run', !!target);
  const myself = () => { const why = terminalOnly || target ? 'Use the terminal button for this command' : host.whyNotRunMyself(); if (why) showMenuNotice(why); else host.openConfirm(messageId, script); };
  const menu = (): MenuItem[] => runItems(host, messageId, script, { inline: false, language, target });
  const header = <span className="code-run" role="group" aria-label="Run">
    {terminalOnly && <small className="code-run-tag" title={NEEDS_TERMINAL}>needs a terminal</small>}
    <button type="button" className="code-run-main" title={terminalRunTitle(typed, host.placeName(place))}
      aria-label={label} onClick={run}><Icon name="terminal" size={10}/>{target || review ? label : 'Run'}</button>
    <button type="button" className="code-run-more" title="More ways to run" aria-label="More ways to run" aria-haspopup="menu"
      onClick={event => { const box = event.currentTarget.getBoundingClientRect(); openContextMenu({ x: box.left, y: box.bottom + 4 }, menu(), { keyboard: event.detail === 0, target: event.currentTarget, label: 'Run' }); }}><Icon name="chevron" size={10}/></button>
  </span>;
  const footer = (view.agent || view.self || view.terminal || view.confirming) ? <div className="code-run-footer">
    {view.confirming && <RunConfirm host={host} messageId={messageId} script={script}/>}
    <RunStatus host={host} view={view} script={script} messageId={messageId}/>
  </div> : null;
  const onKeyDown = (event: ReactKeyboardEvent<HTMLElement>) => {
    if (event.target !== event.currentTarget || event.key !== 'Enter' || event.metaKey || event.ctrlKey || event.shiftKey) return;
    event.preventDefault(); event.stopPropagation();
    if (event.altKey) myself(); else run();
  };
  return { script, header, footer, menu, onKeyDown };
}

/** Under an agent message: the inline commands someone ran from it, and the confirm card of one. */
export function InlineRuns({ messageId }: { messageId: string }) {
  const host = useContext(CodeRunContext);
  const signature = useSyncExternalStore(host ? host.subscribe : noSubscribe, () => host ? host.inlineSignature(messageId) : '');
  if (!host) return null;
  return <>
    {signature && <div className="inline-runs">{signature.split('\0').map(script => <InlineRun key={script} host={host} messageId={messageId} script={script}/>)}</div>}
    <HomeRuns host={host} messageId={messageId}/>
  </>;
}

/**
 * Under the conversation's last message, once its turn is over: one button
 * per command it hands back to the person ("run in the home checkout of api:
 * `make playground`", a login). A click runs it in a shell there (or types
 * it at the prompt for review); the ⌄ beside it pastes it without running,
 * runs it elsewhere, opens the person's own terminal, or copies it. Once it ran, "Tell Claude it's done".
 */
function HomeRuns({ host, messageId }: { host: CodeRuns; messageId: string }) {
  const signature = useSyncExternalStore(host.subscribe, () => host.homeSignature(messageId));
  const offers = JSON.parse(signature) as { offer: HomeOffer; place: HomePlace }[];
  if (!offers.length) return null;
  return <div className="home-runs" role="group" aria-label="Commands for you to run">
    {offers.map(({ offer, place }) => <HomeRun key={offer.command} host={host} messageId={messageId} offer={offer} place={place}/>)}
  </div>;
}
function HomeRun({ host, messageId, offer, place }: { host: CodeRuns; messageId: string; offer: HomeOffer; place: HomePlace }) {
  const view = JSON.parse(useSyncExternalStore(host.subscribe, () => host.signature(messageId, offer.command))) as RunView;
  const setup = place.kind === 'repository' && !place.repository.home;
  const name = place.kind === 'repository' ? place.repository.name : '';
  const label = setup ? `Create ${name} home and run` : placeLabel(place);
  const others = (): MenuItem[] => [
    { label: PASTE_LABEL, icon: 'paste', run: () => void host.runIn(messageId, offer.command, place, 'paste') },
    ...host.homePlaces().filter(other => placeLabel(other) !== placeLabel(place)).map(other => ({ label: placeLabel(other), icon: 'home', run: () => void host.runIn(messageId, offer.command, other) })),
    { label: 'Open in Terminal (your app)', icon: 'external', run: () => void host.openOwnTerminal(offer.command, place) },
    { label: 'Copy command', icon: 'copy', run: () => void copyText(place.kind === 'repository' ? inHome(offer.command).command : offer.command, 'Copied the command') },
  ];
  return <span className="home-run-row">
    <span className="home-run-split" role="group" aria-label={`${label}: ${offer.command}`}>
      <button type="button" className={'home-run' + (setup ? ' is-setup' : '')}
        title={setup ? `Clone ${name}’s home checkout (your own copy, outside the workspace), then ${runsAtOnce(offer.command) ? 'run this in a shell there' : 'type this in a shell there for you to review'}` : terminalRunTitle(place.kind === 'repository' ? inHome(offer.command).command : offer.command, host.placeName(place))}
        onClick={() => void host.runIn(messageId, offer.command, place, 'run', true)}>
        <Icon name="home" size={12}/><span>{label}</span><code>{offer.command.split('\n')[0]}{offer.command.includes('\n') ? ' …' : ''}</code>
      </button>
      <button type="button" className="home-run-more" aria-label={`Other ways to run ${offer.command.split('\n')[0]}`} aria-haspopup="menu" title="Paste it without running, another home, your own terminal, or copy it"
        onClick={event => { const box = event.currentTarget.getBoundingClientRect(); openContextMenu({ x: box.left, y: box.bottom + 4 }, others(), { keyboard: event.detail === 0, target: event.currentTarget, label: 'Run' }); }}><Icon name="chevron" size={10}/></button>
    </span>
    {view.terminal && <TerminalStatus host={host} messageId={messageId} script={offer.command} run={view.terminal}/>}
  </span>;
}
const terminalText: Record<TerminalState, (run: TerminalRunView) => string> = {
  starting: run => `Waiting for the shell (${run.where})…`,
  running: run => `Running in the terminal (${run.where})`,
  typed: run => `Typed in the terminal (${run.where}): review and press Return`,
  finished: run => `Finished in ${run.where} · exit ${run.exitCode}`,
};
/** A run in the Terminal panel: waiting for the shell, running or waiting for Return, then how it ended, and "Tell Claude it's done". */
function TerminalStatus({ host, messageId, script, run }: { host: CodeRuns; messageId: string; script: string; run: TerminalRunView }) {
  const finished = run.state === 'finished';
  const tone = !finished ? 'running' : run.exitCode === 0 ? 'ok' : 'failed';
  return <span className="code-run-terminal">
    <span className={`code-run-chip tone-${tone}`} role="status">{!finished ? <span className="code-run-dot" aria-hidden/> : <Icon name="terminal" size={10}/>}{terminalText[run.state](run)}</span>
    {run.told ? <span className="code-run-chip" role="status"><Icon name="send" size={10}/>Told {host.agentName}</span>
      : run.reporting ? <span className="code-run-chip" role="status">Reporting to {host.agentName}…</span>
      : finished && <button type="button" className="code-run-link" title={`Sends “${doneMessage(script, run.exitCode).split('\n')[0]}” to ${host.agentName}`} onClick={() => void host.tellDone(messageId, script)}><Icon name="send" size={10}/>Tell {host.agentName} it’s done</button>}
  </span>;
}
function InlineRun({ host, messageId, script }: { host: CodeRuns; messageId: string; script: string }) {
  const signature = useSyncExternalStore(host.subscribe, () => host.signature(messageId, script));
  const view = JSON.parse(signature) as RunView;
  return <div className="inline-run">
    {view.confirming ? <RunConfirm host={host} messageId={messageId} script={script}/> : <code className="inline-run-command">{script}</code>}
    <RunStatus host={host} view={view} script={script} messageId={messageId}/>
  </div>;
}

function RunStatus({ host, view, script, messageId }: { host: CodeRuns; view: RunView; script: string; messageId: string }) {
  const { agent, self } = view;
  if (!agent && !self && !view.terminal) return null;
  const name = host.agentName;
  const agentText = !agent ? '' : agent.state === 'queued' ? (agent.start ? 'Queued — starts after the current turn' : 'Queued — runs after the current turn')
    : agent.state === 'running' ? (agent.start ? `Sent to ${name} · starting…` : `Sent to ${name} · running…`)
    : agent.state === 'background' ? `Sent to ${name} · running in the background${agent.port ? ` · port ${agent.port}` : ''}`
    : agent.state === 'ok' ? `Sent to ${name} · ran ✓`
    : agent.state === 'failed' ? `Sent to ${name} · failed ✗${agent.exitCode !== undefined ? ` exit ${agent.exitCode}` : ''}`
    : `Sent to ${name}`;
  // A server's run never ends: "Running · port 3000" says where it listens, not that it is still on its way to a result.
  const selfText = !self ? '' : self.state === 'running' ? (self.server && self.port ? `Running · port ${self.port}` : self.server ? 'Running · starting…' : 'Running') : self.state === 'ok' ? `✓ exit ${self.exitCode ?? 0}` : self.state === 'stopped' ? `■ Stopped${self.signal ? ` (${self.signal})` : ''}` : `✗ exit ${self.exitCode ?? '?'}`;
  const agentTone = agent?.state === 'background' ? 'running' : agent?.state;
  return <div className="code-run-status">
    {view.terminal && <TerminalStatus host={host} messageId={messageId} script={script} run={view.terminal}/>}
    {agent && <span className={`code-run-chip tone-${agentTone}`} role="status" title={agent.state === 'queued' ? `${name} gets it when the turn in progress ends.` : agent.start ? `Asked ${name} to start this in the background.` : `Asked ${name} to run this.`}>
      {agent.state === 'running' || agent.state === 'queued' || agent.state === 'background' ? <span className="code-run-dot" aria-hidden/> : <Icon name="send" size={10}/>}{agentText}</span>}
    {agent?.state === 'background' && agent.port && !self && <button type="button" className="code-run-link" onClick={() => host.openPreview(`http://localhost:${agent.port}`)}><Icon name="eye" size={10}/>Open preview</button>}
    {self && <span className="code-run-self">
      <span className={`code-run-chip tone-${self.state}`} role="status" title={self.action ? `You started the workspace action ${self.action}` : self.folder ? `You ran this in ${self.folder}` : 'You ran this'}>
        {self.state === 'running' ? <span className="code-run-dot" aria-hidden/> : <Icon name="terminal" size={10}/>}{selfText}{self.action ? <small>action {self.action}</small> : self.folder && <small>in {self.folder}</small>}</span>
      {self.state === 'running' && self.url && <button type="button" className="code-run-link" title={self.url} onClick={() => host.openPreview(self.url!)}><Icon name="eye" size={10}/>Open preview</button>}
      {host.canOpenOutput() && self.commandId && <button type="button" className="code-run-link" onClick={() => host.openOutput(self.commandId)}>Show output</button>}
      {self.state === 'running' && <button type="button" className="code-run-link" onClick={() => host.stop(self.commandId, self.action)}><Icon name="stop" size={10}/>Stop</button>}
      {self.state === 'failed' && <button type="button" className="code-run-fix" onClick={() => void host.askToFix(self.commandId, script)}><Icon name="sparkle" size={10}/>Ask {name} to fix</button>}
    </span>}
  </div>;
}

/**
 * Run myself, before it runs: the exact script (editable, and to be edited
 * when it holds placeholders), the folder, Run and Cancel. Enter runs, Escape
 * cancels; Shift+Enter is a new line.
 */
function RunConfirm({ host, messageId, script }: { host: CodeRuns; messageId: string; script: string }) {
  const [draft, setDraft] = useState(script);
  const [folder, setFolder] = useState(() => host.defaultFolder());
  const [error, setError] = useState('');
  const [starting, setStarting] = useState(false);
  const form = useRef<HTMLFormElement>(null), area = useRef<HTMLTextAreaElement>(null), run = useRef<HTMLButtonElement>(null);
  // What the conversation allows can change while the card is open (it connects, it ends, its mode changes).
  useSyncExternalStore(host.subscribe, () => `${host.whyNotRunMyself()}|${host.readOnlyMode()}|${host.folders().length}|${host.actionFor(script)?.key || ''}`);
  const placeholders = placeholdersIn(script), left = placeholders.filter(value => draft.includes(value));
  const why = !draft.trim() ? 'There is nothing to run.' : left.length ? `Replace ${left.join(', ')} first.` : host.whyNotRunMyself();
  const mode = host.readOnlyMode();
  const folders = host.folders();
  // The same command as a workspace action: starting the action gets the workspace's own port, and it shows in Run and Preview.
  const action = host.actionFor(draft), server = !action && serverLike(draft).server;
  const openedAt = host.confirmOpenedAt(messageId, script);
  useLayoutEffect(() => {
    form.current?.scrollIntoView({ block: 'nearest' });
    if (placeholders.length && area.current) {
      // Select the first thing to fill in.
      const at = script.indexOf(placeholders[0]);
      area.current.focus({ preventScroll: true }); area.current.setSelectionRange(at, at + placeholders[0].length);
    } else run.current?.focus({ preventScroll: true });
  }, [openedAt]);
  useEffect(() => { if (area.current) { area.current.style.height = 'auto'; area.current.style.height = `${Math.min(area.current.scrollHeight + 2, 260)}px`; } }, [draft]);
  const cancel = () => { host.closeConfirm(); form.current?.closest<HTMLElement>('.code-block,.message')?.focus?.({ preventScroll: true }); };
  const submit = async () => {
    if (why || starting) { if (why) setError(why); return; }
    setStarting(true); setError('');
    const problem = await host.runMyself(messageId, script, draft.replace(/\s+$/, ''), folder);
    setStarting(false);
    if (problem) setError(`Could not start: ${problem}`);
  };
  return <form ref={form} className="code-run-confirm" aria-label="Run this yourself" onSubmit={event => { event.preventDefault(); void submit(); }}
    onKeyDown={event => {
      if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); cancel(); }
      else if (event.key === 'Enter' && !event.shiftKey && !event.metaKey && !event.ctrlKey && !event.altKey && (event.target as HTMLElement).tagName !== 'BUTTON' && (event.target as HTMLElement).tagName !== 'SELECT') { event.preventDefault(); event.stopPropagation(); void submit(); }
    }}>
    <div className="code-run-confirm-head"><strong>Run myself</strong><small>Runs as you, in this conversation’s command output. {host.agentName} is not involved.</small></div>
    {action && <p className="code-run-note" role="note"><Icon name="play" size={12}/><span>Starts the workspace action <strong>{action.key}</strong>{action.port ? ` on port ${action.port}` : ''}: it gets the workspace’s own port, and shows in Run and Preview.</span></p>}
    {server && <p className="code-run-note" role="note"><Icon name="play" size={12}/><span>This keeps running (a server or a watcher). It runs in the background; stop it here or in Processes.</span></p>}
    {mode && <p className="code-run-warning" role="note"><Icon name="alert" size={12}/>This conversation is {mode === 'plan' ? 'in Plan mode' : 'read-only'}: {host.agentName} may not change anything here, but this command runs as you and can.</p>}
    {placeholders.length > 0 && <p className={`code-run-warning${left.length ? '' : ' is-done'}`} role="note"><Icon name={left.length ? 'pencil' : 'check'} size={12}/>{left.length ? <>Fill in {left.map((value, i) => <code key={i}>{value}</code>)} before running.</> : 'Placeholders filled in.'}</p>}
    <label className="code-run-field"><span>Script</span>
      <textarea ref={area} value={draft} spellCheck={false} autoCapitalize="off" autoCorrect="off" rows={Math.min(10, draft.split('\n').length)} onChange={event => { setDraft(event.target.value); setError(''); }}/></label>
    {folders.length > 0 && !action && <label className="code-run-field"><span>Folder</span>
      <select value={folder} onChange={event => setFolder(event.target.value)}>{folders.map(entry => <option key={entry.path} value={entry.path}>{entry.label} — {shortPath(entry.path)}</option>)}</select></label>}
    {error && <p className="code-run-error" role="alert">{error}</p>}
    <div className="code-run-confirm-actions">
      <button type="button" onClick={cancel}>Cancel <kbd>Esc</kbd></button>
      <button ref={run} type="submit" className="primary" disabled={starting} aria-disabled={!!why} title={why || (action ? `Start the workspace action ${action.key}` : 'Run this script in the chosen folder')}>{starting ? 'Starting…' : <>{action ? 'Start action' : 'Run'} <kbd>↵</kbd></>}</button>
    </div>
  </form>;
}
