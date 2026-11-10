import { ToolActivity } from "./ToolActivity";
import {SnippetPicker} from './SnippetPicker';
import {useKeyboardNavigation,useKeyboardAction,useKeymap} from './keyboard';
import {continueConversation} from './transcript-cache';
import {useChatScroll} from './useChatScroll';
import {sessionModelLabel,sessionAccountLabel} from './conversation-model';
import { DeleteConversationDialog } from './DeleteConversationDialog';
import { FilePreview } from './FilePreview';
import type { FileLink } from './file-reference';
import { useTranscript } from './useTranscript';
import { activityLabel } from './activity';
import { RichContent } from './RichContent';
import { useRunnerSettings } from './RunnerSettings';
import { TelemetryPanel } from './TelemetryPanel';
import { useEffect, useMemo, useRef, useState, type DragEvent, type FormEvent } from 'react';
import { type CommandStatus, type Session } from '@lazybox/protocol';
import { Console, RequestCard } from './session-parts';
import type { SurfaceTarget, ViewStateStore } from '@lazybox/platform';
import { readDraft, writeDraft, readLaunchChoices, writeLaunchChoices, type FileReference } from './view-state';
import { RunnerProvider, useRunner, type RunnerConnection } from './RunnerContext';
import { Icon } from './Icons';
export type { RunnerConnection } from './RunnerContext';
import './style.css';
import { sessionStatus, statusLabel, accountLabel } from './agent-status';

export type AgentWindowProps = {
  connection: RunnerConnection;
  purpose?: 'coding' | 'help' | 'planning' | 'summary';
  assistantContext?: string;
  /** Initial choices. Starting a session always requires an explicit click. */
  agent?: 'claude' | 'codex';
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
  onSessionChange?: (sessionId: string) => void;
  onOpenViewer?: (sessionId: string, commandId: string) => void | Promise<void>;
};

/** One independent view of a Runner session. Unmounting disconnects readers,
 * never execution. Changing the connection resets the view and its readers. */
export function AgentWindow(props: AgentWindowProps) {
  // A detached chat is visually embedded but still owns its window's keyboard
  // dispatcher. Workspace-hosted panels use their Workspace's dispatcher.
  const keyboardHint = useKeyboardNavigation(props.viewState);
  return <RunnerProvider connection={props.connection}><ChatPanel {...props} keyboardHint={keyboardHint}/></RunnerProvider>;
}

export function ChatPanel({ purpose = 'coding', assistantContext = '', agent, workingDirectory = '', emptyWorkspace = false, initialSessionId = '', collectionId = '', embedded = false, showCommands = true, allowStart = true, autoStart = false, onOpenChat, initialCommandId = '', initialCommandsOpen, mode = 'agent', viewState, onViewChange, onSessionChange, onOpenViewer, keyboardHint = '' }: Omit<AgentWindowProps, 'connection'> & { keyboardHint?: string }) {
  const { api, info, connecting, error: connectionError, refresh } = useRunner();
  const keymap=useKeymap(viewState);
  const composer=useRef<HTMLTextAreaElement>(null);
  const [snippetsOpen,setSnippetsOpen]=useState(false);
  useKeyboardAction('snippets.open',()=>{if(canCompose&&(document.activeElement===composer.current||document.querySelector('[data-agent-composer]')===composer.current))setSnippetsOpen(true);});
  useKeyboardAction('chat.send',()=>{const focused=document.activeElement as HTMLTextAreaElement;if(focused===composer.current&&canSend)focused.form?.requestSubmit();});
  const [remembered] = useState(() => {const choices=readLaunchChoices(viewState);return agent&&agent!==choices.provider ? {} : choices;});
  const {settings: runnerSettings, loading: settingsLoading} = useRunnerSettings();
  const [provider, setProvider] = useState<string>(agent || remembered.provider || 'claude');
  const [profile, setProfile] = useState(remembered.profile || '');
  const [rotationProfiles, setRotationProfiles] = useState<string[]>([]);
  const assistant = purpose !== 'coding';
  const [cwd, setCwd] = useState(workingDirectory);
  const [model, setModel] = useState(remembered.model || '');
  const [gateway, setGateway] = useState(remembered.gateway || '');
  const [instructions, setInstructions] = useState('');
  const quickStarted = useRef(false);
  const [permission, setPermission] = useState('');
  const [workflow, setWorkflow] = useState('execute');
  const [sessionID, setSessionID] = useState(initialSessionId);
  const transcript = useTranscript(api, sessionID, !!info);
  const [session, setSession] = useState<Session | undefined>(transcript.reader.session);
  const { recent: events, messages: conversation, status: stream } = transcript;
  const scroll=useChatScroll(transcript.reader,conversation,session?.pending.length);
  const {visibleMessages,followingLatest,jumpToLatest}=scroll;
  const [message, setMessage] = useState('');
  const [files, setFiles] = useState<File[]>([]);
  const [missingFiles, setMissingFiles] = useState<FileReference[]>([]);
  const [commandsOpen, setCommandsOpen] = useState(initialCommandsOpen ?? !!initialCommandId);
  const [uncertainSend, setUncertainSend] = useState('');
  const [sending, setSending] = useState(false);
  const [commands, setCommands] = useState<CommandStatus[]>([]);
  const [command, setCommand] = useState(initialCommandId);
  const [shell, setShell] = useState('');
  const [error, setError] = useState('');
  const [opening, setOpening] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [renaming,setRenaming]=useState(false),[conversationName,setConversationName]=useState(''),[savingName,setSavingName]=useState(false);
  const [deleting,setDeleting]=useState(false);
  const [previewFile,setPreviewFile] = useState<FileLink>();
  const [resuming,setResuming]=useState(false);
  const [processesStopped,setProcessesStopped]=useState(false);
  const openingRef = useRef(false);
  const sendingRef = useRef(false);
  const lifetime = useRef<AbortController | null>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const sendAttempt = useRef<{ id: string; text: string; files: File[] } | null>(null);
  const canResumeOnSend = !!session?.closed && !session.resumeBlocked && !session.recoveryRequired && !!session.providerReference && !!session.capabilities?.resume;
  const canCompose = !!session && (!session.closed || canResumeOnSend);
  const canAttach = !!session?.capabilities?.files && !!info?.attachmentLimits && canCompose && !sending;
  const canSend = canCompose && (!session?.fault || canResumeOnSend) && !sending && missingFiles.length === 0 && (!!message.trim() || files.length > 0);

  useEffect(() => {
    try {
      const draft = readDraft(viewState, sessionID);
      setMessage(draft.text); setMissingFiles(draft.files); setFiles([]); setUncertainSend(draft.operationId); sendAttempt.current = null;
    } catch (e) { setError(`Could not restore this window's draft: ${String(e)}`); }
  }, [viewState, sessionID]);
  const persistDraft = (text: string, selected: File[], missing: FileReference[], operationId = '') => {
    try { writeDraft(viewState, sessionID, { text, files: [...selected.map(file => ({ name: file.name, size: file.size, lastModified: file.lastModified })), ...missing], operationId }); }
    catch (e) { setError(`Could not save this window's draft: ${String(e)}`); }
  };

  useEffect(() => {
    const control = new AbortController(); lifetime.current = control;
    return () => { control.abort(); lifetime.current = null; };
  }, []);

  const profiles = info?.profiles.filter(p => p.provider === provider) || [];
  const selectedProfile = profiles.find(p => p.id === profile)?.id || profiles.find(p => p.managed)?.id || profiles[0]?.id || '';
  const connected = !!info;
  useEffect(() => {
    if (!sessionID || !info) return;
    const control = new AbortController(); setSession(transcript.reader.session); setCommands([]);
    let refreshing = false;
    const update = async () => {
      if (refreshing || control.signal.aborted) return;
      refreshing = true;
      try {
        const [state, output] = await Promise.all([api.getSession({ sessionId: sessionID }, { signal: control.signal }), showCommands ? api.listCommands({ sessionId: sessionID }, { signal: control.signal }) : Promise.resolve({ commands: [] })]);
        if (!control.signal.aborted) {
          if (state.session?.successorExecutionId && !sendingRef.current) {
            const next=await api.getSession({sessionId:state.session.successorExecutionId},{signal:control.signal});
            if(!control.signal.aborted&&next.session){continueConversation(api,sessionID,next.session);setSessionID(next.session.id);onSessionChange?.(next.session.id);}
          } else { transcript.reader.session=state.session; setSession(state.session); setCommands(output.commands); }
        }
      } catch (e) { if (!control.signal.aborted) setError(String(e)); }
      finally { refreshing = false; }
    };
    void update(); const timer = setInterval(() => void update(), 1500);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, sessionID, connected, showCommands]);
  const open = async (event?: FormEvent) => {
    event?.preventDefault();
    if (openingRef.current || !selectedProfile || !allowStart) return;
    openingRef.current = true; setOpening(true); setError('');
    const signal = lifetime.current!.signal;
    try {
      const options = { model, permission, workflow, gatewayId: gateway, rotationProfileIds: rotationProfiles, instructions };
      const result = assistant ? await api.openAssistant({ profileId: selectedProfile, purpose, context: assistantContext, collectionId, options }, { signal }) : await api.openSession({ profileId: selectedProfile, cwd: collectionId || emptyWorkspace ? '' : cwd, collectionId, emptyWorkspace, options }, { signal });
      if (!result.session) throw new Error('Runner did not return a session');
      if (!signal.aborted) { try { writeLaunchChoices(viewState, {provider, profile:selectedProfile, model, gateway}); } catch { /* Session remains usable if local preferences cannot be saved. */ } setSessionID(result.session.id); setCommand(''); onSessionChange?.(result.session.id); }
    } catch (e) { if (!signal.aborted) setError(String(e)); }
    finally { if (!signal.aborted) { openingRef.current = false; setOpening(false); } }
  };
  useEffect(() => {
    if (!autoStart || quickStarted.current || sessionID || !info || settingsLoading || !selectedProfile || !allowStart) return;
    if (remembered.profile && !profiles.some(p => p.id === remembered.profile)) {
      quickStarted.current = true;setError('The last account is no longer available. Choose an account to start this session.');return;
    }
    let cancelled = false;
    queueMicrotask(() => {if(!cancelled&&!quickStarted.current){quickStarted.current = true;void open();}});
    return () => {cancelled=true;};
  }, [autoStart, sessionID, info, settingsLoading, selectedProfile, allowStart]);
  const send = async (event?: FormEvent, snippetText?:string) => {
    event?.preventDefault();
    const text=snippetText??message;
    if(sendingRef.current||!(snippetText?canCompose&&!session?.fault&&!sending&&!message.trim()&&!files.length&&!missingFiles.length:canSend))return false;
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
        continueConversation(api,sessionID,resumed.session);
        destination = resumed.session.id;
        // A view may close while delivery is in flight. Retain the same draft
        // and operation identity under the successor before sending any bytes.
        writeDraft(viewState, destination, { text, files: files.map(file => ({ name: file.name, size: file.size, lastModified: file.lastModified })), operationId: attempt.id });
      }
      await api.apply({ sessionId: destination, operationId: attempt.id, operation: { case: 'message', value: { text, attachments } } }, { signal });
      if (!signal.aborted) { setMessage(''); setFiles([]); setUncertainSend(''); sendAttempt.current = null; persistDraft('', [], []); if (destination !== sessionID) { writeDraft(viewState, destination, { text: '', files: [], operationId: '' }); setSessionID(destination); onSessionChange?.(destination); refresh(); } }
      return !signal.aborted;
    } catch (e) { if (!signal.aborted) {setError(String(e));if(snippetText)setMessage(text);} return false; }
    finally { if (!signal.aborted) { sendingRef.current = false; setSending(false); } }
  };
  const selectFiles = (selected: FileList | File[] | null) => {
    const limits = info?.attachmentLimits;
    if (!limits) return;
    // Different client files can share a name, size, and modification time.
    // Preserve the selected bytes; the Runner deduplicates by content.
    const next = [...files, ...Array.from(selected || [])];
    const remaining = [...missingFiles];
    let changed = false;
    for (const file of Array.from(selected || [])) {
      const index = remaining.findIndex(saved => saved.name === file.name && saved.size === file.size && saved.lastModified === file.lastModified);
      if (index >= 0) remaining.splice(index, 1); else changed = true;
    }
    if (next.length + remaining.length > limits.maxFiles || next.some(file => BigInt(file.size) > limits.maxFileBytes) || [...next, ...remaining].reduce((size, file) => size + BigInt(file.size), 0n) > limits.maxMessageBytes) {
      setError(`Attach up to ${limits.maxFiles} files, ${Number(limits.maxFileBytes) / (1 << 20)} MiB per file and ${Number(limits.maxMessageBytes) / (1 << 20)} MiB total.`); return;
    }
    if (changed) { setUncertainSend(''); sendAttempt.current = null; }
    setFiles(next); setMissingFiles(remaining); setError(''); persistDraft(message, next, remaining, changed ? '' : uncertainSend);
  };
  const interrupt = async () => {
    try { await api.apply({ sessionId: sessionID, operationId: crypto.randomUUID(), operation: { case: 'interrupt', value: {} } }); }
    catch (e) { setError(String(e)); }
  };
  const close = async () => {
    try { const result = await api.stopSessionResources({ sessionId: sessionID }); setSession(result.session); }
    catch (e) { setError(String(e)); }
  };
  const startCommand = async (event: FormEvent) => {
    event.preventDefault();
    if (!shell.trim()) return;
    try { const output = await api.startCommand({ sessionId: sessionID, title: shell.slice(0, 64), argv: ['/bin/sh', '-c', shell] }); setCommand(output.id); }
    catch (e) { setError(String(e)); }
  };
  const activity=activityLabel(events,session);
  const activeProfile = info?.profiles.find(p => p.id === session?.profileId);
  const agentName = (activeProfile?.provider || provider) === 'claude' ? 'Claude' : 'Codex';
  const currentCommand = command || commands.at(-1)?.id;
  useEffect(() => {
    if (!onViewChange) return;
    void (async () => { try { await onViewChange({ sessionId: sessionID, commandId: currentCommand || '', commandsOpen }); } catch (e) { setError(`Could not save this window: ${String(e)}`); } })();
  }, [onViewChange, sessionID, currentCommand, commandsOpen]);
  const newSession = () => {
    setSessionID(''); setSession(undefined); setCommands([]); setCommand(''); setMessage(''); setFiles([]); setMissingFiles([]); setUncertainSend(''); sendAttempt.current = null; setError(''); onSessionChange?.('');
  };
  const resumeSession=async()=>{
    if(resuming)return;setResuming(true);setError('');
    try{const result=await api.resumeSession({sessionId:sessionID,processesStopped},{signal:lifetime.current!.signal});if(result.session){continueConversation(api,sessionID,result.session);setSessionID(result.session.id);setProcessesStopped(false);onSessionChange?.(result.session.id);refresh();}}
    catch(error){setError(String(error));}finally{setResuming(false);}
  };
  const isFileDrag = (event: DragEvent) => Array.from(event.dataTransfer.types).includes('Files');
  const dragOver = (event: DragEvent) => {
    if (!isFileDrag(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = canAttach ? 'copy' : 'none';
    setDragging(canAttach);
  };
  const dropFiles = (event: DragEvent) => {
    if (!isFileDrag(event)) return;
    // Suppress the webview's default navigation to the dropped file, including
    // when this conversation cannot accept an attachment.
    event.preventDefault();
    event.stopPropagation();
    setDragging(false);
    if (!canAttach) { setError('This conversation cannot accept files right now.'); return; }
    if (Array.from(event.dataTransfer.items).some(item => item.webkitGetAsEntry?.()?.isDirectory)) {
      setError('Attach files individually; folders are not supported.'); return;
    }
    selectFiles(event.dataTransfer.files);
  };
  if (mode === 'console') return <main className="agent-window console-window"><header className="topbar"><div><h1>Command console</h1><p>Independent view · execution stays on the Runner</p></div></header>{error && <div role="alert" className="error">{error}</div>}{currentCommand ? <Console api={api} command={currentCommand} viewState={viewState} onError={setError}/> : <div className="empty">No command is selected.</div>}</main>;
  return <main className={'agent-window' + (embedded ? ' embedded-chat' : '') + (dragging ? ' file-drag-active' : '')}
    onDragEnter={dragOver} onDragOver={dragOver} onDrop={dropFiles}
    onDragLeave={event => {
      const next = event.relatedTarget;
      if (!(next instanceof Node) || !event.currentTarget.contains(next)) setDragging(false);
    }}>
    {keyboardHint&&!embedded&&<div className="keyboard-sequence-hint" role="status">{keyboardHint}</div>}
    {snippetsOpen&&<SnippetPicker store={viewState} canSend={canCompose&&!sending&&!message.trim()&&!files.length&&!missingFiles.length} onClose={()=>{setSnippetsOpen(false);composer.current?.focus();}} onInsert={body=>{const node=composer.current,start=node?.selectionStart??message.length,end=node?.selectionEnd??message.length;const next=message.slice(0,start)+body+message.slice(end);setMessage(next);persistDraft(next,files,missingFiles);setUncertainSend('');sendAttempt.current=null;requestAnimationFrame(()=>{node?.focus();node?.setSelectionRange(start+body.length,start+body.length);});}} onSend={text=>send(undefined,text)}/>}
    {!embedded && <header className="topbar"><span className="brand" aria-hidden="true">l<span>↗</span></span><div><h1>lazybox <span>V2</span></h1><p>One window. Your agent.</p></div><span className="pill" role="status">{connecting ? 'Connecting to Runner' : !info ? 'Runner unavailable' : sessionID ? sessionStatus(session) : 'Runner connected'}</span></header>}
    {(error || connectionError) && !(embedded && connectionError) && <div className="error" role="alert">{error || connectionError}<button onClick={() => setError('')}>Dismiss</button></div>}
    {!info && !connecting && <div className="connection-retry"><button onClick={refresh}>Retry connection</button></div>}
    {!sessionID && !allowStart ? <div className="empty">Select an existing session to view its chat.</div> : !sessionID ? <section className="session-setup"><div className="eyebrow">New conversation</div><h2>{emptyWorkspace ? 'Empty workspace' : 'Choose your agent.'}</h2><p>{assistant ? `${purpose === 'help' ? 'Ask about Lazybox' : purpose === 'planning' ? 'Discuss a plan' : 'Summarize supplied evidence'}. This conversation runs on the Runner.` : emptyWorkspace ? 'Start a conversation with room to create files. No repository needed.' : 'Connect to your project and start working.'}</p>
      <form onSubmit={open} className="open-form"><div className="setup-row"><label>Agent<select value={provider} disabled={!info || opening} onChange={e => { setProvider(e.target.value); setProfile(''); setModel(''); setGateway(''); setRotationProfiles([]); }}><option value="claude">Claude</option><option value="codex">Codex</option></select></label><label>Account profile<select value={selectedProfile} disabled={!profiles.length || opening} onChange={e => setProfile(e.target.value)}>{profiles.map(p => <option key={p.id} value={p.id}>{p.label || p.id}{p.availability && p.availability.state !== 'available' && p.availability.state !== 'unknown' ? ` · ${accountLabel(p.availability.state)}` : ''}</option>)}</select></label></div>
        {info && !profiles.length && <div className="error">No {agentName} profile is configured on this Runner.</div>}
        {assistant ? <p className="collection-context">Advice and proposals · no repository required</p> : emptyWorkspace ? <p className="collection-context">A private folder is created on the Runner. Your files remain after the session ends.</p> : collectionId ? <p className="collection-context">Uses the selected workspace and its repositories</p> : <label>Working directory<input required disabled={opening} placeholder="/absolute/path/to/project" value={cwd} onChange={e => setCwd(e.target.value)}/></label>}
        <label>Permissions<select disabled={opening || assistant} value={assistant ? 'read-only' : permission} onChange={e => setPermission(e.target.value)}><option value="">Runner default{runnerSettings?.permission ? ` · ${runnerSettings.permission === 'yolo' ? 'YOLO' : runnerSettings.permission}` : ' · current CLI settings'}</option><option value="inherit">Current CLI settings</option><option value="ask">Ask for approval</option><option value="read-only">Read only</option><option value="yolo" disabled={workflow === 'plan'}>YOLO · full access, no approval prompts</option></select></label>
        {(permission==='yolo'||(!permission&&runnerSettings?.permission==='yolo'))&&!assistant&&<small>Full tool access for this session. Existing conversations keep their original permissions.</small>}
        <details className="session-options"><summary>Model, gateway & context</summary><div className="options-grid"><label>Model<input disabled={opening} placeholder="Provider default" value={model} onChange={e => setModel(e.target.value)}/></label><label>Gateway<select disabled={opening} value={gateway} onChange={e => setGateway(e.target.value)}><option value="">Runner default</option><option value="direct">Direct</option>{info?.gateways.filter(g => g.provider === provider).map(g => <option key={g.id} value={g.id}>{g.id==='metered'?'Metered · track workspace cost':g.id}</option>)}</select></label>{!assistant && <label>Workflow<select disabled={opening} value={workflow} onChange={e => {setWorkflow(e.target.value);if(e.target.value==='plan'&&permission==='yolo')setPermission('read-only');}}><option value="execute">Execute</option><option value="plan">Plan</option></select></label>}</div><label>Additional session context<textarea rows={3} maxLength={32768} value={instructions} disabled={opening} onChange={e=>setInstructions(e.target.value)} placeholder="Instructions for this conversation"/></label><small>Shared Lazybox and provider context can be edited in Settings → Agents.</small></details>
        {profiles.some(p => p.isolation === 'environment' && p.id !== selectedProfile) && <details className="session-options"><summary>Subscription rotation</summary><p>Allow switching to these saved subscriptions when this account needs sign-in or reaches a limit.</p>{profiles.filter(p => p.isolation === 'environment').map(p => <label key={p.id} className="rotation-choice"><input type="checkbox" checked={rotationProfiles.includes(p.id)} disabled={opening} onChange={e => setRotationProfiles(value => e.target.checked ? [...value, p.id] : value.filter(id => id !== p.id))}/>{p.label || p.id}</label>)}</details>}
        <button className="primary" disabled={opening || !info || !selectedProfile}>{opening ? `Starting ${agentName}…` : `Start ${agentName} ↗`}</button>
      </form><small>Uses the selected Runner account. Closing this window leaves your agent running.</small>
    </section> : <section className="single-session">
      {session && <div className="conversation-title"><strong>{session.title || 'Conversation'}</strong><button className="icon-button" aria-label="Rename conversation" onClick={()=>{setConversationName(session.title);setRenaming(true);}}>Rename</button></div>}
      {renaming && <form className="conversation-rename" onSubmit={async event=>{event.preventDefault();if(savingName)return;setSavingName(true);try{const result=await api.renameConversation({sessionId:sessionID,title:conversationName});setSession(result.session);setRenaming(false);refresh();}catch(error){setError(String(error));}finally{setSavingName(false);}}}><label>Conversation name<input autoFocus required maxLength={120} value={conversationName} disabled={savingName} onChange={event=>setConversationName(event.target.value)}/></label><button disabled={savingName||!conversationName.trim()}>Save conversation name</button><button type="button" disabled={savingName} onClick={()=>setRenaming(false)}>Cancel</button></form>}

      {embedded ? <div className="chat-heading"><span className={'agent-avatar ' + agentName.toLowerCase()}>{agentName === 'Claude' ? '✳︎' : '›'}</span><div><h2>{agentName}<span className="chat-model">{sessionModelLabel(session,transcript.reportedModel)}</span></h2>{sessionAccountLabel(session,activeProfile)!=='Account not reported'&&<small className="chat-account">{sessionAccountLabel(session,activeProfile)}</small>}<span className="chat-status" data-state={session?.closed?'saved':session?.status} role="status"><i/>{sessionStatus(session)}{session?.queued ? ` · ${session.queued} queued` : ''}</span></div><div className="chat-heading-actions">{onOpenChat && <button className="icon-button" aria-label="Open chat window ↗" title="Open in a separate window" onClick={onOpenChat}><Icon name="external"/></button>}<details className="session-menu"><summary aria-label="Session actions"><Icon name="more"/></summary><div><strong>Session</strong><small>{sessionAccountLabel(session,activeProfile)}</small><small>Gateway: {session?.gatewayId || 'direct'}</small><small className="mono">{sessionID}</small><small>{stream}</small>{session?.closed ? allowStart ? <button onClick={newSession}>New session</button> : <span>Saved conversation</span> : <button disabled={!session} onClick={() => void close()}>End session</button>}<button className="danger" disabled={!session} onClick={()=>setDeleting(true)}>Delete conversation…</button></div></details></div></div> : <><div className="panel-title"><div><span className="eyebrow">Agent session</span><h2>{agentName}<span className="chat-model">{sessionModelLabel(session,transcript.reportedModel)}</span></h2></div>{session?.closed ? allowStart ? <button onClick={newSession}>New session</button> : <span>Saved conversation</span> : <button disabled={!session} onClick={() => void close()}>End session</button>}</div><div className="session-meta"><span role="status">{sessionStatus(session)}</span><span>{sessionAccountLabel(session,activeProfile)}</span><span>Gateway: {session?.gatewayId || 'direct'}</span><span>{session?.queued || 0} queued</span><span className="mono">{sessionID.slice(0, 12)}</span></div></>}
      {dragging && <div className="file-drop-hint" role="status">Drop files to attach</div>}
      {session?.closed&&<div className="resume-bar"><span>{'Saved conversation'}</span>{session.resumeBlocked?<p>{session.resumeBlocked}</p>:canResumeOnSend?<small>Send a message to continue with this conversation’s saved account and permissions.</small>:<>{session.recoveryRequired&&<label><input type="checkbox" checked={processesStopped} onChange={event=>setProcessesStopped(event.target.checked)}/>I have stopped or reconciled the previous agent and its commands.</label>}<button disabled={resuming||!session.providerReference||!session.capabilities?.resume||(session.recoveryRequired&&!processesStopped)} onClick={()=>void resumeSession()}>{resuming?'Resuming…':'Resume conversation'}</button></>}</div>}
      {deleting&&session&&<DeleteConversationDialog session={session} onClose={()=>setDeleting(false)} onDeleted={()=>{setDeleting(false);newSession();}}/>}
    {previewFile && session?.collectionId && <FilePreview workspaceId={session.collectionId} reference={previewFile} onClose={()=>setPreviewFile(undefined)}/>}
      {sessionID && <div className="agent-inspection-bar"><TelemetryPanel executionId={sessionID} conversationId={session?.conversationId} compact/><ToolActivity events={transcript.activity} truncated={transcript.activityTruncated}/></div>}
      {session?.account && ['auth_required', 'exhausted', 'throttled', 'billing_error', 'context_full', 'budget_exceeded'].includes(session.account.state) && <div className="account-notice" role="status">{accountLabel(session.account.state)}{session.account.resetAt > 0n ? ` · resets ${new Date(Number(session.account.resetAt) * 1000).toLocaleString()}` : ''}</div>}
    {session && session.agents.length > 1 && <details className="agent-activity"><summary>Agent activity ({session.agents.length - 1})</summary><ul>{session.agents.filter(a => a.id !== 'root').map(a => <li key={a.id} data-parent={a.parentId}><span>{a.name || 'Sub-agent'}</span><small>{statusLabel(a.status)}</small></li>)}</ul>{session.activityTruncated && <small>Additional activity was omitted.</small>}</details>}
      {transcript.error && <div className="transcript-notice error" role="alert"><span>Conversation could not be loaded. {transcript.error}</span><button onClick={transcript.retry}>Retry conversation</button></div>}
      <div className="messages" ref={scroll.paneRef} onScroll={scroll.onScroll}><div className="messages-content" ref={scroll.contentRef}>
      {conversation.length > visibleMessages && <button onClick={scroll.showEarlier}>Show earlier messages</button>}
      {transcript.loading && !conversation.length && <p role="status">Loading conversation…</p>}
      {!transcript.loading && !transcript.error && !conversation.length && <div className="empty"><span>READY TO WORK</span><h2>What are we building?</h2><p>Send {agentName} a message to get started.</p></div>}{session?.fault && <div className="error" role="alert">{session.fault}</div>}{conversation.slice(-visibleMessages).map(m => <article className={'message ' + m.type} key={m.id}><span className="eyebrow">{m.type === 'operation_accepted' ? 'You' : m.type === 'failure' ? 'Provider failure' : m.type === 'notice' ? 'Runner' : agentName}</span><RichContent text={m.text} onOpenFile={session?.collectionId ? setPreviewFile : undefined}/>{m.files?.length ? <ul className="sent-files" aria-label="Sent attachments">{m.files.map((file, i) => <li key={i}>↳ {file.name} · {Math.ceil(file.size / 1024)} KiB</li>)}</ul> : null}</article>)}{session?.pending.map(p => <RequestCard key={p.requestId} request={p} api={api} session={sessionID} onError={setError}/>)}</div></div>
      {!followingLatest && conversation.length > 0 && <button className="jump-to-latest" onClick={jumpToLatest}>Jump to latest</button>}
      {!transcript.error && (activity||stream.startsWith('Disconnected')||stream==='Reconnecting')&&<div className="live-activity" role="status"><i/>{stream.startsWith('Disconnected')||stream==='Reconnecting'?'Connection interrupted · reconnecting…':activity}</div>}
      <form className="composer" onSubmit={send}>
        {files.length > 0 && <ul className="attached-files" aria-label="Attached files">{files.map((file, i) => <li key={i}><span>{file.name} <small>{Math.ceil(file.size / 1024)} KiB</small></span><button type="button" aria-label={`Remove ${file.name}`} disabled={sending} onClick={() => { const next = files.filter((_, index) => index !== i); setFiles(next); setUncertainSend(''); persistDraft(message, next, missingFiles); }}>×</button></li>)}</ul>}
        {missingFiles.length > 0 && <div className="restore-files"><p>Reattach or remove these files before sending:</p>{missingFiles.map((file, i) => <button key={i} type="button" onClick={() => { const next = missingFiles.filter((_, index) => index !== i); setMissingFiles(next); setUncertainSend(''); persistDraft(message, files, next); }}>Remove {file.name}</button>)}</div>}
        {uncertainSend && !sending && <small className="composer-hint">The previous send was not confirmed. Retrying the unchanged message checks the same operation.</small>}
        <textarea ref={composer} data-agent-composer aria-label="Message" placeholder={`Message ${agentName}…`} value={message} onChange={e => { setMessage(e.target.value); setUncertainSend(''); sendAttempt.current = null; persistDraft(e.target.value, files, missingFiles); }} disabled={!canCompose || sending}
          />        <div className="row"><button type="button" className="icon-button" aria-label="Snippets" title={`Snippets · ${keymap['snippets.open']}`} disabled={!canCompose||sending} onClick={()=>setSnippetsOpen(true)}><Icon name="snippets"/></button><button type="button" className="attachment" aria-label={embedded ? 'Attach files' : undefined} disabled={!canAttach} onClick={() => fileInput.current?.click()}>{embedded ? <><Icon name="attach"/> Attach</> : '+ Attach files'}</button><input ref={fileInput} hidden aria-label="Attach files" type="file" multiple onChange={e => { selectFiles(e.target.files); e.target.value = ''; }}/><small className="composer-hint keyboard-hint">{keymap['chat.send']} to send · Shift+Enter for a new line</small>{(!embedded || session?.busy) && <button type="button" onClick={() => void interrupt()} disabled={!session?.busy || session.closed}>Interrupt</button>}<button className="primary send-button" aria-label={sending ? 'Sending…' : session?.busy ? 'Queue follow-up' : 'Send message'} disabled={!canSend}>{embedded ? <Icon name="send" size={18}/> : sending ? 'Sending…' : session?.busy ? 'Queue follow-up' : 'Send message'}</button></div>
      </form>
      {showCommands && !assistant && <details className="session-commands" open={commandsOpen} onToggle={event => setCommandsOpen(event.currentTarget.open)}><summary>Commands & output {commands.length > 0 && `(${commands.length})`}</summary><form onSubmit={startCommand} className="command-form"><input aria-label="Shell command" value={shell} onChange={e => setShell(e.target.value)} disabled={!session || session.closed}/><button disabled={!session || session.closed}>Run</button></form><div className="command-tabs">{commands.map(c => <button key={c.id} className={currentCommand === c.id ? 'selected' : ''} onClick={() => setCommand(c.id)}>{c.done ? '◦' : '●'} {c.title.slice(0, 28)}</button>)}</div>{currentCommand && <><Console key={currentCommand} api={api} command={currentCommand} viewState={viewState} onError={setError} onOpenViewer={onOpenViewer ? async () => { try { await onOpenViewer(sessionID, currentCommand); } catch (e) { setError(String(e)); } } : undefined}/><button className="cancel" onClick={async () => { try { await api.cancelCommand({ commandId: currentCommand }); } catch (e) { setError(String(e)); } }}>Cancel owned command</button></>}</details>}
      {!embedded && <details className="event-detail"><summary>Operation receipts & events ({events.length})</summary><pre>{events.slice(-40).map(e => `${e.cursor} ${e.event?.type} ${e.event?.status || e.receipt?.delivery || ''} ${e.receipt?.providerId || e.event?.turnId || ''}`).join('\n')}</pre></details>}
    </section>}
  </main>;
}
