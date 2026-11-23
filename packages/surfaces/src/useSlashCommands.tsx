import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode, type RefObject } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { Artifact, Session, Snippet } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { runAction, useActions, type ActionId } from './actions';
import { openArtifact } from './artifact-events';
import { TrackArtifactDialog, UpdateArtifactDialog } from './ArtifactDialogs';
import { requestReviewTray } from './ChangesPanel';
import type { ReviewLauncher } from './review-client';
import { copyText, showMenuNotice } from './ContextMenu';
import type { VimMode } from './composer-vim';
import { conversationArtifacts, trackable } from './MessageArtifactActions';
import { saveSnippetUse } from './snippet-usage';
import { SlashMenu, SLASH_MENU_ID, slashOptionId } from './SlashMenu';
import { useDismiss } from './dismiss';
import { useConfirmCard } from './confirm-card';
import {
  boxAfterCommand, decideSend, findCommand, lazyboxCommands, mergeModels, orderedModelChoices, modelRestartText, paletteCommand, reviewRequest, providerCommands, resolveModel, sameModel, slashCommandList, slashMenu, snippetCommands,
  type CommandCatalog, type ProviderModel, type SlashChoice, type SlashCommand, type SlashContext, type SlashMenuState,
} from './slash-commands';
import { useProviderAvailability } from './useProviderAvailability';
import { modelName } from './conversation-model';
import { openUsage } from './TelemetryPanel';
import { requestOrchestrate } from './OrchestratorBadge';
import type { TranscriptMessage } from './transcript-model';
import { humanError } from './human-error';

/** What the chat lets its slash commands do; each is the same thing its menus and buttons do. */
export type SlashActions = {
  /** Send text as its own message (an agent's command), whatever the draft. True once accepted. */
  sendText(text: string): Promise<boolean>;
  interrupt(): void;
  /** Live where the agent can, else the restart prompt. */
  setPermission(mode: string): void;
  switchSubscription(): void;
  /** Give the agent the browser (BROWSER.md): it restarts with it when it has none. */
  giveBrowser(): void;
  /** A conversation that continues in a new agent (a restart with another model). */
  adopt(next: Session): void;
  /** A new conversation in the same workspace; this one is kept. */
  clear(): Promise<void>;
  rename(title: string): Promise<void>;
};

type Options = {
  api: RunnerClient;
  viewState?: ViewStateStore;
  enabled: boolean;
  session?: Session;
  sessionID: string;
  agentName: string;
  /** "claude", "codex" or an ACP agent (providers.ts). */
  provider: string;
  catalog: CommandCatalog;
  /** The model the conversation runs on now, as best known. */
  model: string;
  conversation: readonly TranscriptMessage[];
  shellBlock: boolean;
  detail: string;
  subscriptions: number;
  canCompose: boolean;
  message: string;
  setText(text: string, snippetKey?: string): void;
  textarea: RefObject<HTMLTextAreaElement | null>;
  /** This is the chat in front: the palette's commands mean it. */
  front(): boolean;
  actions: SlashActions;
  /**
   * Who reviews `/review`: the chat's one launcher, shared with the header's
   * Review button, so the two never start two reviews at once.
   */
  reviewLauncher: ReviewLauncher;
  onError(message: string): void;
};

/** The palette's names for Lazybox's commands (actions.ts). */
const ACTIONS: Partial<Record<string, ActionId>> = {
  model: 'chat.model', permissions: 'chat.permissions', subscription: 'chat.subscription', browser: 'chat.browser', compact: 'chat.compact', clear: 'chat.clear',
  context: 'chat.usage', review: 'chat.review', artifact: 'chat.artifact', stop: 'chat.stop', rename: 'chat.rename', orchestrate: 'chat.orchestrate',
};
const PERMISSION_WORDS: Record<string, string> = { ask: 'ask', approve: 'ask', default: 'ask', 'read-only': 'read-only', readonly: 'read-only', 'read only': 'read-only', read: 'read-only', plan: 'read-only', 'full access': 'yolo', yolo: 'yolo', bypass: 'yolo' };

type Tracking = { kind: 'track'; message: TranscriptMessage; existing: Artifact[] } | { kind: 'update'; message: TranscriptMessage; artifact: Artifact };

/**
 * The slash commands of one chat: the menu over its message box, what Enter
 * does with a `/command`, the commands themselves, and their palette entries.
 */
export function useSlashCommands(o: Options) {
  const latest = useRef(o); latest.current = o;
  const { api, session, sessionID, catalog, provider } = o;
  const workspaceId = session?.collectionId || '';

  // The transcript also uses the catalog to fold saved snippet bodies on replay.
  const [snippetRows, setSnippetRows] = useState<Snippet[]>([]);
  useEffect(() => {
    if (!o.enabled) return;
    const controller = new AbortController();
    void api.listSnippets({}, { signal: controller.signal }).then(list => {
      if (!controller.signal.aborted) setSnippetRows(list.snippets);
    }).catch(() => {});
    return () => controller.abort();
  }, [api, o.enabled, sessionID]);
  const [nextSnippets, setNextSnippets] = useState<string[]>([]);
  const typingCommand = o.message.startsWith('/');
  useEffect(() => {
    if (!typingCommand || !o.enabled || !session?.conversationId) return;
    const controller = new AbortController();
    void api.listPromptHistory({ conversationId: session.conversationId, limit: 1 }, { signal: controller.signal }).then(history => {
      if (!controller.signal.aborted) setNextSnippets(history.nextSnippets);
    }).catch(() => {});
    return () => controller.abort();
  }, [api, typingCommand, o.enabled, session?.conversationId]);

  // `/review`: another agent reviews this conversation's work (or, from a
  // review conversation, the work it reviews: a review is never reviewed).
  // The launcher is the chat's (ReviewFromChat.tsx), which reports its errors.
  const { reviewLauncher } = o;
  const live = !!session && !session.closed;
  const lastAgentMessage = useMemo(() => [...o.conversation].reverse().find(trackable), [o.conversation]);
  // The Runner's models for this provider: what its program lists, else built in.
  // An agent started by an older Lazybox lists none of its own; these stand in.
  const availability = useProviderAvailability(o.enabled);
  const providerModels: ProviderModel[] = useMemo(() => (availability.report?.providers.find(p => p.provider === provider)?.models || [])
    .map(m => ({ value: m.value, displayName: m.displayName, description: m.description, ...(m.effortLevels?.length ? { effortLevels: m.effortLevels } : {}) })), [availability.report, provider]);
  const models = useMemo(() => mergeModels(providerModels, catalog.models), [providerModels, catalog.models]);
  const context: SlashContext = {
    provider, agentName: o.agentName, live, busy: !!session?.busy, coding: !!session && (!session.purpose || session.purpose === 'coding'), workspace: !!workspaceId,
    permission: session?.permission || '', planWorkflow: session?.workflow === 'plan', livePermission: !!session?.capabilities?.livePermission,
    model: o.model, effort: session?.effort, liveModel: !!session?.capabilities?.liveModel, resumable: !!session?.capabilities?.resume, models: catalog.models, providerModels,
    agentMessage: !!lastAgentMessage, shellBlock: o.shellBlock, compact: provider === 'claude' || provider === 'codex', detail: o.detail, subscriptions: o.subscriptions,
    reviewers: workspaceId && !reviewLauncher.unavailable ? reviewLauncher.choices : [],
    browser: session?.browserAccess || '',
  };
  const lazybox = lazyboxCommands(context);
  const modelsRef = useRef(models); modelsRef.current = models;
  const commands: SlashCommand[] = slashCommandList(lazybox, providerCommands(catalog, lazybox, o.canCompose ? undefined : 'The conversation cannot take a message now'), snippetCommands(snippetRows, nextSnippets));
  const byName = (name: string) => lazybox.find(command => command.name === name)!;

  // ---- The menu --------------------------------------------------------
  const [dismissed, setDismissed] = useState(false);
  const [vimMode, setVimMode] = useState<VimMode | undefined>();
  // Esc's dismissal lasts while the box holds a command. It is reset during
  // render, not in an effect: an effect that sets state on every keystroke
  // queues a second, default-priority render per key, and keys that land
  // faster than that render runs (fast or scripted typing, a busy machine)
  // pile those up until React stops with "Maximum update depth exceeded".
  if (dismissed && !o.message.startsWith('/')) setDismissed(false);
  const menu = o.enabled && (!vimMode || vimMode === 'insert') ? slashMenu(o.message, commands, dismissed) : null;
  const menuKey = menu ? `${menu.kind}:${menu.kind === 'hint' ? menu.command.key : menu.kind === 'choices' ? `${menu.command.key}:${menu.query}` : menu.query}:${menu.kind === 'hint' ? 0 : menu.items.length}` : '';
  const [picked, setPicked] = useState({ key: '', index: 0 });
  const selected = menu && menu.kind !== 'hint' ? (picked.key === menuKey ? Math.min(picked.index, menu.items.length - 1) : Math.max(0, menu.best)) : -1;
  const select = (index: number) => setPicked({ key: menuKey, index });
  // A menu opened again starts on its best entry, not where the last one was left.
  useEffect(() => { if (!menuKey) setPicked(previous => previous.key ? { key: '', index: 0 } : previous); }, [menuKey]);

  // A draft set aside while a command from the palette or a menu borrows the box.
  const stash = useRef<string | null>(null);
  const setText = (text: string, snippetKey?: string) => {
    latest.current.setText(text, snippetKey);
    // The caret goes to the end a frame later, once the box holds the text — unless
    // the person typed on meanwhile: what they typed keeps its caret.
    const box = latest.current.textarea.current;
    if (box && document.activeElement !== box) box.focus({ preventScroll: true });
    requestAnimationFrame(() => { const box = latest.current.textarea.current; if (box && box.value === text) box.setSelectionRange(text.length, text.length); });
  };
  const finish = () => { const back = stash.current ?? ''; stash.current = null; setText(back); };
  /** Put `/name ` in the box (its choices show), keeping any draft to come back after. */
  const borrow = (text: string) => {
    const current = latest.current.message;
    // What the person typed is theirs to keep (Esc leaves it); a box borrowed from the palette or a menu is given back as it was.
    if (stash.current === null && !current.startsWith('/')) stash.current = current;
    setDismissed(false); setText(text);
  };

  // ---- Running ---------------------------------------------------------
  // A model switch that restarts the agent: asked once, and, during a turn, done when it ends.
  const [restart, setRestart] = useState<{ model: string; effort: string; label: string; queued: boolean }>();
  const [tracking, setTracking] = useState<Tracking>();
  const notice = (text: string) => showMenuNotice(text);
  const switchModel = async (value: string) => {
    const [, name, effort = ''] = /^(.*?)(?:\s+(none|minimal|low|medium|high|xhigh|max|ultra))?$/.exec(value.trim()) ?? [];
    const model = resolveModel(name || '', modelsRef.current);
    const { session: current, sessionID: id, onError } = latest.current;
    if (!model || !current) return;
    const choice = byNameLatest('model')?.choices?.find(c => c.value.toLowerCase() === model.toLowerCase());
    const label = [choice?.label || modelName(model), effort].filter(Boolean).join(' · ');
    // The model it runs on already: nothing to switch, nothing to confirm.
    if ((!effort || current.effort === effort) && (choice?.current || sameModel({ value: model, displayName: modelName(model), description: '' }, latest.current.model))) { setRestart(undefined); notice(`Already on ${label}`); return; }
    if (current.capabilities?.liveModel) {
      try {
        await api.apply({ sessionId: id, operationId: crypto.randomUUID(), operation: { case: 'model', value: { model, effort } } });
        setRestart(undefined);
        if (current.busy) notice(`${label} from the next turn; this one finishes on the model it started with`);
      } catch (e) { onError(humanError(e)); }
      return;
    }
    if (!current.capabilities?.resume) { onError(`${latest.current.agentName} cannot change models during a conversation. Start a new conversation with ${label}.`); return; }
    setRestart({ model, effort, label, queued: false });
  };
  // A restart asked for during a turn runs once the turn ends.
  useEffect(() => { if (restart?.queued && session && !session.busy && !session.closed) void restartWithModel(restart.model, restart.effort); }, [restart, session?.busy, session?.closed]);
  const restartWithModel = async (model: string, effort: string) => {
    setRestart(undefined);
    try {
      const result = await api.restartWithModel({ sessionId: latest.current.sessionID, model, effort });
      if (!result.session) throw new Error('Runner did not return the restarted conversation');
      latest.current.actions.adopt(result.session);
    } catch (e) { latest.current.onError(humanError(e)); }
  };
  const startTracking = async () => {
    const message = lastAgentMessage, current = latest.current.session;
    if (!message || !current?.collectionId) return;
    const scope = { collectionId: current.collectionId, sessionId: latest.current.sessionID, conversationId: current.conversationId };
    let existing: Artifact[] = [];
    try { existing = conversationArtifacts((await api.listArtifacts({ collectionId: current.collectionId }, { timeoutMs: 5000 })).artifacts, scope).filter(artifact => !artifact.format || artifact.format === 'markdown'); } catch { /* offered without updates */ }
    setTracking({ kind: 'track', message, existing });
  };

  /**
   * Lazybox's command, for real. Choosing one that takes an argument without
   * one shows its choices. `from`: typed in the box (or chosen from its menu),
   * whose `/command` gives way to the draft set aside; or the palette, which
   * leaves the box as it is — the draft is the person's.
   */
  const run = (command: SlashCommand, rawArgs: string, from: 'box' | 'palette' = 'box') => {
    const args = rawArgs.trim();
    const { actions, onError } = latest.current;
    if (command.disabled) { notice(command.disabled); return; }
    // `/review` alone starts the default reviewer: one Enter, not a menu.
    if (!args && (command.choices?.length || command.needsArgument) && !(command.name === 'review' && reviewLauncher.choices.length)) { borrow(`/${command.name} `); return; }
    const back = boxAfterCommand(from, stash.current);
    if (back !== undefined) { stash.current = null; setText(back); }
    switch (command.name) {
      case 'model': void switchModel(args); break;
      case 'effort': {
        const { model, session: current } = latest.current;
        if (!args || args === 'status') { notice(`Reasoning effort: ${current?.effort || 'Provider default'}`); break; }
        const target = byNameLatest('model')?.choices?.find(choice => choice.current)?.value || model;
        if (!target) { notice('Choose a model with /model'); break; }
        if (!/^(none|minimal|low|medium|high|xhigh|max|ultra)$/.test(args)) { notice('Choose a supported level with /effort'); break; }
        void switchModel(`${target} ${args}`);
        break;
      }
      case 'diff': if (!runAction('changes.show')) notice('Open Changes in the workspace window'); break;
      case 'status': {
        const { model, session: current } = latest.current;
        notice([current?.status || 'Connecting', model && modelName(model), current?.effort].filter(Boolean).join(' · '));
        openUsage(latest.current.sessionID, 'context');
        break;
      }
      case 'copy': {
        const index = args ? Number(args) : 1;
        const reply = [...latest.current.conversation].reverse().filter(trackable)[index - 1];
        if (!reply) { notice('No reply at that position'); break; }
        void copyText(reply.text, 'Copied agent reply');
        break;
      }
      case 'permissions': {
        const mode = PERMISSION_WORDS[args.toLowerCase()];
        if (!mode) { onError(`“${args}” is not a permission mode: choose ask, read-only or yolo.`); break; }
        const choice = command.choices?.find(c => c.value === mode);
        if (choice?.disabled) { notice(choice.disabled); break; }
        if (choice?.current) { notice(`Already ${choice.label}`); break; }
        actions.setPermission(mode); break;
      }
      case 'subscription': actions.switchSubscription(); break;
      case 'browser': actions.giveBrowser(); break;
      case 'compact':
        if (provider !== 'codex') { void actions.sendText(args ? `/compact ${args}` : '/compact'); break; }
        if (args) { notice('Codex compaction does not take instructions'); break; }
        void api.apply({ sessionId: latest.current.sessionID, operationId: crypto.randomUUID(), operation: { case: 'compactContext', value: {} } })
          .then(receipt => {
            if (receipt.delivery === 'failed') onError(receipt.error || 'Could not compact context.');
            else notice('Compacting context…');
          })
          .catch(e => onError(humanError(e)));
        break;
      case 'clear': void actions.clear().catch(e => onError(humanError(e))); break;
      case 'skills': case 'apps': case 'plugins':
        void api.apply({ sessionId: latest.current.sessionID, operationId: crypto.randomUUID(), operation: { case: 'listProviderCatalog', value: { section: command.name } } })
          .then(receipt => { if (receipt.delivery === 'failed') onError(receipt.error || `Could not list Codex ${command.name}.`); })
          .catch(e => onError(humanError(e)));
        break;
      case 'mcp':
        void api.apply({ sessionId: latest.current.sessionID, operationId: crypto.randomUUID(), operation: { case: 'listMcpServers', value: {} } })
          .then(receipt => { if (receipt.delivery === 'failed') onError(receipt.error || 'Could not list MCP servers.'); })
          .catch(e => onError(humanError(e)));
        break;
      case 'cost': case 'context':
        if (!openUsage(latest.current.sessionID, command.name)) notice('The usage of this conversation shows in its header');
        break;
      case 'review': {
        const request = reviewRequest(reviewLauncher.choices, args);
        if (request.tray || !request.reviewer) {
          if (!request.tray && args) { onError(`No agent is available to review with: ${reviewLauncher.unavailable || 'sign one in under Settings → Subscriptions'}.`); break; }
          requestReviewTray(workspaceId); if (!runAction('changes.show')) notice('Open the Changes panel to see the review tray'); break;
        }
        if (reviewLauncher.busy) { notice('A review is starting'); break; }
        notice(`Starting ${request.reviewer.label}’s review${request.focus ? ', with your focus' : ''}…`);
        void reviewLauncher.start({ scope: 'changes' }, request.reviewer, request.focus);
        break;
      }
      case 'artifact': void startTracking(); break;
      case 'mission': if (!runAction('missions.open')) notice('Missions open in the main window'); break;
      case 'orchestrate': requestOrchestrate(latest.current.sessionID); break;
      case 'run': if (!runAction('code.run-last')) notice('No command block to run yet'); break;
      case 'stop': actions.interrupt(); break;
      case 'detail': {
        const value = args.toLowerCase();
        if (!['quiet', 'smart', 'verbose'].includes(value)) { onError(`“${args}” is not a detail: choose quiet, smart or verbose.`); break; }
        if (!runAction(`activity.${value}`)) notice(`Already ${value[0].toUpperCase()}${value.slice(1)}`);
        break;
      }
      case 'rename': void actions.rename(args).catch(e => onError(humanError(e))); break;
      case 'help': runAction('shortcuts.open'); break;
    }
  };
  /** An entry of the menu, chosen with Enter (or a click). */
  const choose = (index: number, complete = false) => {
    if (!menu || menu.kind === 'hint') return;
    if (menu.kind === 'choices') {
      const choice: SlashChoice | undefined = menu.items[index];
      if (!choice) return;
      if (choice.disabled) { notice(choice.disabled); return; }
      if (complete) { setText(`/${menu.command.name} ${choice.value}`); return; }
      run(menu.command, choice.value); return;
    }
    const command = menu.items[index]?.command;
    if (!command) return;
    if (command.group === 'snippet') { saveSnippetUse(latest.current.viewState, command.name); stash.current = null; setText(command.body || '', command.name); return; }
    if (command.disabled) { notice(command.disabled); return; }
    if (complete || command.argumentHint || command.choices?.length || command.needsArgument) { setText(`/${command.name} `); return; }
    if (command.group === 'provider') {
      const text = `/${command.name}`;
      finish();
      // A send that did not go gives the command back to the box.
      void latest.current.actions.sendText(text).then(sent => { if (!sent && !latest.current.message) latest.current.setText(text); });
      return;
    }
    run(command, '');
  };

  /** Keys in the message box while the menu shows. True when the menu took the key. */
  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>, mode?: VimMode) => {
    if (mode !== vimMode) setVimMode(mode);
    const native = event.nativeEvent;
    if (native.isComposing || native.keyCode === 229) return false;
    if (!menu || (mode && mode !== 'insert')) return false;
    const count = menu.kind === 'hint' ? 0 : menu.items.length;
    const move = (by: number) => { event.preventDefault(); if (count) select((selected + by + count) % count); return true; };
    if (event.key === 'ArrowDown' || (event.ctrlKey && event.key === 'n')) return move(1);
    if (event.key === 'ArrowUp' || (event.ctrlKey && event.key === 'p')) return move(-1);
    if (event.key === 'Escape') {
      event.preventDefault(); setDismissed(true);
      if (stash.current !== null) finish();
      return true;
    }
    if (menu.kind === 'hint') return false;
    const unavailable = menu.kind === 'commands'
      ? menu.items[selected]?.command.disabled
      : menu.items[selected]?.disabled;
    if (event.key === 'Tab' && !event.altKey && !event.metaKey && !event.ctrlKey) {
      if (unavailable) { setDismissed(true); return false; }
      if (event.shiftKey) return move(-1);
      event.preventDefault(); choose(selected, true); return true;
    }
    if (event.key === 'Enter' && !event.shiftKey && !event.altKey && !event.metaKey && !event.ctrlKey) {
      if (unavailable) { setDismissed(true); return false; }
      if (menu.kind === 'commands' && menu.query && picked.key !== menuKey) {
        const command = menu.items[selected]?.command;
        const typed = menu.query.toLowerCase();
        const exact = findCommand(commands, typed);
        // Fuzzy matches stay available for an explicit selection.
        if (exact ? command !== exact : !command || ![command.name, ...(command.aliases || [])].some(name => name.toLowerCase().startsWith(typed))) return false;
      }
      event.preventDefault(); choose(selected); return true;
    }
    return false;
  };

  /**
   * Before a send: a Lazybox command runs instead, a snippet's name becomes
   * its body. Unrecognized commands use the normal send path.
   */
  const intercept = (text: string): boolean => {
    if (!latest.current.enabled) return false;
    const decision = decideSend(text, commands);
    switch (decision.kind) {
      case 'text': return false;
      case 'lazybox': run(decision.command, decision.args); return true;
      case 'provider': if (decision.command.disabled) { notice(decision.command.disabled); return true; } return false;
      case 'snippet': setText(decision.command.body || '', decision.command.name); return true;
    }
  };

  // ---- The palette -----------------------------------------------------
  const fromPalette = (name: string) => {
    const command = byName(name);
    if (command.disabled) { notice(command.disabled); return; }
    const what = paletteCommand(command);
    if ('borrow' in what) borrow(what.borrow);
    else run(command, what.args, 'palette');
  };
  const can = (name: string) => () => latest.current.enabled && latest.current.front() && !byNameLatest(name)?.disabled;
  const byNameLatest = (name: string) => lazyboxCommands(contextRef.current).find(command => command.name === name);
  const contextRef = useRef(context); contextRef.current = context;
  const handlers: Partial<Record<ActionId, { run(): void; enabled(): boolean } | false>> = {
    'chat.commands': o.enabled && { run: () => borrow('/'), enabled: () => latest.current.enabled && latest.current.front() && latest.current.canCompose },
  };
  for (const [name, id] of Object.entries(ACTIONS)) handlers[id!] = o.enabled && { run: () => fromPalette(name), enabled: can(name) };
  useActions(handlers);

  // ---- What the composer shows ------------------------------------------
  // A press outside the menu and the message box puts the menu away, as Escape
  // does (Escape stays with the box's keys, above), and gives a borrowed box its draft back.
  const menuBox = useRef<HTMLDivElement>(null);
  useDismiss(!!menu, () => {
    setDismissed(true);
    if (stash.current !== null) { const back = stash.current; stash.current = null; latest.current.setText(back); }
  }, [menuBox, o.textarea], { escape: false, restoreFocus: false });
  const menuElement = menu && <div ref={menuBox} style={{ display: 'contents' }}><SlashMenu menu={menu} selected={selected} sessionId={sessionID} agentName={o.agentName}
    onSelect={select} onChoose={index => choose(index)} onClose={() => { setDismissed(true); if (stash.current !== null) finish(); latest.current.textarea.current?.focus(); }}/></div>;
  const above = menuElement;
  const prompt: ReactNode = restart ? <ModelRestartPrompt provider={provider} agentName={o.agentName} label={restart.label} busy={!!session?.busy} queued={restart.queued} returnTo={o.textarea}
    onRestart={() => { if (session?.busy) setRestart({ ...restart, queued: true }); else void restartWithModel(restart.model, restart.effort); }} onCancel={() => setRestart(undefined)}/> : null;
  const dialogs: ReactNode = tracking && workspaceId && <>
    {tracking.kind === 'track' && <TrackArtifactDialog collectionId={workspaceId} content={tracking.message.text}
      source={{ kind: 'message', sessionId: sessionID, conversationId: session?.conversationId, messageId: tracking.message.id, turnId: tracking.message.turnId }} existing={tracking.existing}
      onClose={() => setTracking(undefined)} onTracked={artifact => { setTracking(undefined); if (!openArtifact({ collectionId: artifact.collectionId, artifactId: artifact.id })) notice(`Tracked “${artifact.title}”`); }}
      onUpdateInstead={artifact => setTracking({ kind: 'update', message: tracking.message, artifact })}/>}
    {tracking.kind === 'update' && <UpdateArtifactDialog collectionId={workspaceId} artifactId={tracking.artifact.id} content={tracking.message.text}
      source={{ kind: 'message', sessionId: sessionID, conversationId: session?.conversationId, messageId: tracking.message.id, turnId: tracking.message.turnId }}
      onClose={() => setTracking(undefined)} onSaved={artifact => { setTracking(undefined); if (!openArtifact({ collectionId: artifact.collectionId, artifactId: artifact.id })) notice(`Updated “${artifact.title}”`); }}/>}
  </>;
  const textareaProps = {
    // Still the "Message" textbox (not a combobox): the menu is announced through these.
    'aria-autocomplete': 'list' as const,
    'aria-controls': menu ? SLASH_MENU_ID(sessionID) : undefined,
    'aria-activedescendant': menu && menu.kind !== 'hint' && selected >= 0 ? slashOptionId(sessionID, selected) : undefined,
  };
  const modelCommand = byName('model');
  const choices = modelCommand.choices || [];
  const currentModel = choices.find(choice => choice.current)?.value;
  const modelMenuChoices = orderedModelChoices(choices);
  const effortChoices = (lazybox.find(command => command.name === 'effort')?.choices || []).map(choice => ({ ...choice, value: `${currentModel} ${choice.value}` }));

  return {
    snippetRows, updateSnippetCatalog: setSnippetRows, menu: menu as SlashMenuState | null, onKeyDown, intercept, above, prompt, dialogs, textareaProps,
    /** The model picker of the ⋯ menu and the header: the same choices and switch as `/model`. */
    models: { choices: modelMenuChoices, efforts: effortChoices, live: context.liveModel, disabled: modelCommand.disabled, choose: (value: string) => { void switchModel(value); }, pick: () => fromPalette('model') },
    pickPermissions: byName('permissions').disabled ? undefined : () => fromPalette('permissions'),
    find: (name: string) => findCommand(commands, name),
  };
}

/** The one question before a model switch that restarts the agent; during a turn, it restarts when the turn ends. */
function ModelRestartPrompt({ provider, agentName, label, busy, queued, returnTo, onRestart, onCancel }: { provider: string; agentName: string; label: string; busy: boolean; queued: boolean; returnTo: RefObject<HTMLTextAreaElement | null>; onRestart(): void; onCancel(): void }) {
  // Enter restarts (now, or after this turn), Esc cancels; the message box has the keyboard back after either.
  const { cardProps, giveBack } = useConfirmCard(onCancel, returnTo);
  return <div className="restart-permission restart-model" role="alert" {...cardProps} aria-keyshortcuts="Enter Escape">
    <span>{modelRestartText(provider, agentName, label, busy, queued)}</span>
    {!queued && <button className="primary" onClick={() => { onRestart(); giveBack(); }}>{busy ? 'Restart after this turn' : 'Restart now'}</button>}
    <button onClick={onCancel}>Cancel</button>
  </div>;
}
