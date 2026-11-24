import { useEffect, useState } from 'react';
import type { InfoResponse, Session } from '@lazybox/protocol';
import type { RunnerClient } from '@lazybox/client-core';
import { copyText, registerContextMenu, showMenuNotice } from './ContextMenu';
import type { MenuItem } from './context-menu-model';
import { runAction, useAction } from './actions';
import { useRunner } from './RunnerContext';
import { showToast } from './toasts';
import { cdCommand, DEFAULT_TERMINAL, folderFor, legacyTerminalCommand, parseTerminalChoice, resumeCommand, TERMINAL_APPS, terminalFor, terminalTitle, type TerminalChoice } from './handoff-model';
import { SettingsGroup, SettingsRow } from './settings-parts';

/**
 * Handing work to the person's own tools: "Continue in Claude Code / Codex…"
 * copies the provider's resume command, and "Open in Terminal" opens their
 * own terminal app at a workspace's folder (the desktop app, through its
 * host; a browser gets the `cd` command to paste). The model is pure, in
 * `handoff-model.ts`.
 */

/**
 * What the desktop app adds: its installed terminals, and opening one at a
 * folder. The page names a folder and a terminal, never a command line: a
 * custom one is the host's own setting, which `setCustomCommand` saves only
 * once the person allowed it in the host's native confirmation (it rejects
 * otherwise) and `customCommand` reads back ('' when none).
 */
export type ExternalTerminalHost = {
  installed(): Promise<string[]>;
  open(request: { path: string; app: string }): Promise<unknown>;
  customCommand(): Promise<string>;
  setCustomCommand(command: string): Promise<string>;
};
let terminalHost: ExternalTerminalHost | undefined;
export function configureExternalTerminal(host: ExternalTerminalHost | undefined) { terminalHost = host; installedApps = undefined; }
/** Whether this window can open the person's own terminal app (the desktop app), rather than offer a `cd` to paste. */
export const hasTerminalHost = () => !!terminalHost;
let installedApps: Promise<string[]> | undefined;
const installed = () => (installedApps ??= terminalHost ? terminalHost.installed().catch(() => []) : Promise.resolve([]));
const customCommand = () => terminalHost ? terminalHost.customCommand().catch(() => '') : Promise.resolve('');

// Which terminal is installed is this computer's, not the Runner's: kept in this browser's storage.
const TERMINAL_PREFERENCE = 'lazybox:external-terminal';
export function readTerminalChoice(): TerminalChoice {
  try { return parseTerminalChoice(globalThis.localStorage?.getItem(TERMINAL_PREFERENCE)); } catch { return DEFAULT_TERMINAL; }
}
function readLegacyCommand(): string {
  try { return legacyTerminalCommand(globalThis.localStorage?.getItem(TERMINAL_PREFERENCE)); } catch { return ''; }
}
export function writeTerminalChoice(choice: TerminalChoice) {
  try { globalThis.localStorage?.setItem(TERMINAL_PREFERENCE, JSON.stringify(choice)); } catch { /* kept for this window only */ }
}

/** A workspace's folder on the Runner, read once per workspace. */
const roots = new Map<string, Promise<string>>();
export function workspaceFolder(api: RunnerClient, workspaceId: string): Promise<string> {
  let root = roots.get(workspaceId);
  if (!root) {
    root = api.getCollection({ collectionId: workspaceId }, { timeoutMs: 8000 }).then(result => result.collection?.root || '');
    root.catch(() => roots.delete(workspaceId));
    roots.set(workspaceId, root);
  }
  return root;
}

/** Shows the `cd` command where nothing can open a terminal (a browser): copied, and on screen to copy again. */
async function offerCd(folder: string) {
  const command = cdCommand(folder);
  let copied = true;
  try { await navigator.clipboard.writeText(command); } catch { copied = false; }
  showToast({ id: 'open-in-terminal', tone: 'info', timeout: 12000, title: copied ? 'Copied — paste it in your terminal' : 'Paste this in your terminal', body: command,
    button: { label: 'Copy', run: () => void copyText(command, 'Copied the cd command') } });
}

/**
 * Opens the person's terminal at a folder: their chosen app (Settings →
 * Agents & workspaces), else the first installed one. Without a desktop host, the
 * `cd` command to paste.
 */
export async function openInTerminal(folder: string): Promise<void> {
  if (!folder) { showMenuNotice('This has no folder yet'); return; }
  if (!terminalHost) { await offerCd(folder); return; }
  const choice = readTerminalChoice();
  const custom = choice.app === 'custom' ? await customCommand() : '';
  const target = terminalFor(choice, await installed(), custom);
  if (!target) {
    showToast({ id: 'open-in-terminal', tone: 'failed', title: choice.app === 'custom' ? 'Your terminal command is empty' : 'No terminal found', body: 'Choose yours in Settings → Agents & workspaces.', action: { label: 'Open settings', run: () => void runAction('settings.open', 'agents') } });
    return;
  }
  try { await terminalHost.open({ path: folder, app: target.app }); }
  catch (error) {
    showToast({ id: 'open-in-terminal', tone: 'failed', title: `Could not open ${terminalTitle(target.app, custom)}`, body: String(error).replace(/^Error: /, ''), button: { label: 'Copy cd', run: () => void copyText(cdCommand(folder), 'Copied the cd command') } });
  }
}

/** The folder a conversation works in: its own, else its workspace's. */
async function conversationFolder(api: RunnerClient, session: Session): Promise<string> {
  return session.workingDirectory || (session.collectionId ? await workspaceFolder(api, session.collectionId) : '');
}

/** "Continue in Claude Code…": copies the command, says where to paste it. */
export function continueItem(session: Session | undefined, info?: InfoResponse): MenuItem | undefined {
  if (!session) return undefined;
  const resume = resumeCommand(session, info?.profiles.find(p => p.id === session.profileId));
  if (!resume) return undefined;
  return {
    label: `Continue in ${resume.cli}…`, icon: 'terminal', disabled: resume.unavailable || false,
    run: () => {
      if (!resume.command) return;
      void copyText(resume.command, `Copied — paste it in your terminal${session.busy && !session.closed ? '. The agent is still working here: let it finish first' : ''}`);
    },
  };
}

const openFolder = (folder: Promise<string>) => void folder.then(openInTerminal, error => showMenuNotice(`The folder is unknown: ${String(error)}`));
function terminalItem(folder: () => Promise<string>, disabled: string | false = false): MenuItem {
  return { label: terminalHost ? 'Open in Terminal' : 'Open in Terminal…', icon: 'terminal', disabled, run: () => openFolder(folder()) };
}

/** A workspace row's Open in Terminal: in its own list, so its ⋯ and right-click menus stay the same. */
export function workspaceTerminalItem(api: RunnerClient, workspaceId: string): MenuItem {
  return terminalItem(() => workspaceFolder(api, workspaceId));
}

/** The handoff items of a conversation: open its folder in a terminal, continue it in its CLI. */
export function conversationHandoffItems(api: RunnerClient, session: Session | undefined, info: InfoResponse | undefined): MenuItem[] {
  if (!session || (session.purpose && session.purpose !== 'coding')) return [];
  return [
    terminalItem(() => conversationFolder(api, session), !session.workingDirectory && !session.collectionId && 'It has no folder yet'),
    continueItem(session, info),
  ].filter((item): item is MenuItem => !!item);
}

// The Files menu reads the Runner of whichever window mounted it last; every window has one.
const live: { api?: RunnerClient } = {};
let mounted = 0, unregister: (() => void) | undefined;
function registerMenus(): () => void {
  const removals = [
    registerContextMenu<{ path: string; directory: boolean; workspaceId: string }>('file', context => live.api ? [
      terminalItem(async () => folderFor(await workspaceFolder(live.api!, context.detail.workspaceId), context.detail.path, context.detail.directory)),
    ] : []),
  ];
  return () => removals.forEach(remove => remove());
}

/**
 * Adds "Open in Terminal" to the menus of Files rows. Conversations (every
 * list's menu, `ConversationActions.tsx`), a chat's header (`withHandoff`)
 * and workspace rows (`workspaceTerminalItem`) list the handoff items in
 * their own menus, so their ⋯ and right-click menus stay the same. Any number of components may mount it; the items are added once.
 */
export function useHandoffMenus() {
  const { api } = useRunner();
  live.api = api;
  useEffect(() => {
    if (mounted++ === 0) unregister = registerMenus();
    return () => { if (--mounted === 0) { unregister?.(); unregister = undefined; } };
  }, []);
}

/** The palette's "Open in Terminal": the conversation in front's folder, else its workspace's. */
export function useTerminalAction(workspaceId: string, sessionId: string) {
  const { api, info } = useRunner();
  const current = info?.sessions.find(s => s.id === sessionId);
  useAction('terminal.external', () => openFolder(current && current.collectionId === workspaceId ? conversationFolder(api, current) : workspaceFolder(api, workspaceId)), () => !!workspaceId);
}

/** Settings → Agents & workspaces → Your terminal: which app "Open in Terminal" opens. */
export function TerminalSettings() {
  const [choice, setChoice] = useState(readTerminalChoice);
  const [apps, setApps] = useState<string[]>();
  // The command line the host keeps, and the one being typed: saved only
  // through the host's own confirmation. One an earlier version kept in this
  // page's storage is offered as the draft, to save the same way.
  const [saved, setSaved] = useState<string>();
  const [draft, setDraft] = useState(readLegacyCommand);
  const [saving, setSaving] = useState(false);
  const [note, setNote] = useState<{ error: boolean; text: string }>();
  useEffect(() => { void installed().then(setApps); }, []);
  useEffect(() => { void customCommand().then(command => { setSaved(command); if (command) setDraft(command); }); }, []);
  const change = (next: TerminalChoice) => { setChoice(next); writeTerminalChoice(next); };
  const saveCommand = async () => {
    if (!terminalHost) return;
    setSaving(true); setNote(undefined);
    try { const kept = await terminalHost.setCustomCommand(draft); setSaved(kept); setDraft(kept); setNote({ error: false, text: kept ? 'Saved.' : 'Cleared.' }); }
    catch (error) { setNote({ error: true, text: String(error).replace(/^Error: /, '') }); }
    finally { setSaving(false); }
  };
  const automatic = terminalFor({ app: '' }, apps || []);
  if (!terminalHost) return <SettingsGroup title="Your terminal" description={<>“Open in Terminal” copies a <code>cd</code> to the workspace’s folder for you to paste. In the desktop app it opens your terminal there.</>}><></></SettingsGroup>;
  return <SettingsGroup title="Your terminal" description={<>“Open in Terminal” (on a workspace, a conversation, a file, or from the command palette) opens this app at the workspace’s folder. <code>lazybox run</code> and <code>lazybox log</code> typed there show in the workspace’s console.</>}>
    <SettingsRow label="Terminal app" htmlFor="external-terminal">
      <select id="external-terminal" value={choice.app} onChange={event => change({ ...choice, app: event.target.value as TerminalChoice['app'] })}>
        <option value="">Automatic{automatic ? ` · ${terminalTitle(automatic.app)}` : ''}</option>
        {TERMINAL_APPS.map(app => <option key={app.id} value={app.id}>{app.title}{apps && !apps.includes(app.id) ? ' (not installed)' : ''}</option>)}
        <option value="custom">Custom command…</option>
      </select>
    </SettingsRow>
    {choice.app === 'custom' && <SettingsRow label="Command" htmlFor="external-terminal-command" description={<>One line; <code>{'{dir}'}</code> is replaced by the folder, already quoted. It runs in that folder. Lazybox asks you to allow a new command before it keeps it.</>}>
      <form className="settings-inline-form" onSubmit={event => { event.preventDefault(); void saveCommand(); }}>
        <input id="external-terminal-command" className="mono" spellCheck={false} placeholder="alacritty --working-directory {dir}" maxLength={1000} value={draft} onChange={event => { setDraft(event.target.value); setNote(undefined); }}/>
        <button type="submit" disabled={saving || saved === undefined || draft.trim() === saved}>{saving ? 'Saving…' : 'Save'}</button>
      </form>
    </SettingsRow>}
    {choice.app === 'custom' && note && <p className={note.error ? 'settings-error' : 'settings-error tone-note'} role={note.error ? 'alert' : 'status'}>{note.text}</p>}
    {choice.app && choice.app !== 'custom' && apps && !apps.includes(choice.app) && <p className="settings-error tone-note" role="note">{terminalTitle(choice.app)} is not in your Applications folder. Lazybox will still ask macOS to open it.</p>}
  </SettingsGroup>;
}
