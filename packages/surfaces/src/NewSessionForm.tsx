import { BehaviorOptions, behaviorForProvider, CHECK_IN_FIRST, readDefaultBehavior, saveDefaultBehavior } from './BehaviorProfile';
import { useEffect, useRef, useState, type FormEvent, type RefObject } from 'react';
import { SubscriptionOptions } from './SubscriptionGroups';
import type { RunnerClient } from '@lazybox/client-core';
import type { InfoResponse } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { AgentPicker } from './AgentPicker';
import { useRunnerSettings } from './RunnerSettings';
import { conversationDefaults, writeLaunchChoices } from './view-state';

import { showToast } from './toasts';

export type ConversationCopy = { provider: string; model: string; title: string };

type Purpose = 'coding' | 'help' | 'planning' | 'summary';

/**
 * The choices a new conversation starts with (agent, account, permissions,
 * model, gateway, workflow and context) and the start itself. Remembered
 * choices are reused only for the same agent. Starting always takes an
 * explicit click, except an explicit Quick start (`autoStart`).
 */
export function useSessionLauncher({ api, info, viewState, agent, copy, workingDirectory, purpose, assistantContext, collectionId, emptyWorkspace, allowStart, autoStart, sessionID, lifetime, onError, onStarted }: {
  api: RunnerClient;
  info?: InfoResponse;
  viewState?: ViewStateStore;
  agent?: 'claude' | 'codex';
  copy?: ConversationCopy;
  workingDirectory: string;
  purpose: Purpose;
  assistantContext: string;
  collectionId: string;
  emptyWorkspace: boolean;
  allowStart: boolean;
  autoStart: boolean;
  sessionID: string;
  lifetime: RefObject<AbortController | null>;
  onError(message: string): void;
  onStarted(sessionId: string, collectionId?: string): void;
}) {
  const [remembered] = useState(() => { const choices = conversationDefaults(viewState, info); return copy ? { provider: copy.provider, model: copy.model } : agent && agent !== choices.provider ? {} : choices; });
  const { settings: runnerSettings, loading: settingsLoading } = useRunnerSettings();
  const [provider, setProvider] = useState<string>(agent || remembered.provider || 'claude');
  const [title, setTitle] = useState(copy?.title || '');
  const [profile, setProfile] = useState(remembered.profile || '');
  // On by default: the Runner may continue on another signed-in subscription of this provider.
  const [rotate, setRotate] = useState(true);
  const assistant = purpose !== 'coding';
  const [cwd, setCwd] = useState(workingDirectory);
  const [model, setModel] = useState(remembered.model || '');
  const [gateway, setGateway] = useState(remembered.gateway || '');
  const [instructions, setInstructions] = useState('');
  const [behaviorProfile, setBehaviorProfile] = useState(() => behaviorForProvider(readDefaultBehavior(viewState), provider));
  const quickStarted = useRef(false);
  const [permission, setPermission] = useState('');
  const [workflow, setWorkflow] = useState('execute');
  const checkInFirst = behaviorProfile === CHECK_IN_FIRST;
  const [opening, setOpening] = useState(false);
  const openingRef = useRef(false);

  const profiles = info?.profiles.filter(p => p.provider === provider) || [];
  const gateways = info?.gateways.filter(g => g.provider === provider) || [];
  const selectedProfile = profiles.find(p => p.id === profile)?.id || profiles.find(p => p.managed)?.id || profiles[0]?.id || '';

  const open = async (event?: FormEvent) => {
    event?.preventDefault();
    if (openingRef.current || !selectedProfile || !allowStart || (copy && !title.trim())) return;
    openingRef.current = true; setOpening(true); onError('');
    const signal = lifetime.current!.signal;
    try {
      const options = { model, permission: checkInFirst ? 'read-only' : permission, workflow, gatewayId: gateway, rotationProfileIds: [], noRotation: !rotate, instructions, behaviorProfile: assistant ? '' : behaviorProfile };
      const result = assistant ? await api.openAssistant({ profileId: selectedProfile, purpose, context: assistantContext, collectionId, options }, { signal }) : await api.openSession({ profileId: selectedProfile, cwd: collectionId || emptyWorkspace ? '' : cwd, collectionId, emptyWorkspace, options }, { signal });
      if (!result.session) throw new Error('Runner did not return a session');
      if (signal.aborted) return;
      if (!assistant) {
        try {
          writeLaunchChoices(viewState, { provider, profile: selectedProfile, model, gateway, behaviorProfile });
          saveDefaultBehavior(viewState, behaviorProfile);
        } catch { /* The conversation can still open. */ }
      }
      if (title.trim()) {
        try { await api.renameConversation({ sessionId: result.session.id, title: title.trim() }, { signal }); }
        catch { if (!signal.aborted) showToast({ tone: 'info', title: 'Conversation started', body: 'Could not save its name. Rename it from the chat header.' }); }
      }
      if (!signal.aborted) onStarted(result.session.id, result.session.collectionId);
    } catch (e) { if (!signal.aborted) onError(String(e)); }
    finally { if (!signal.aborted) { openingRef.current = false; setOpening(false); } }
  };

  useEffect(() => {
    if (!autoStart || quickStarted.current || sessionID || !info || settingsLoading || !allowStart) return;
    if (remembered.profile && !profiles.some(p => p.id === remembered.profile)) {
      quickStarted.current = true; onError('The last account is no longer available. Choose an account to start this session.'); return;
    }
    if (!selectedProfile) return;
    let cancelled = false;
    queueMicrotask(() => { if (!cancelled && !quickStarted.current) { quickStarted.current = true; void open(); } });
    return () => { cancelled = true; };
  }, [autoStart, sessionID, info, settingsLoading, selectedProfile, allowStart]);

  return {
    runnerSettings, copy, title, setTitle, provider, setProvider, setProfile, rotate, setRotate, cwd, setCwd, model, setModel, gateway, setGateway,
    instructions, setInstructions, behaviorProfile, setBehaviorProfile, checkInFirst, permission, setPermission, workflow, setWorkflow, opening, profiles, gateways, selectedProfile, open,
  };
}

export type SessionLauncher = ReturnType<typeof useSessionLauncher>;

const purposeLabel = (purpose: Purpose) => purpose === 'help' ? 'Ask about Lazybox' : purpose === 'planning' ? 'Discuss a plan' : 'Summarize supplied evidence';

/** The form that starts a new conversation. */
export function NewSessionForm({ launcher, info, purpose, emptyWorkspace, collectionId, agentName }: {
  launcher: SessionLauncher;
  info?: InfoResponse;
  purpose: Purpose;
  emptyWorkspace: boolean;
  collectionId: string;
  agentName: string;
}) {
  const { runnerSettings, copy, title, setTitle, provider, setProvider, setProfile, rotate, setRotate, cwd, setCwd, model, setModel, gateway, setGateway, instructions, setInstructions, behaviorProfile, setBehaviorProfile, checkInFirst, permission, setPermission, workflow, setWorkflow, opening, profiles, gateways, selectedProfile, open } = launcher;
  const assistant = purpose !== 'coding';
  return <section className="session-setup">
    {assistant && <p>{purposeLabel(purpose)}. This conversation runs on the Runner.</p>}
    <form onSubmit={open} className="open-form">
      {copy && <label>Conversation name<input autoFocus required maxLength={128} value={title} disabled={opening} onFocus={event => event.target.select()} onChange={event => setTitle(event.target.value)}/></label>}
      <div className="setup-row">
        <AgentPicker value={provider} disabled={!info || opening} onChange={next => { setProvider(next); setProfile(''); setModel(''); setGateway(''); setBehaviorProfile(behaviorForProvider(behaviorProfile, next)); }}/>
        <label>Account profile<select value={selectedProfile} disabled={!profiles.length || opening} onChange={e => setProfile(e.target.value)}>
          <SubscriptionOptions profiles={profiles}/>
        </select></label>
      </div>
      {info && !profiles.length && <div className="error">No {agentName} profile is configured on this Runner.</div>}
      {assistant
        ? <p className="collection-context">Advice and proposals · no repository required</p>
        : emptyWorkspace
          ? <p className="collection-context">A private folder is created on the Runner. Your files remain after the session ends.</p>
          : collectionId
            ? null
            : <label>Working directory<input required disabled={opening} placeholder="/absolute/path/to/project" value={cwd} onChange={e => setCwd(e.target.value)}/></label>}
      <label>Permissions<select disabled={opening || assistant || checkInFirst} value={assistant || checkInFirst ? 'read-only' : permission} onChange={e => setPermission(e.target.value)}>
        <option value="">Runner default{runnerSettings?.permission ? ` · ${runnerSettings.permission === 'yolo' ? 'YOLO' : runnerSettings.permission}` : ' · current CLI settings'}</option>
        <option value="inherit">Current CLI settings</option>
        <option value="ask">Ask for approval</option>
        <option value="read-only">Read only</option>
        <option value="yolo" disabled={workflow === 'plan'}>YOLO · full access, no approval prompts</option>
      </select></label>
      {(permission === 'yolo' || (!permission && runnerSettings?.permission === 'yolo')) && !assistant && !checkInFirst && <small>Full tool access for this session. Existing conversations keep their original permissions.</small>}
      {!assistant && <label>Behavior profile<select disabled={opening} value={behaviorProfile} onChange={event => setBehaviorProfile(event.target.value)}>
        <BehaviorOptions provider={provider}/>
      </select></label>}
      {checkInFirst && <small>Starts read only. Approve the plan when you’re ready. Use the session menu to change permissions if it stays read only.</small>}
      <details className="session-options" open={copy ? true : undefined}>
        <summary>Model & context</summary>
        <div className="options-grid">
          <label>Model<input disabled={opening} placeholder="Provider default" value={model} onChange={e => setModel(e.target.value)}/></label>
          {/* A gateway is a choice only where the Runner has one besides the direct connection. */}
          {(gateways.length > 0 || !!gateway) && <label>Gateway<select disabled={opening} value={gateway} onChange={e => setGateway(e.target.value)}>
            <option value="">Runner default</option>
            <option value="direct">Direct</option>
            {gateways.map(g => <option key={g.id} value={g.id}>{g.id === 'metered' ? 'Metered · track workspace cost' : g.id}</option>)}
          </select></label>}
          {!assistant && <label>Workflow<select disabled={opening} value={workflow} onChange={e => { setWorkflow(e.target.value); if (e.target.value === 'plan' && permission === 'yolo') setPermission('read-only'); }}>
            <option value="execute">Execute</option>
            <option value="plan">Plan</option>
          </select></label>}
        </div>
        <label>Additional session context<textarea rows={3} maxLength={32768} value={instructions} disabled={opening} onChange={e => setInstructions(e.target.value)} placeholder="Instructions for this conversation"/></label>
        <small>Shared Lazybox and provider context can be edited in Settings → Agents.</small>
      </details>
      {!assistant && runnerSettings?.automaticSwitchingProviders.includes(provider) && <RotationChoice profiles={profiles} selectedProfile={selectedProfile} excluded={runnerSettings?.rotationExcludedProfileIds} rotate={rotate} disabled={opening} onRotate={setRotate}/>}
      <button className="primary" disabled={opening || !info || !selectedProfile}>{opening ? `Starting ${agentName}…` : `Start ${agentName} ↗`}</button>
    </form>
    <small>Uses the selected Runner account. Closing this window leaves your agent running.</small>
  </section>;
}

/** Whether the Runner may continue on another subscription of this provider at a limit. */
function RotationChoice({ profiles, selectedProfile, excluded, rotate, disabled, onRotate }: {
  profiles: InfoResponse['profiles'];
  selectedProfile: string;
  excluded?: readonly string[];
  rotate: boolean;
  disabled: boolean;
  onRotate(rotate: boolean): void;
}) {
  const pool = profiles.filter(p => p.isolation === 'environment' && p.id !== selectedProfile && !excluded?.includes(p.id));
  const current = profiles.find(p => p.id === selectedProfile);
  if (!pool.length) return null;
  return <div className="rotation-choice">
    <label><input type="checkbox" checked={rotate} disabled={disabled} onChange={e => onRotate(e.target.checked)}/>Switch subscriptions automatically</label>
    <small>{rotate ? `When ${current?.label || 'this account'} hits a limit: ${pool.map(p => p.label || p.id).join(', ')}` : `Stays on ${current?.label || 'this account'}, even at a limit`}</small>
  </div>;
}
