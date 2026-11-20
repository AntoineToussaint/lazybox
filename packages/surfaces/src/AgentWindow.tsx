import { useDisplayDetail } from './display-detail';
import { ArtifactsChip } from './ArtifactCard';
import { DetachedArtifacts } from './DetachedArtifacts';
import { AppearanceSync } from './appearance/AppearanceSync';
import { LeaderMenu } from './LeaderMenu';
import { DetachedCommands } from './DetachedCommands';
import { SnippetPicker } from './SnippetPicker';
import { useKeyboardNavigation, useKeyboardAction, useKeymap, useMenuSnippet, type KeyHint } from './keyboard';
import { continueConversation } from './transcript-cache';
import { useChatScroll } from './useChatScroll';
import { sessionModelLabel, sessionAccountLabel, conversationProvider, conversationTitle, titleFromMessage, branchSummary } from './conversation-model';
import { DeleteConversationDialog } from './DeleteConversationDialog';
import { useFileOpener, type OpenModifiers } from './file-links';
import type { FileLink } from './file-reference';
import { useTranscript } from './useTranscript';
import { useLiveStatus } from './LiveIndicator';
import { PromptHistoryDialog } from './PromptHistoryDialog';
import { savePrompt } from './prompt-history';
import { saveSnippetUse } from './snippet-usage';
import { useCallback, useContext, useEffect, useLayoutEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import { flushSync } from 'react-dom';
import { type CommandStatus, type Session } from '@lazybox/protocol';
import type { SurfaceTarget, ViewStateStore } from '@lazybox/platform';
import { readDraft, writeDraft } from './view-state';
import { RunnerProvider, useRunner, type RunnerConnection } from './RunnerContext';
import { AttentionScope, PanelShownContext, useMarkViewed } from './useAttention';
import { useComposeRequests } from './compose';
import { queuedMessages } from './transcript-model';
export type { RunnerConnection } from './RunnerContext';
import './style.css';
import './chat-header.css';
import { awaitingFirstMessage } from './agent-status';
import { appendToDraft, composeRunMessage, useComposerOffers, useSharedRuns } from './command-runs';
import { fileReferences, useComposerDraft } from './useComposerDraft';
import { useComposerAttachments } from './useComposerAttachments';
import { usePromptRecall } from './usePromptRecall';
import { useSessionRefresh, useSessionWorkspace } from './useSessionPolling';
import { useFileDrop } from './useFileDrop';
import { NewSessionForm, useSessionLauncher, type ConversationCopy } from './NewSessionForm';
import { agentTitle } from './providers';
import { ChatHeader, ChatStatus, SessionHeader, SessionInspection, WindowTopbar, type HeaderStats } from './ChatHeader';
import { showConversation, showReview } from './review-client';
import { ReviewChatButton, useChatReview } from './ReviewFromChat';
import { refreshWorkspaceReview } from './useWorkspaceReview';
import { TelemetryPanel } from './TelemetryPanel';
import { ToolActivity } from './ToolActivity';
import { BrowsingBadge } from './BrowserPanel';
import { useBrowserAutoShow } from './useBrowser';
import { ConversationTitle } from './ConversationTitle';
import { PermissionRestartPrompt, SessionLifecycleButton, SessionMenu, sessionMenuItems, withHandoff } from './SessionMenu';
import { conversationHandoffItems } from './handoff';
import { contextMenu } from './ContextMenu';
import { ResumeBar, TranscriptNotice } from './SessionNotices';
import { useSubagentHost } from './SubagentHost';
import { LiveBar } from './LiveBar';
import { QueuedMessages, useQueuedDelivery } from './QueuedMessages';
import { SubagentContext } from './subagent-view';
import { BackgroundWorkButton, SubagentAttention } from './SubagentTray';
import { SubagentPanel } from './SubagentPanel';
import { ConversationHandoff, type HandoffMessage, type OpenHandoffConversation } from './ConversationHandoff';
import type { PanelTitle } from './PanelRegistry';
import { revealMessage } from './artifact-events';
import { continuationPrompt } from './continuation-prompt';
import { ConfirmDialog } from './ConfirmDialog';
import type { TranscriptMessage } from './transcript-model';
import { MessageArea } from './MessageArea';
import { SubscriptionControls } from './SubscriptionSwitch';
import { Composer } from './Composer';
import { ComposerInfo } from './ComposerInfo';
import { ConsoleWindow, EventLog, SessionCommands } from './SessionCommands';
import { faultSentence, sessionLimit, subscriptionName, turnFault } from './subscription-model';
import { CurrentChatContext, useConversationActivity } from './activity-detail';
import { bindings, displayBinding, useActions } from './actions';
import { CodeRunContext, CodeRuns, type RunActions } from './code-runs';
import { shellBlocksIn } from './code-run';
import { showMenuNotice } from './ContextMenu';
import { useSlashCommands } from './useSlashCommands';
import { showToast } from './toasts';
import { useReadAloudReplies, latestAgentReply } from './voice/useReadAloudReplies';
import { speakableText } from './voice/voice-model';
import { dictating, readAloud, voiceState } from './voice/voice-session';
import { ItemOriginChip } from './WorkItems';
import { humanError } from './human-error';
import { focusComposerOf, wantsComposerFocus } from './confirm-card';

const HANDOVER_NOTICE = 'Added the compact view of a terminal run. Review it, add what you want, and send.';

export type AgentWindowProps = {
  connection: RunnerConnection;
  purpose?: 'coding' | 'help' | 'planning' | 'summary';
  assistantContext?: string;
  /** Initial choices. Starting a session always requires an explicit click. */
  agent?: 'claude' | 'codex';
  copy?: ConversationCopy;
  workingDirectory?: string;
  emptyWorkspace?: boolean;
  initialSessionId?: string;
  collectionId?: string;
  embedded?: boolean;
  showCommands?: boolean;
  allowStart?: boolean;
  /** Set only by an explicit Quick start action; never persisted. */
  autoStart?: boolean;
  onOpenChat?: () => void;
  initialCommandId?: string;
  initialCommandsOpen?: boolean;
  mode?: 'agent' | 'console';
  viewState?: ViewStateStore;
  onViewChange?: (target: SurfaceTarget) => void | Promise<void>;
  onOpenHandoff?: OpenHandoffConversation;
  onChatTitle?(title: PanelTitle): void;
  onSessionChange?: (sessionId: string, collectionId?: string) => void;
  onOpenViewer?: (sessionId: string, commandId: string) => void | Promise<void>;
  /** Shows one of this conversation's managed commands in the host's command console. */
  onOpenCommand?: (commandId: string) => void;
  /** Brings the main window forward: the command palette here navigates there. */
  onFocusMain?: () => void;
  /** Shows this conversation's agent browser beside the chat (BrowserPanel.tsx). */
  onShowBrowser?: () => void;
};

/** One independent view of a Runner session. Unmounting disconnects readers,
 * never execution. Changing the connection resets the view and its readers. */

export function AgentWindow(props: AgentWindowProps) {
  // A detached chat is visually embedded but still owns its window's keyboard
  // dispatcher. Workspace-hosted panels use their Workspace's dispatcher.
  const keyboardHint = useKeyboardNavigation(props.viewState);
  // The conversation shown: artifacts opened here are shown beside it, in this window.
  const [sessionId, setSessionId] = useState(props.initialSessionId || '');
  const onSessionChange = useCallback((id: string) => { setSessionId(id); props.onSessionChange?.(id); }, [props.onSessionChange]);
  return <RunnerProvider connection={props.connection}><AppearanceSync store={props.viewState}/>
    <AttentionScope root={props.viewState?.root}>
      <DetachedArtifacts viewState={props.viewState} sessionId={sessionId} onFocusMain={props.onFocusMain}>
        <ChatPanel {...props} onSessionChange={onSessionChange} keyboardHint={keyboardHint}/>
      </DetachedArtifacts>
      <DetachedCommands viewState={props.viewState} collectionId={props.collectionId || ''} sessionId={props.initialSessionId || ''} onFocusMain={props.onFocusMain}/>
    </AttentionScope>
  </RunnerProvider>;
}

export function ChatPanel({ purpose = 'coding', assistantContext = '', agent, copy, workingDirectory = '', emptyWorkspace = false, initialSessionId = '', collectionId = '', embedded = false, showCommands = true, allowStart = true, autoStart = false, onOpenChat, initialCommandId = '', initialCommandsOpen, mode = 'agent', viewState, onViewChange, onSessionChange, onOpenViewer, onOpenCommand, onShowBrowser, onOpenHandoff, onChatTitle, keyboardHint = null }: Omit<AgentWindowProps, 'connection'> & { keyboardHint?: KeyHint | null }) {
  const { api, info, connecting, error: connectionError, refresh, revision } = useRunner();
  const keymap = useKeymap(viewState);
  const composer = useRef<HTMLTextAreaElement>(null);
  const composerFocused = () => document.activeElement === composer.current || document.querySelector('[data-agent-composer]') === composer.current;

  // Snippets: the picker, and a ]] menu entry. A menu entry sends its snippet
  // to the conversation being typed in, or inserts it when there is already a
  // draft (never overwriting one).
  const [snippetsOpen, setSnippetsOpen] = useState(false);
  useKeyboardAction('snippets.open', () => { if (canCompose && composerFocused()) setSnippetsOpen(true); });
  useMenuSnippet(entry => {
    if (!canCompose || !composerFocused()) return;
    void api.listSnippets({}).then(result => {
      const snippet = result.snippets.find(row => row.key === entry.snippet);
      if (!snippet) { setError(`Snippet "${entry.snippet}" no longer exists. Update the ]] menu in Settings → Keyboard.`); return; }
      saveSnippetUse(viewState, snippet.key);
      if (entry.mode === 'send' && !message.trim() && !files.length && !missingFiles.length && !sending) void send(undefined, snippet.body);
      else { const next = message + (message && !message.endsWith('\n') ? '\n' : '') + snippet.body; setMessage(next); persistDraft(next, files, missingFiles); requestAnimationFrame(() => composer.current?.focus()); }
    }, error => setError(humanError(error)));
  });
  // Enter while a send is in flight (the box is read-only meanwhile) does nothing: the send under way says how it went.
  useKeyboardAction('chat.send', () => { const focused = document.activeElement as HTMLTextAreaElement; if (focused === composer.current && !sendingRef.current && (canSend || missingFiles.length > 0 || !!message.trim())) focused.form?.requestSubmit(); });

  const assistant = purpose !== 'coding';
  const [sessionID, setSessionID] = useState(initialSessionId);
  // A conversation in a tab behind another holds no stream: it catches up from
  // its cursor when brought to the front. A browser allows six connections to
  // the Runner, and a workspace's tabs, each streaming, took them all: every
  // other read of the window (its review, its sessions) then waited forever.
  const shown = useContext(PanelShownContext);
  const transcript = useTranscript(api, sessionID, !!info && shown);
  const [branchSelection, setBranchSelection] = useState<{ message: TranscriptMessage; source: string; requestKey: string }>();
  const branchMessage = branchSelection?.source === sessionID ? branchSelection.message : undefined;
  const [retryDraft, setRetryDraft] = useState('');
  const [branchFromCheckpoint, setBranchFromCheckpoint] = useState(false);
  const [handoffMessage, setHandoffMessage] = useState<HandoffMessage>();
  const [session, setSession] = useState<Session | undefined>(transcript.reader.session);
  useMarkViewed(session, mode === 'agent');
  const workspace = useSessionWorkspace(api, session?.collectionId || collectionId, !!info, revision);
  const { recent: events, messages: conversation, status: stream } = transcript;
  const scroll = useChatScroll(transcript.reader, conversation, session?.pending.length);
  const { jumpToLatest } = scroll;
  const [error, setError] = useState('');
  const { message, setMessage, files, setFiles, missingFiles, setMissingFiles, uncertainSend, setUncertainSend, sendAttempt, persistDraft } = useComposerDraft(viewState, sessionID, setError);
  const [commandsOpen, setCommandsOpen] = useState(initialCommandsOpen ?? !!initialCommandId);
  const [sending, setSending] = useState(false);
  const [commands, setCommands] = useState<CommandStatus[]>([]);
  const [command, setCommand] = useState(initialCommandId);
  // A failed refresh of the session is current state, not an event: the next
  // successful refresh clears it (a Runner restart must not leave a stale alarm).
  const [refreshError, setRefreshError] = useState('');
  const [deleting, setDeleting] = useState(false);
  const [switchingSubscription, setSwitchingSubscription] = useState(false);
  const fileLinks = useFileOpener(session?.collectionId, viewState);
  // Stable, so the memoized messages do not re-render when the opener does.
  const openFileRef = useRef(fileLinks.open); openFileRef.current = fileLinks.open;
  const openFile = useCallback((reference: FileLink, how: OpenModifiers) => void openFileRef.current(reference, how), []);
  const sendingRef = useRef(false);
  const lifetime = useRef<AbortController | null>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const canResumeOnSend = !!session?.closed && !session.resumeBlocked && !session.recoveryRequired && !!session.providerReference && !!session.capabilities?.resume;
  const canCompose = !!session && (!session.closed || canResumeOnSend);
  const canAttach = !!session?.capabilities?.files && !!info?.attachmentLimits && canCompose && !sending;
  const boxes = useComposerAttachments({ draft: { message, setMessage, files, setFiles, missingFiles, setMissingFiles, uncertainSend, setUncertainSend, sendAttempt, persistDraft }, limits: info?.attachmentLimits, canAttach, textarea: composer, sessionID, onError: setError });
  // At a subscription limit a message waits and is sent once the conversation continues.
  const limited = !!sessionLimit(session);
  // After a provider error ends a turn, the next message continues the conversation.
  const continuesAfterError = !!session && !session.closed && turnFault(session.fault);
  const canSend = canCompose && (!session?.fault || canResumeOnSend || limited || continuesAfterError) && !sending && missingFiles.length === 0 && (!!message.trim() || files.length > 0);

  // A run handed over from a terminal joins the draft, after anything already
  // typed, and the cursor goes to its end: the person adds a line and sends.
  const [handedOver, setHandedOver] = useState('');
  // An emptied draft drops the notice. Adjusted during render, not in an effect
  // keyed on the text: that effect queued a second render for every key typed
  // into a blank box, and fast keys piled them up into "Maximum update depth exceeded".
  if (handedOver && !message.trim()) setHandedOver('');
  useEffect(() => setHandedOver(''), [sessionID]);
  const addToDraft = (text: string, notice: string) => {
    const next = appendToDraft(message, text);
    setMessage(next); setUncertainSend(''); sendAttempt.current = null; persistDraft(next, files, missingFiles); setHandedOver(notice);
    requestAnimationFrame(() => { const box = composer.current; if (box) { box.focus(); box.setSelectionRange(next.length, next.length); box.scrollTop = box.scrollHeight; } });
  };
  useComposerOffers(canCompose && !sending && !assistant ? sessionID : '', text => addToDraft(text, HANDOVER_NOTICE));
  useSharedRuns(api, assistant ? '' : sessionID, session?.conversationId || session?.id || '', (claimedFor, command, view) => {
    const text = composeRunMessage(command, view);
    if (claimedFor === sessionID && !sending) { addToDraft(text, `“${command.title}” was shared from your terminal. Review it, add what you want, and send.`); return; }
    // The view moved on after the claim: keep the run in that conversation's draft.
    try { const draft = readDraft(viewState, claimedFor); writeDraft(viewState, claimedFor, { ...draft, text: appendToDraft(draft.text, text) }); }
    catch (e) { setError(`Could not keep the shared run in its draft: ${humanError(e)}`); }
  });
  // Another panel (Changes: failing checks, Quote in reply, Ask to fix) puts
  // text after any draft, for the person to review and send; it is never sent
  // for them. The saved draft is current (every edit persists it) even when
  // this render has not seen it yet, and saving now survives this view
  // remounting. While a send is in flight the text waits: the send empties the
  // box when it is confirmed, and would erase it (and the draft it retries).
  useComposeRequests(sessionID, shown && canCompose && !sending, text => {
    const current = viewState ? readDraft(viewState, sessionID).text : message;
    const next = current + (current.trim() ? '\n\n' : '') + text;
    setMessage(next); persistDraft(next, files, missingFiles); setUncertainSend(''); sendAttempt.current = null;
    requestAnimationFrame(() => { const node = composer.current; if (node) { node.focus(); node.setSelectionRange(node.value.length, node.value.length); } });
  }, api);

  useEffect(() => {
    const control = new AbortController(); lifetime.current = control;
    return () => { control.abort(); lifetime.current = null; };
  }, []);

  const refreshSession = useSessionRefresh({
    api, sessionID, connected: !!info, showCommands, reader: transcript.reader, sending: sendingRef, setSession, setCommands, setRefreshError,
    onSuccessor: next => { setSessionID(next.id); onSessionChange?.(next.id); },
  });
  const launcher = useSessionLauncher({
    api, info, viewState, agent, copy, workingDirectory, purpose, assistantContext, collectionId, emptyWorkspace, allowStart, autoStart, sessionID, lifetime,
    onError: setError,
    onStarted: (id, workspace) => { setSessionID(id); setCommand(''); onSessionChange?.(id, workspace); },
  });

  const send = async (event?: FormEvent, snippetText?: string) => {
    event?.preventDefault();
    // ⌘↵ (chat.send-now) submits this same form, after dictation's last words
    // settle when it was listening: the message it sends goes now.
    const now = !snippetText && Date.now() - sendNowAt.current < 15_000; sendNowAt.current = 0;
    // `/command`: Lazybox's runs here, an unknown one asks first; the agent's goes on as the prompt.
    if (!snippetText && !files.length && !missingFiles.length && !sendingRef.current && !sending && slash.intercept(message)) return false;
    const text = snippetText ?? message;
    // Never swallow Enter: say what is in the way.
    if (!snippetText && !sendingRef.current && !sending && missingFiles.length && (message.trim() || files.length)) { setError(`Attach ${missingFiles.length === 1 ? missingFiles[0].name : 'the files'} again, or remove ${missingFiles.length === 1 ? 'it' : 'them'}, to send.`); return false; }
    // Any other refusal says why: a message that cannot go must never look sent.
    if (!snippetText && (message.trim() || files.length) && !canSend) {
      const why = sendingRef.current || sending ? 'Still sending your previous message…'
        : !session ? 'Still connecting to this conversation…'
        : !canCompose ? (session.recoveryRequired ? 'A previous Lazybox left this conversation’s processes unconfirmed: settle that above first.' : session.resumeBlocked ? 'This conversation cannot be continued from here.' : 'This conversation has ended.')
        : session.fault ? `${faultSentence(session.fault)} Resume it or start a new conversation to continue.`
        : '';
      if (why) setError(why);
      return false;
    }
    if (sendingRef.current || !(snippetText ? canCompose && (!session?.fault || limited || continuesAfterError) && !sending && !message.trim() && !files.length && !missingFiles.length : canSend)) return false;
    sendingRef.current = true; setSending(true); setError(''); jumpToLatest();
    const signal = lifetime.current!.signal;
    try {
      const previous = sendAttempt.current;
      const attempt = previous?.text === text && previous.files.length === files.length && files.every((file, i) => file === previous.files[i]) ? previous : { id: uncertainSend || crypto.randomUUID(), text, files };
      sendAttempt.current = attempt;
      setUncertainSend(attempt.id); persistDraft(text, files, [], attempt.id);
      const attachments = await Promise.all(files.map(async file => ({ name: file.name, mediaType: file.type, data: new Uint8Array(await file.arrayBuffer()) })));
      let destination = sessionID;
      if (session?.closed) {
        const resumed = await api.resumeSession({ sessionId: sessionID }, { signal });
        if (!resumed.session) throw new Error('Runner did not return the resumed conversation');
        continueConversation(api, sessionID, resumed.session);
        destination = resumed.session.id;
        // A view may close while delivery is in flight. Retain the same draft
        // and operation identity under the successor before sending any bytes.
        writeDraft(viewState, destination, { text, files: fileReferences(files), operationId: attempt.id });
      }
      // A delivery that never answers (a Runner restarting mid-request) must not
      // block sending forever. The operation id is kept, so sending again cannot
      // deliver the message twice.
      const deadline = AbortSignal.timeout(30_000);
      try { await api.apply({ sessionId: destination, operationId: attempt.id, operation: { case: 'message', value: { text, attachments } } }, { signal: AbortSignal.any([signal, deadline]) }); }
      catch (e) { if (deadline.aborted && !signal.aborted) throw new Error('The Runner did not confirm this message within 30 seconds. Press Enter to send it again: it will not be delivered twice.'); throw e; }
      savePrompt(viewState, text, session?.conversationId || sessionID); recall.reset();
      // Queued behind a busy turn, it shows above the box now, not at the next poll.
      refreshSession();
      // Behind a busy turn the message was queued: it goes next, the turn stopped for it.
      if (now && destination === sessionID && session?.busy && !session.closed) void queuedDelivery.deliver(attempt.id, 'now', true);
      if (!signal.aborted) { setMessage(''); setFiles([]); boxes.clearRejected(); setUncertainSend(''); sendAttempt.current = null; persistDraft('', [], []); if (destination !== sessionID) { writeDraft(viewState, destination, { text: '', files: [], operationId: '' }); setSessionID(destination); onSessionChange?.(destination); refresh(); } }
      return !signal.aborted;
    } catch (e) { if (!signal.aborted) { setError(humanError(e)); if (snippetText) setMessage(text); } return false; }
    finally { if (!signal.aborted) { sendingRef.current = false; setSending(false); } }
  };
  // Files chosen, dropped or pasted: those over a limit become error boxes (useComposerAttachments).
  const selectFiles = boxes.select;
  const interrupt = async () => {
    try { await api.apply({ sessionId: sessionID, operationId: crypto.randomUUID(), operation: { case: 'interrupt', value: {} } }); }
    catch (e) { setError(humanError(e)); }
  };
  const [restartTo, setRestartTo] = useState('');
  // An agent whose worker predates live permission changes restarts in the new
  // mode between turns; the same provider conversation continues.
  const restartWithPermission = async (mode: string) => {
    setRestartTo('');
    try {
      const result = await api.restartWithPermission({ sessionId: sessionID, mode });
      if (!result.session) throw new Error('Runner did not return the restarted conversation');
      adopt(result.session);
    } catch (e) { setError(humanError(e)); }
  };
  /** The conversation continues in a new agent (a restart in another mode or model). */
  // The message box has the keyboard again once the new agent can take a message (confirm-card.ts).
  const adopt = (next: Session) => { focusComposerOf(next.id); continueConversation(api, sessionID, next); setSessionID(next.id); onSessionChange?.(next.id); refresh(); };
  // "Give Claude the browser" (BROWSER.md): an agent started without the browser
  // restarts with it and the conversation continues; one off for its workspace gets it back.
  const giveBrowser = async () => {
    try {
      const result = await api.giveBrowser({ sessionId: sessionID });
      if (!result.session) throw new Error('Runner did not return the conversation');
      if (result.session.id !== sessionID) adopt(result.session); else setSession(result.session);
      showMenuNotice(`${agentName} has the browser`);
    } catch (e) { setError(humanError(e)); }
  };
  const setPermission = (mode: string) => { if (session?.capabilities?.livePermission) void changePermission(mode); else setRestartTo(mode); };
  const changePermission = async (mode: string) => {
    try {
      await api.apply({ sessionId: sessionID, operationId: crypto.randomUUID(), operation: { case: 'permission', value: { mode } } });
      setSession(current => current ? { ...current, permission: mode } : current);
    } catch (e) { setError(humanError(e)); }
  };
  const close = async () => {
    try {
      const result = await api.stopSessionResources({ sessionId: sessionID }); setSession(result.session);
      // Ending a reviewer stops its review: its card says so at once.
      if (result.session?.collectionId) refreshWorkspaceReview(result.session.collectionId);
    } catch (e) { setError(humanError(e)); }
  };
  const live = useLiveStatus(session, events);
  // Send now from the keyboard (⌘↵): the typed message, or with an empty box
  // the oldest queued one, interrupting the turn in progress. Idle, it is a send.
  // What waits behind the turn: the Runner's queue and the transcript's, merged (transcript-model.ts).
  const queued = useMemo(() => queuedMessages(session?.queuedMessages, transcript), [session?.queuedMessages, transcript.pending, transcript.messages, transcript.canceled]);
  const queuedDelivery = useQueuedDelivery(api, sessionID, queued);
  const sendNowAt = useRef(0);
  const sendNowBinding = bindings(keymap['chat.send-now'])[0];
  const sendNowKey = sendNowBinding ? displayBinding(sendNowBinding).join(' ') : '';
  const draftToSend = () => !!message.trim() || files.length > 0 || missingFiles.length > 0;
  // Words dictated into this box count before `message` shows them: a key pressed right after them can
  // run before that render (a busy machine). The form's submit waits for the last words (Composer).
  const saidHere = () => dictating() && voiceState().kind === 'composer' && !!voiceState().text.trim() && document.activeElement === composer.current;
  const workingTurn = () => !!session?.busy && !session.closed;
  const sendNow = () => {
    if (draftToSend() || saidHere()) {
      if (workingTurn()) sendNowAt.current = Date.now();
      composer.current?.form?.requestSubmit();
      return;
    }
    const oldest = workingTurn() ? queued[0] : undefined;
    if (oldest) { jumpToLatest(); void queuedDelivery.deliver(oldest.operationId, 'now'); }
    else showMenuNotice(workingTurn() ? 'Nothing queued to send now' : 'Nothing to send');
  };
  useActions({
    'chat.send-now': mode === 'agent' && !!sessionID && {
      run: sendNow,
      // From the message box always (an empty one says so); from the palette, in the chat in front, when there is something to send now.
      enabled: () => document.activeElement === composer.current || (activity.current() && canCompose && workingTurn() && (draftToSend() || queued.length > 0)),
    },
  });
  const disconnected = stream.startsWith('Disconnected') || stream === 'Reconnecting';
  const activeProfile = info?.profiles.find(p => p.id === session?.profileId);
  const agentName = agentTitle(activeProfile?.provider || session?.provider || launcher.provider, info?.profiles);
  const currentCommand = command || commands.at(-1)?.id;
  // How much tool activity this conversation shows: the default, or its own.
  const display = useDisplayDetail(viewState);
  const activity = useConversationActivity(viewState, session?.conversationId || sessionID, mode === 'agent' && !!sessionID);
  const [historyOpen, setHistoryOpen] = useState(false);
  const recall = usePromptRecall({
    viewState, sessionID, conversationKey: session?.conversationId || sessionID, conversation,
    onRecall: text => {
      setMessage(text); setUncertainSend(''); sendAttempt.current = null; persistDraft(text, files, missingFiles);
    },
  });
  useEffect(() => {
    if (!onViewChange) return;
    void (async () => { try { await onViewChange({ sessionId: sessionID, commandId: currentCommand || '', commandsOpen }); } catch (e) { setError(`Could not save this window: ${humanError(e)}`); } })();
  }, [onViewChange, sessionID, currentCommand, commandsOpen]);
  const newSession = () => {
    setSessionID(''); setSession(undefined); setCommands([]); setCommand(''); setMessage(''); setFiles([]); setMissingFiles([]); setUncertainSend(''); sendAttempt.current = null; setError(''); onSessionChange?.('');
  };
  const realTitle = conversationTitle(session);
  const chatProvider = session?.id === sessionID ? conversationProvider(session, info?.profiles || []) : '';
  const chatTitleRef = useRef(onChatTitle);
  chatTitleRef.current = onChatTitle;
  useEffect(() => {
    if (sessionID) chatTitleRef.current?.({ text: realTitle, agent: { sessionId: sessionID, provider: chatProvider } });
  }, [sessionID, realTitle, chatProvider]);
  // While only the latest turns are in (the earlier history still loading), the first of them is not the first message.
  const firstPrompt = transcript.partial ? '' : conversation.find(m => m.type === 'operation_accepted' && !m.origin)?.text || '';
  // An assistant conversation is about Lazybox, not the workspace's branches: its header names none.
  const branch = assistant ? undefined : branchSummary(workspace);
  const modelLabel = [sessionModelLabel(session, transcript.reportedModel), session?.effort].filter(Boolean).join(' · ');
  const accountName = sessionAccountLabel(session, activeProfile);
  const profileList = info?.profiles;
  const nameFor = useCallback((id: string) => subscriptionName(profileList?.find(p => p.id === id), id), [profileList]);
  const shownAccount = accountName === 'Account not reported' || accountName === 'Loading account…' ? '' : accountName;
  // The header names the subscription only when the provider has another one to tell it from.
  const provider0 = (activeProfile?.provider || launcher.provider) === 'codex' ? 'codex' : 'claude';
  const headerAccount = (profileList || []).filter(p => p.provider === provider0).length > 1 ? shownAccount : '';
  // A Runner restart while nothing was said yet is not news: an empty
  // conversation stays empty instead of showing "Runner restarted".
  const spoken = conversation.some(m => m.type !== 'reattached');
  // A conversation that opened and waits for its first message takes the keyboard once.
  const focusedFor = useRef('');
  const firstMessage = !!session && awaitingFirstMessage(session) && !spoken;
  useEffect(() => {
    if (!firstMessage || !canCompose || focusedFor.current === sessionID) return;
    focusedFor.current = sessionID;
    const active = document.activeElement;
    if (!active || active === document.body || composer.current?.closest('main')?.contains(active)) composer.current?.focus({ preventScroll: true });
  }, [firstMessage, canCompose, sessionID]);
  // A restart answered from its card (keyboard or not): the conversation continues here, and so does typing.
  useEffect(() => {
    if (!canCompose || !wantsComposerFocus(sessionID)) return;
    const active = document.activeElement;
    if (!active || active === document.body || composer.current?.closest('main')?.contains(active)) composer.current?.focus({ preventScroll: true });
  }, [canCompose, sessionID]);
  const resumeSession = async (processesStopped: boolean) => {
    const result = await api.resumeSession({ sessionId: sessionID, processesStopped }, { signal: lifetime.current!.signal });
    if (!result.session) return false;
    continueConversation(api, sessionID, result.session); setSessionID(result.session.id); onSessionChange?.(result.session.id); refresh();
    return true;
  };
  const drop = useFileDrop({ canAttach, onFiles: selectFiles, onError: setError });
  const insertSnippet = (body: string) => {
    const node = composer.current, start = node?.selectionStart ?? message.length, end = node?.selectionEnd ?? message.length;
    const next = message.slice(0, start) + body + message.slice(end);
    // The text and the caret after it are placed now, not on a later frame: a
    // frame that came late used to move the caret over whatever the person did
    // next (select all, then Delete, deleted nothing).
    flushSync(() => { setMessage(next); setUncertainSend(''); });
    persistDraft(next, files, missingFiles); sendAttempt.current = null;
    node?.setSelectionRange(start + body.length, start + body.length);
    // Only the focus waits, for the picker to go (it makes the composer inert);
    // a focus already back in the box keeps its own selection.
    requestAnimationFrame(() => { if (node && document.activeElement !== node) node.focus(); });
  };

  // Run on a shell block of the agent's: asking the agent, or running it yourself (code-runs.tsx).
  const codeRuns = useMemo(() => sessionID && !assistant && mode === 'agent' ? new CodeRuns(api, sessionID) : null, [api, sessionID, assistant, mode]);
  useEffect(() => codeRuns?.attach(), [codeRuns]);
  const openCommand = onOpenCommand || (showCommands ? (commandId: string) => { setCommand(commandId); setCommandsOpen(true); } : onOpenViewer ? (commandId: string) => void onOpenViewer(sessionID, commandId) : undefined);
  // A Run request goes as its own message, beside any draft: at once, or queued behind the turn in progress.
  const sendRunRequest = async (text: string, reportId?: string): Promise<'sent' | 'drafted'> => {
    const ready = !!session && !session.closed && (!session.fault || limited || continuesAfterError);
    if (!ready) {
      if (reportId || !canCompose) throw new Error(session ? 'This conversation cannot take a message.' : 'Still connecting to this conversation…');
      addToDraft(text, 'Added to your message: sending it continues the conversation and asks to run it.');
      return 'drafted';
    }
    await api.apply({ sessionId: sessionID, operationId: reportId || crypto.randomUUID(), operation: { case: 'message', value: { text, attachments: [] } } }, { signal: reportId ? AbortSignal.timeout(30_000) : AbortSignal.any([lifetime.current!.signal, AbortSignal.timeout(30_000)]) });
    if (!reportId) jumpToLatest();
    refreshSession();
    return 'sent';
  };
  const runActions: RunActions = {
    send: sendRunRequest,
    draft: (text, notice) => { if (canCompose && !sending) addToDraft(text, notice); else showMenuNotice(sending ? 'Still sending your previous message…' : 'This conversation cannot take a message.'); },
    openOutput: openCommand,
  };
  useLayoutEffect(() => { codeRuns?.update({ session, workspace, messages: conversation, agentName, actions: runActions }); });
  // The agent's latest shell block, found once per change of the conversation, not on every render (every key typed).
  const lastShellBlock = useMemo(() => {
    for (let i = conversation.length - 1; i >= 0; i--) {
      const m = conversation[i];
      if (m.type !== 'message_delta') continue;
      const block = shellBlocksIn(m.text).at(-1);
      if (block) return { messageId: m.id, ...block };
    }
    return undefined;
  }, [conversation]);
  const lastBlock = useRef(lastShellBlock); lastBlock.current = lastShellBlock;
  const runLast = (myself: boolean) => {
    const block = lastBlock.current;
    if (!codeRuns || !block) return;
    const why = myself && codeRuns.whyNotRunMyself();
    if (why) { showMenuNotice(why); return; }
    scroll.revealMessage(block.messageId);
    // Run opens the Terminal panel with the command at the prompt, as the block's own Run does; never a message to the agent.
    if (myself) codeRuns.openConfirm(block.messageId, block.script); else void codeRuns.runLastTerminal(block.messageId, block.script);
  };
  const canRunLast = () => !!codeRuns && activity.current() && !!lastBlock.current;
  useActions({
    'voice.read-latest': mode === 'agent' && {
      run: () => void readAloud(speakableText(latestAgentReply(transcript.messages), 'full')),
      enabled: () => activity.current() && !dictating() && !!latestAgentReply(transcript.messages),
    },
    'code.run-last': !!codeRuns && { run: () => runLast(false), enabled: canRunLast },
    'code.run-last-myself': !!codeRuns && { run: () => runLast(true), enabled: canRunLast },
  });

  // Review from the chat: the header's Review button, its ⋯ item, the right-click menu and `/review`.
  const chatReview = useChatReview({ session: assistant ? undefined : session, sessionID, onError: setError });
  const openFreshConversation = async (source: Session, profileId: string, firstMessage = '') => {
    const sameAccount = source.profileId === profileId;
    const opened = await api.openSession({ profileId, collectionId: source.collectionId, cwd: source.collectionId ? '' : source.workingDirectory,
      options: { model: sameAccount ? source.model : '', effort: sameAccount ? source.effort : '', permission: source.permission, workflow: source.workflow, gatewayId: sameAccount && source.gatewayId !== 'direct' ? source.gatewayId : '', rotationProfileIds: [], instructions: '', noRotation: false } });
    if (!opened.session) throw new Error('Runner did not return the new conversation');
    let transferError = '';
    if (firstMessage) {
      const operationId = crypto.randomUUID();
      try {
        await api.apply({ sessionId: opened.session.id, operationId,
          operation: { case: 'message', value: { text: firstMessage, attachments: [] } } }, { timeoutMs: 30_000 });
      } catch (error) {
        writeDraft(viewState, opened.session.id, { text: firstMessage, files: [], operationId });
        transferError = `Codex could not confirm the handoff. The context is saved in the draft. ${humanError(error)}`;
      }
    }
    setSession(undefined); setCommands([]); setCommand(''); setFiles([]); setMissingFiles([]); setUncertainSend(''); sendAttempt.current = null; setError('');
    setSessionID(opened.session.id); onSessionChange?.(opened.session.id); refresh();
    if (transferError) showMenuNotice(transferError);
  };
  const openBranch = async () => {
    if (!branchMessage || !branchSelection) return;
    const retry = branchMessage.type === 'operation_accepted';
    const boundary = retry
      ? { case: 'beforePromptCursor' as const, value: BigInt(branchMessage.id) }
      : { case: 'afterTurnId' as const, value: branchMessage.turnId };
    const opened = await api.openSession({ fork: { sourceSessionId: branchSelection.source, boundary, checkpointFiles: branchFromCheckpoint, requestKey: branchSelection.requestKey } }, { timeoutMs: 90_000 });
    if (!opened.session) throw new Error('Runner did not return the branched conversation');
    const child = opened.session;
    if (retry) {
      const stopped = await api.stopSessionResources({ sessionId: branchSelection.source }, { timeoutMs: 90_000 });
      if (!stopped.session?.closed || stopped.session.recoveryRequired) {
        throw new Error('Lazybox could not confirm that the previous continuation stopped.');
      }
      await api.apply({ sessionId: child.id, operationId: branchSelection.requestKey,
        operation: { case: 'message', value: { text: retryDraft, attachments: [] } } });
      savePrompt(viewState, retryDraft, child.conversationId || child.id);
    }
    setBranchSelection(undefined);
    if (!retry || branchFromCheckpoint) {
      if (!onOpenHandoff?.({ collectionId: child.collectionId, sessionId: child.id, commandId: '' })) {
        showMenuNotice('The new conversation is available in History.');
      }
      refresh();
      return;
    }
    setSession(undefined); setCommands([]); setCommand(''); setFiles([]); setMissingFiles([]); setUncertainSend(''); sendAttempt.current = null; setError('');
    setSessionID(opened.session.id); onSessionChange?.(opened.session.id); refresh();
  };
  const openBranchSource = async (id: string, cursor: bigint) => {
    try {
      const { session: source } = await api.getSession({ sessionId: id });
      if (!source) throw new Error('The source conversation is no longer available.');
      if (!onOpenHandoff?.({ collectionId: source.collectionId, sessionId: source.id, commandId: '' })) {
        setSessionID(source.id); onSessionChange?.(source.id);
      }
      if (cursor) revealMessage({ sessionId: source.id, messageId: cursor.toString() });
    } catch (error) {
      setError(humanError(error));
    }
  };
  const continueInCodex = session && allowStart && !assistant ? async (profileId: string) => {
    const source = transcript.reader.getSnapshot();
    if (source.loading || source.partial || source.error) throw new Error('The source conversation is still loading or could not be read. Try again once its history is available.');
    const pending = queuedMessages(session.queuedMessages, source);
    await openFreshConversation(session, profileId, continuationPrompt(session.title, source.messages, pending, message));
  } : undefined;
  // Slash commands: the menu over the message box, and Lazybox's own commands (useSlashCommands.tsx).
  const provider = activeProfile?.provider || session?.provider || launcher.provider;
  const slash = useSlashCommands({
    api, viewState, enabled: mode === 'agent' && !!sessionID, session, sessionID, agentName, provider, catalog: transcript.catalog,
    model: transcript.reportedModel || session?.model || '', conversation, shellBlock: !!codeRuns && !!lastShellBlock, detail: activity.detail,
    subscriptions: (profileList || []).filter(p => p.provider === provider).length, canCompose, message,
    setText: text => { setMessage(text); setUncertainSend(''); sendAttempt.current = null; persistDraft(text, files, missingFiles); },
    textarea: composer, front: activity.current, onError: setError, reviewLauncher: chatReview.launcher,
    actions: {
      sendText: async text => {
        const ready = !!session && !session.closed && (!session.fault || limited || continuesAfterError);
        if (!ready) { if (canCompose) addToDraft(text, 'Sending this continues the conversation and runs the command.'); else setError('This conversation cannot take a message now.'); return false; }
        try {
          await api.apply({ sessionId: sessionID, operationId: crypto.randomUUID(), operation: { case: 'message', value: { text, attachments: [] } } }, { signal: AbortSignal.any([lifetime.current!.signal, AbortSignal.timeout(30_000)]) });
          savePrompt(viewState, text, session?.conversationId || sessionID); jumpToLatest(); refreshSession(); return true;
        } catch (e) { setError(humanError(e)); return false; }
      },
      interrupt: () => void interrupt(), setPermission, switchSubscription: () => setSwitchingSubscription(true), adopt, giveBrowser: () => void giveBrowser(),
      clear: async () => {
        if (!session) return;
        await openFreshConversation(session, session.profileId);
        showToast({ tone: 'info', title: 'New conversation', body: 'The previous one is kept in this workspace.' });
      },
      rename: async title => { const result = await api.renameConversation({ sessionId: sessionID, title }); setSession(result.session); refresh(); },
    },
  });

  // Settings → Voice → Read aloud: a reply that finishes while this conversation is shown.
  useReadAloudReplies(mode === 'agent' ? sessionID : '', transcript);
  const subagents = useSubagentHost({ api, sessionID, session, index: transcript.subagents, revealMessage: scroll.revealMessage, openFile: session?.collectionId ? openFile : undefined, onError: setError });
  // The agent starts browsing: its browser shows beside the chat (Settings → Browser).
  useBrowserAutoShow(api, session?.conversationId || session?.id || '', session?.closed ? undefined : session?.browser, onShowBrowser);
  if (mode === 'console') return <ConsoleWindow api={api} command={currentCommand} error={error} viewState={viewState} onError={setError} onOpenFile={session?.collectionId ? openFile : undefined} files={fileLinks.element}/>;

  const title = <ConversationTitle api={api} session={session} sessionID={sessionID} realTitle={realTitle} suggestedTitle={titleFromMessage(firstPrompt)} firstMessage={firstPrompt} firstMessageLoading={!!transcript.partial} branch={branch}
    onRenamed={renamed => { setSession(renamed); refresh(); }} onError={setError}/>;
  const status = <><ChatStatus session={session} live={live}/><BrowsingBadge session={session} agentName={agentName} onShow={onShowBrowser}/><SubagentAttention/></>;
  const inspection = <><BackgroundWorkButton/><SessionInspection sessionID={sessionID} session={session} activity={transcript.activity} activityTruncated={transcript.activityTruncated}/></>;
  const stats: HeaderStats = { background: <BackgroundWorkButton/>, usage: <TelemetryPanel executionId={sessionID} conversationId={session?.conversationId} compact/>, tools: <ToolActivity sessionId={sessionID} events={transcript.activity} truncated={transcript.activityTruncated}/>,
    artifacts: session?.collectionId ? <ArtifactsChip collectionId={session.collectionId} sessionId={sessionID} conversationId={session.conversationId} title={realTitle}/> : undefined };
  const lifecycle = <SessionLifecycleButton session={session} allowStart={allowStart} onNewSession={newSession} onClose={() => void close()}/>;

  return <main className={'agent-window' + (embedded ? ' embedded-chat' : '') + (drop.dragging ? ' file-drag-active' : '')} {...drop.handlers} onFocusCapture={activity.mark} onPointerDownCapture={activity.mark}>
    {!embedded && <LeaderMenu hint={keyboardHint}/>}
    {snippetsOpen && <SnippetPicker store={viewState} canSend={canCompose && !sending && !message.trim() && !files.length && !missingFiles.length}
      onClose={() => { setSnippetsOpen(false); composer.current?.focus(); }}
      onInsert={insertSnippet}
      onSend={text => send(undefined, text)}/>}
    {!embedded && <WindowTopbar connecting={connecting} info={info} sessionID={sessionID} session={session}/>}
    {(error || connectionError || refreshError) && !(embedded && connectionError) && <div className="error" role="alert">{error || connectionError || refreshError}<button onClick={() => { setError(''); setRefreshError(''); }}>Dismiss</button></div>}
    {!info && !connecting && <div className="connection-retry"><button onClick={refresh}>Retry connection</button></div>}
    {!sessionID && !allowStart
      ? <div className="empty">Select an existing session to view its chat.</div>
      : !sessionID
        ? <NewSessionForm launcher={launcher} info={info} purpose={purpose} emptyWorkspace={emptyWorkspace} collectionId={collectionId} agentName={agentName}/>
        : <SubagentContext.Provider value={subagents}><CurrentChatContext.Provider value={activity.current}><section className="single-session">
          {embedded
            ? <ChatHeader agentName={agentName} session={session} sessionID={sessionID} title={title} status={status} statusText={live.text + (session?.queued ? ` · ${session.queued} queued` : '')} statusBusy={live.tone === 'busy' || live.tone === 'waiting'}
              modelLabel={modelLabel} stats={stats} realTitle={realTitle} branch={branch} shownAccount={headerAccount} onOpenChat={onOpenChat} origin={assistant ? undefined : <ItemOriginChip workspace={workspace}/>}
              modelRequested={!!session?.model && !session.modelReported && !transcript.reportedModel}
              review={!assistant && <ReviewChatButton review={chatReview} keys={keymap['review.start']}/>}
              onContextMenu={contextMenu('session', () => [...chatReview.menuItems, onOpenChat && { label: 'Open in its own window', icon: 'external', run: onOpenChat }, 'separator', ...withHandoff(sessionMenuItems({ session, allowStart,
                onChangePermission: mode => void changePermission(mode), onRestartWithPermission: setRestartTo, onSwitchSubscription: () => setSwitchingSubscription(true), onGiveBrowser: () => void giveBrowser(),
                onNewSession: newSession, onClose: () => void close(), onDelete: () => setDeleting(true), activity, models: slash.models, sessionID }), conversationHandoffItems(api, session, info))], { sessionId: sessionID })}
              onPickModel={session && !session.closed ? slash.models.pick : undefined}
              onSwitchSubscription={session && !session.closed ? () => setSwitchingSubscription(true) : undefined}
              menu={<SessionMenu session={session} sessionID={sessionID} accountName={shownAccount} stream={stream} allowStart={allowStart}
                onChangePermission={mode => void changePermission(mode)} onRestartWithPermission={setRestartTo}
                onSwitchSubscription={() => setSwitchingSubscription(true)} onGiveBrowser={() => void giveBrowser()}
                onNewSession={newSession} onClose={() => void close()} onDelete={() => setDeleting(true)} activity={activity} models={slash.models} extra={chatReview.menuItems}/>}/>
            : <SessionHeader agentName={agentName} session={session} sessionID={sessionID} title={title} status={status} modelLabel={modelLabel} inspection={inspection}
              accountName={shownAccount} lifecycle={lifecycle}/>}
          {drop.dragging && <div className="file-drop-hint" role="status">Drop files to attach</div>}
          {session?.closed && <ResumeBar session={session} canResumeOnSend={canResumeOnSend} onResume={resumeSession} onError={setError}/>}
          {deleting && session && <DeleteConversationDialog session={session} onClose={() => setDeleting(false)} onDeleted={() => { setDeleting(false); newSession(); }}/>}
          {fileLinks.element}
          {slash.dialogs}
          {chatReview.dialog}
          {chatReview.asReviewer && <div className="reviewer-notice" role="note" aria-label="Read-only review">
            <span><strong>Read-only review</strong> · Use the author conversation to apply findings and merge.</span>
            {embedded && <><button type="button" onClick={() => showConversation({ collectionId: chatReview.workspaceId, sessionId: chatReview.asReviewer!.sourceSessionId })}>Open author conversation</button>
              <button type="button" onClick={() => showReview({ collectionId: chatReview.workspaceId, reviewId: chatReview.asReviewer!.id })}>Review tray</button></>}
          </div>}
          {transcript.error && <TranscriptNotice error={transcript.error} onRetry={transcript.retry}/>}
          {handoffMessage && session?.collectionId && onOpenHandoff && <ConversationHandoff source={session} message={handoffMessage}
            open={onOpenHandoff} onClose={() => setHandoffMessage(undefined)}/>}
          {branchMessage && <ConfirmDialog title={branchMessage.type === 'operation_accepted' ? 'Edit and restart here' : 'Fork after this turn'}
            confirm={branchMessage.type === 'operation_accepted' ? 'Restart with revised prompt' : 'Fork conversation'} working="Opening conversation…" tone="primary"
            closeLabel="Close conversation branch" canConfirm={branchMessage.type !== 'operation_accepted' || !!retryDraft.trim()}
            onClose={() => setBranchSelection(undefined)} onConfirm={openBranch}
            details={busy => <>
              {session?.collectionId && onOpenHandoff && <label>Workspace files<select disabled={busy} value={branchFromCheckpoint ? 'checkpoint' : 'current'} onChange={event => setBranchFromCheckpoint(event.target.value === 'checkpoint')}>
                <option value="checkpoint">New workspace from this turn</option>
                <option value="current">Use current workspace files</option>
              </select></label>}
              {branchMessage.type === 'operation_accepted' && <label>Revised prompt<textarea disabled={busy} value={retryDraft} onChange={event => setRetryDraft(event.target.value)}/></label>}
            </>}>
            {branchMessage.type === 'operation_accepted'
              ? 'Restarting stops the previous continuation and sends your revised prompt. The previous continuation remains available in History.'
              : 'The source conversation can keep working while the fork runs.'}
            {' '}
            {branchFromCheckpoint
              ? 'The new worktrees start at the saved repository checkpoint. Ignored files are excluded.'
              : 'This uses the current workspace files.'}
          </ConfirmDialog>}
          {session?.forkedFromSessionId && <button type="button" className="text-button" onClick={() => void openBranchSource(session.forkedFromSessionId, session.forkedFromCursor)}>
            Open previous conversation
          </button>}
          <CodeRunContext.Provider value={codeRuns}><MessageArea onBranch={allowStart && !assistant && session?.provider === 'codex'
            ? entry => { setBranchSelection({ message: entry, source: sessionID, requestKey: crypto.randomUUID() }); setRetryDraft(entry.text); setBranchFromCheckpoint(entry.type !== 'operation_accepted' && !!session.collectionId && !!onOpenHandoff); } : undefined} onOpenHandoff={onOpenHandoff} onHandoff={session?.collectionId && onOpenHandoff && !assistant ? setHandoffMessage : undefined} api={api} session={session} sessionID={sessionID} transcript={transcript} scroll={scroll} spoken={spoken} agentName={agentName} limited={limited} nameFor={nameFor}
            onOpenFile={session?.collectionId ? openFile : undefined} onError={setError} detail={activity.detail} alwaysShowTime={display.timestamps}
            subscription={session && <SubscriptionControls session={session} profiles={profileList || []} viewState={viewState} dialogOpen={switchingSubscription} onCloseDialog={() => { setSwitchingSubscription(false); requestAnimationFrame(() => { const active = document.activeElement; if (!active || active === document.body) composer.current?.focus({ preventScroll: true }); }); }} onSession={next => { transcript.reader.session = next; setSession(next); }} onError={setError} onContinueInCodex={continueInCodex}/>}/></CodeRunContext.Provider>
          {!transcript.error && <LiveBar api={api} session={session} sessionID={sessionID} live={live} disconnected={disconnected} activity={transcript.activity} viewState={viewState} onInterrupt={() => void interrupt()}
            onOpenCommand={openCommand} onOpenFile={session?.collectionId ? openFile : undefined}/>}
          {!transcript.error && !disconnected && <QueuedMessages session={session} queued={queued} turnRunning={transcript.turnRunning} live={live} agentName={agentName} activity={transcript.activity} delivery={queuedDelivery} sendNowKey={sendNowKey}/>}
          {historyOpen && sessionID && <PromptHistoryDialog key={sessionID} conversationId={session?.conversationId || sessionID} messages={conversation}
            onClose={() => setHistoryOpen(false)} onPick={text => {
              recall.reset(); setMessage(text); setUncertainSend(''); sendAttempt.current = null; persistDraft(text, files, missingFiles);
              setHistoryOpen(false); requestAnimationFrame(() => composer.current?.focus());
            }}/>}
          <Composer textarea={composer} fileInput={fileInput} agentName={agentName} embedded={embedded} session={session} sessionID={sessionID} viewState={viewState} keymap={keymap}
            limited={limited} message={message} files={files} attachments={boxes} missingFiles={missingFiles} uncertainSend={uncertainSend} handedOver={handedOver}
            sending={sending} canCompose={canCompose} canAttach={canAttach} canSend={canSend}
            permissionPrompt={<>{restartTo && <PermissionRestartPrompt mode={restartTo} busy={!!session?.busy} returnTo={composer} onRestart={() => void restartWithPermission(restartTo)} onCancel={() => setRestartTo('')}/>}{slash.prompt}</>}
            slash={slash}
            info={session ? <ComposerInfo agentName={agentName} modelLabel={modelLabel} modelRequested={!!session.model && !session.modelReported && !transcript.reportedModel} sessionID={sessionID} onPickModel={session.closed ? undefined : slash.models.pick}/> : undefined}
            onSubmit={send}
            onChange={text => { if (sendingRef.current) return; recall.reset(); setMessage(text); setUncertainSend(''); sendAttempt.current = null; persistDraft(text, files, missingFiles); }}
            onDictate={setMessage}
            onRecallKey={recall.onKeyDown}
            onOpenPromptHistory={canCompose && sessionID ? () => setHistoryOpen(true) : undefined}
            onRemoveMissingFile={i => { const next = missingFiles.filter((_, index) => index !== i); setMissingFiles(next); setUncertainSend(''); setError(''); persistDraft(message, files, next); }}
            onDropMissingFiles={() => { setMissingFiles([]); setUncertainSend(''); setError(''); persistDraft(message, files, []); requestAnimationFrame(() => composer.current?.focus()); }}
            onDismissHandover={() => setHandedOver('')}
            onOpenSnippets={() => setSnippetsOpen(true)}
            onSelectFiles={selectFiles}
            onInterrupt={() => void interrupt()}
            interruptInBar={!transcript.error && !disconnected && !!session?.busy && live.tone === 'busy'}
            onError={setError}/>
          {showCommands && !assistant && <SessionCommands api={api} session={session} sessionID={sessionID} commands={commands} currentCommand={currentCommand} open={commandsOpen} viewState={viewState}
            onToggle={setCommandsOpen} onSelect={setCommand} onError={setError} onOpenViewer={onOpenViewer}
            onSendToAgent={canCompose && !sending ? text => { addToDraft(text, HANDOVER_NOTICE); return ''; } : undefined}/>}
          {!embedded && <EventLog events={events}/>}
          <SubagentPanel/>
        </section></CurrentChatContext.Provider></SubagentContext.Provider>}
  </main>;
}
