import { BehaviorOptions, BEHAVIOR_PROFILES, readDefaultBehavior, saveDefaultBehavior } from './BehaviorProfile';
import { DISPLAY_PRESETS, useDisplayDetail } from './display-detail';
import { DisplayPresetPicker } from './DisplayPresetPicker';
import { writeActivityDefault } from './activity-detail';
import { lazy, Suspense, useEffect, useMemo, useRef, useState } from 'react';
import type { Collection } from '@lazybox/protocol';
import type { ViewStateStore, WorkspaceHost } from '@lazybox/platform';
import { isAcp } from './providers';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { invokeAction, readKeymap } from './keyboard';
import { displayBinding, runAction, useActions } from './actions';
import { readPreference, writePreference } from './view-state';
import { shortcut } from './workbench';
import { useRunner } from './RunnerContext';
import { useConversationIndex } from './useConversationIndex';
import type { SettingsTab } from './SettingsPage';
import { essentialKeys, releaseGroups, welcomeDecision, whatsNewDecision, WELCOME_PREFERENCE, WHATS_NEW_PREFERENCE, type Release, type ReleaseNote } from './onboarding-model';
import { releaseNotes } from './release-notes';
import { providerLine } from './help-availability';
import { useProviderAvailability } from './useProviderAvailability';
import { checkForUpdate, takeInstalling, useUpdatePreferencesStore } from './app-update';
import { postUpdateDecision } from './update-model';
import type { FeedbackShot } from './FeedbackDialog';
import './onboarding.css';
import './claude-auth.css';

// The documentation ships with the app and reads offline (DocsViewer.tsx); it
// loads only when opened.
const DocsViewer = lazy(() => import('./DocsViewer').then(m => ({ default: m.DocsViewer })));
// Help → Send feedback loads only when asked for.
const FeedbackDialog = lazy(() => import('./FeedbackDialog').then(m => ({ default: m.FeedbackDialog })));

/** What Help asks the onboarding host for. */
export type OnboardingRequest = 'welcome' | 'whats-new' | 'shortcuts' | 'docs' | 'feedback' | 'updates';
const EVENT = 'lazybox-onboarding';
/** Open the welcome, What's new or the shortcuts from anywhere in the window. */
export const openOnboarding = (request: OnboardingRequest) => window.dispatchEvent(new CustomEvent<OnboardingRequest>(EVENT, { detail: request }));
const DOCS_EVENT = 'lazybox-docs';
/** Open the documentation at a page, with an optional #anchor ("troubleshooting.md#…"). */
export const openDocs = (page = 'README.md') => window.dispatchEvent(new CustomEvent<string>(DOCS_EVENT, { detail: page }));

// A preference is a convenience: an unavailable store never breaks the window.
const read = (store: ViewStateStore | undefined, name: string) => { try { return readPreference(store, name); } catch { return null; } };
const write = (store: ViewStateStore | undefined, name: string, value: string) => { try { writePreference(store, name, value); } catch { /* shown again next time, at worst */ } };

/**
 * The first-run welcome and What's new after an update, plus what Help opens
 * (Send feedback among them). Mounted once per main window; `automatic` is
 * false in a workspace window, which only answers Help.
 */
export function Onboarding({ viewState, collections, collectionsLoaded, automatic, settingsOpen, onOpenSettings, onWorkspace, host, sessionId, workspaceId }: { workspaceId?: string; viewState?: ViewStateStore; collections: readonly Collection[]; collectionsLoaded: boolean; automatic: boolean; settingsOpen?: boolean; onOpenSettings(tab: SettingsTab): void; onWorkspace?(id: string): void; /** The window's host: the app (updates, diagnostics) and its screenshot. */ host?: WorkspaceHost; /** The conversation in front, which a feedback report may name. */ sessionId?: string }) {
  const [welcome, setWelcome] = useState(false);
  const [setupInSettings, setSetupInSettings] = useState(false);
  useEffect(() => {
    if (setupInSettings && !settingsOpen) { setSetupInSettings(false); setWelcome(true); }
  }, [setupInSettings, settingsOpen]);
  const [decided, setDecided] = useState(() => !automatic || read(viewState, WELCOME_PREFERENCE) !== null);
  const [whatsNew, setWhatsNew] = useState<{ releases: readonly Release[]; asked: boolean; updated?: string; failed?: { version: string; current: string } }>();
  // The page the documentation opens at; '' while it is closed.
  const [docs, setDocs] = useState('');
  const [feedback, setFeedback] = useState<FeedbackShot>();
  // Update choices are app-wide preferences of this window (app-update.ts).
  useUpdatePreferencesStore(automatic ? viewState : undefined);
  useEffect(() => {
    if (!automatic) return;
    const decision = whatsNewDecision(read(viewState, WHATS_NEW_PREFERENCE), releaseNotes);
    if (decision.action === 'record') write(viewState, WHATS_NEW_PREFERENCE, decision.id);
    if (decision.action === 'show') setWhatsNew({ releases: decision.releases, asked: false });
    // The launch after an install: say that it took (with what changed), or that it did not.
    const installing = takeInstalling();
    if (installing && host?.app) void host.app.info().then(info => {
      const after = postUpdateDecision(installing, info.version);
      if (after === 'updated') {
        if (releaseNotes[0]) write(viewState, WHATS_NEW_PREFERENCE, releaseNotes[0].id);
        setWhatsNew(current => ({ releases: current?.releases || (decision.action === 'show' ? decision.releases : releaseNotes.slice(0, 1)), asked: false, updated: info.version }));
      }
      if (after === 'failed') setWhatsNew({ releases: [], asked: false, failed: { version: installing, current: info.version } });
    }, () => {});
  }, []);
  // Send feedback: the screenshot is the window as it was, so it is taken before the dialog opens
  // (after the menu that asked has closed).
  const openFeedback = async () => {
    if (!host?.captureWindow) { setFeedback({}); return; }
    await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    try { setFeedback({ png: await host.captureWindow() }); } catch (error) { setFeedback({ error: String(error instanceof Error ? error.message : error) }); }
  };
  const openUpdates = () => { onOpenSettings('about'); void checkForUpdate(host?.app); };
  useEffect(() => {
    const listener = (event: Event) => {
      const request = (event as CustomEvent<OnboardingRequest>).detail;
      if (request === 'welcome') setWelcome(true);
      if (request === 'whats-new') setWhatsNew({ releases: releaseNotes, asked: true });
      if (request === 'docs') setDocs('README.md');
      if (request === 'feedback') void openFeedback();
      if (request === 'updates') openUpdates();
      // The shortcuts overlay lists every key; its Customize leads to Settings.
      if (request === 'shortcuts' && !runAction('shortcuts.open')) onOpenSettings('keyboard');
    };
    const docsListener = (event: Event) => setDocs((event as CustomEvent<string>).detail || 'README.md');
    window.addEventListener(EVENT, listener);
    window.addEventListener(DOCS_EVENT, docsListener);
    return () => { window.removeEventListener(EVENT, listener); window.removeEventListener(DOCS_EVENT, docsListener); };
  });
  // Help in the command palette and the desktop menu bar.
  useActions({
    'help.welcome': () => setWelcome(true),
    'help.whats-new': () => setWhatsNew({ releases: releaseNotes, asked: true }),
    'help.docs': () => setDocs('README.md'),
    'help.feedback': () => void openFeedback(),
    'help.updates': openUpdates,
  });
  const closeWelcome = () => { write(viewState, WELCOME_PREFERENCE, 'seen'); setWelcome(false); };
  const setupSettings = (tab: SettingsTab) => {
    closeWelcome();
    if (settingsOpen !== undefined) setSetupInSettings(true);
    onOpenSettings(tab);
  };
  const dismissWhatsNew = () => { if (releaseNotes[0]) write(viewState, WHATS_NEW_PREFERENCE, releaseNotes[0].id); setWhatsNew(undefined); };
  return <>
    {!decided && <FirstRun collections={collections} collectionsLoaded={collectionsLoaded} onDecision={show => {
      setDecided(true);
      if (show) setWelcome(true); else write(viewState, WELCOME_PREFERENCE, 'existing');
    }}/>}
    {welcome && <Welcome viewState={viewState} collections={collections} onClose={closeWelcome}
      onRunners={() => setupSettings('runners')}
      onSubscription={() => setupSettings('subscriptions')}
      onClaude={() => { setupSettings('agents'); requestAnimationFrame(() => document.getElementById('claude-settings')?.scrollIntoView?.({ block: 'start' })); }}
      onTask={() => { closeWelcome(); requestAnimationFrame(() => invokeAction('task.new')); }}
      onWorkspace={onWorkspace ? id => { closeWelcome(); onWorkspace(id); } : undefined}/>}
    {whatsNew && <WhatsNew releases={whatsNew.releases} focus={whatsNew.asked} updated={whatsNew.updated} failed={whatsNew.failed} onDismiss={dismissWhatsNew} onUpdates={() => { setWhatsNew(undefined); openUpdates(); }}/>}
    {docs && <Suspense fallback={null}><DocsViewer key={docs} initial={docs} onClose={() => setDocs('')}/></Suspense>}
    {feedback && <Suspense fallback={null}><FeedbackDialog workspaceId={workspaceId} host={host} shot={feedback} sessionId={sessionId} onClose={() => setFeedback(undefined)}/></Suspense>}
  </>;
}

/** Waits for the Runner's answer before calling an install fresh: no workspace and no conversation. */
function FirstRun({ collections, collectionsLoaded, onDecision }: { collections: readonly Collection[]; collectionsLoaded: boolean; onDecision(show: boolean): void }) {
  const { info } = useRunner();
  const index = useConversationIndex();
  const decision = welcomeDecision({ seen: false, workspacesLoaded: collectionsLoaded, workspaces: collections.length, conversationsLoaded: !!info && (index.complete || index.unsupported), conversations: index.sessions.length });
  const sent = useRef(false);
  useEffect(() => { if (!sent.current && (decision === 'show' || decision === 'record')) { sent.current = true; onDecision(decision === 'show'); } }, [decision]);
  return null;
}

function Keys({ keys }: { keys: string[] }) {
  return <span className="onboarding-keys">{keys.map((key, i) => <kbd key={i}>{key}</kbd>)}</span>;
}

function Welcome({ viewState, collections, onClose, onRunners, onSubscription, onClaude, onTask, onWorkspace }: {
  viewState?: ViewStateStore; collections: readonly Collection[]; onClose(): void; onRunners(): void;
  onSubscription(): void; onClaude(): void; onTask(): void; onWorkspace?(id: string): void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const runner = useRunner();
  const display = useDisplayDetail(viewState);
  const [behavior, setBehavior] = useState(() => readDefaultBehavior(viewState));
  const example = BEHAVIOR_PROFILES.find(profile => profile.id === behavior)?.example;
  const [preferenceError, setPreferenceError] = useState('');
  const availableWorkspaces = collections.filter(collection => collection.state === 'ready');
  const lessons = useMemo(() => essentialKeys(readKeymap(viewState), shortcut('Mod+P'), binding => displayBinding(binding)), [viewState]);
  useEffect(() => { const node = dialog.current; if (node && !node.open) node.showModal(); node?.querySelector<HTMLElement>('[data-autofocus]')?.focus(); }, []);
  return <DraggableDialog ref={dialog} className="onboarding-welcome" aria-labelledby="onboarding-welcome-title" onCancel={event => { event.preventDefault(); onClose(); }}>
    <DialogHeader>
      <div className="onboarding-title"><span className="brand-mark" aria-hidden="true">l<span>↗</span></span><h2 id="onboarding-welcome-title">Welcome to Lazybox</h2></div>
      <button className="onboarding-close" aria-label="Close welcome" onClick={onClose}><Icon name="close" size={16}/></button>
    </DialogHeader>
    <p className="onboarding-lede">Agents work in workspaces: your repositories, each on its own branch. They run on this machine with your own subscription, and come back to you when they need you.</p>
    <ol className="onboarding-steps">
      <li>
        <span className="onboarding-step-icon" aria-hidden="true"><Icon name="processes" size={16}/></span>
        <div><h3>Connect a Runner</h3>
          <p role="status">{runner.error ? 'Disconnected. Open Runners to check the connection and retry.' : runner.info ? 'Runner connected and ready.' : 'Waiting for the Runner…'}</p>
        </div>
        <button type="button" onClick={onRunners}>Manage Runners</button>
      </li>
      <li>
        <span className="onboarding-step-icon" aria-hidden="true"><Icon name="card" size={16}/></span>
        <div><h3>Connect your agents</h3>
          <p><strong>Claude:</strong> add an Anthropic API key. It is the way Anthropic documents for apps built on its Agent SDK, like Lazybox; your own Claude Code sign-in also works, but Anthropic has not confirmed it for such apps. <strong>Codex:</strong> sign in with your ChatGPT plan.</p>
          <AgentReadiness/></div>
        <div className="onboarding-step-actions"><button className="primary" onClick={onClaude}>Add an API key</button><button onClick={onSubscription}>Sign in to Codex</button></div>
      </li>
      <li className="onboarding-choice">
        <span className="onboarding-step-icon" aria-hidden="true"><Icon name="sliders" size={16}/></span>
        <div><DisplayPresetPicker value={display.preset} onChange={preset => {
          try {
            display.save({ preset });
            writeActivityDefault(viewState, DISPLAY_PRESETS.find(choice => choice.id === preset)!.activity);
            setPreferenceError('');
          } catch { setPreferenceError('Could not save your display choice. Try again in Settings → Appearance.'); }
        }}/></div>
      </li>
      <li className="onboarding-choice">
        <span className="onboarding-step-icon" aria-hidden="true"><Icon name="agent" size={16}/></span>
        <div><label className="setup-choice">Agent personality
          <select value={behavior} onChange={event => {
            const next = event.target.value;
            try { saveDefaultBehavior(viewState, next); setBehavior(next); setPreferenceError(''); }
            catch { setPreferenceError('Could not save your personality choice. You can choose it when starting a conversation.'); }
          }}><BehaviorOptions/></select></label>
          <div className="display-preview" aria-label="Personality preview"><p><strong>You</strong> · Fix the failing test.</p><div><strong>Agent</strong> · {example}</div></div>
          <p>Change this when you start a task or conversation.</p>
        </div>
      </li>
      <li>
        <span className="onboarding-step-icon" aria-hidden="true"><Icon name="repository" size={16}/></span>
        <div><h3>Start a task</h3><p>Pick one or more repositories, an agent and what to do. Lazybox makes the task its workspace, on its own branches, never your checkout.</p></div>
        <div className="onboarding-step-actions"><button onClick={onTask} disabled={!runner.info || !!runner.error}>New task</button>
          {onWorkspace && availableWorkspaces.length > 0 && <label className="setup-choice">Open a workspace<select value="" onChange={event => { if (event.target.value) onWorkspace(event.target.value); }}>
            <option value="">Choose a workspace…</option>
            {availableWorkspaces.map(collection => <option value={collection.id} key={collection.id}>{collection.name}</option>)}
          </select></label>}
        </div>
      </li>
      <li className="onboarding-keys-step">
        <span className="onboarding-step-icon" aria-hidden="true"><Icon name="keyboard" size={16}/></span>
        <div>
          <h3>The five keys that matter</h3>
          <dl className="onboarding-key-list" aria-label="The five keys that matter">
            {lessons.map(lesson => <div key={lesson.label}>
              <dt>{lesson.keys.map((keys, i) => <span key={i}>{i > 0 && <span className="onboarding-or"> / </span>}<Keys keys={keys}/></span>)}</dt>
              <dd><strong>{lesson.label}</strong><small>{lesson.detail}</small></dd>
            </div>)}
          </dl>
        </div>
      </li>
    </ol>
    {preferenceError && <p role="alert">{preferenceError}</p>}
    <footer>
      <button type="button" className="onboarding-docs" onClick={() => openOnboarding('docs')}>Read the documentation <Icon name="notes" size={12}/></button>
      <small>Skip any optional step and return from Help.</small>
      <button className="primary" data-autofocus onClick={onClose}>Get started</button>
    </footer>
  </DraggableDialog>;
}

/**
 * What is ready on the Runner's computer, from the same check Ask Lazybox
 * uses: "Claude Code 2.1.311 installed · signed in", "Codex not installed".
 */
function AgentReadiness() {
  const { report, error, checking, refresh } = useProviderAvailability();
  if (error) return <p className="onboarding-provider-check">Could not check agent readiness. <button type="button" onClick={refresh} disabled={checking}>Retry check</button></p>;
  if (!report) return <p role="status" className="onboarding-provider-check">Checking agent readiness…</p>;
  return <ul className="onboarding-agents" aria-label="Agents on this computer">
    {report.providers.filter(p => !isAcp(p.provider) || p.installed).map(p => <li key={p.provider} className={p.state === 'ready' ? 'ready' : 'missing'}>
      <Icon name={p.state === 'ready' ? 'check' : p.installed ? 'alert' : 'skip'} size={12}/><span>{providerLine(p)}</span>
    </li>)}
  </ul>;
}

/**
 * Release notes in a corner card: it never takes focus from work unless asked
 * for from Help. Right after an update it says so ("Updated to 0.2.0"); after
 * an install that did not take, it says that instead, once.
 */
function WhatsNew({ releases, focus, updated, failed, onDismiss, onUpdates }: { releases: readonly Release[]; focus: boolean; updated?: string; failed?: { version: string; current: string }; onDismiss(): void; onUpdates(): void }) {
  const card = useRef<HTMLElement>(null);
  useEffect(() => { if (focus) card.current?.focus(); }, [focus, releases]);
  const [newest, ...older] = releases;
  if (failed) return <aside ref={card} className="whats-new is-failed" aria-labelledby="whats-new-title" tabIndex={-1}
    onKeyDown={event => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); onDismiss(); } }}>
    <header>
      <span className="whats-new-badge"><Icon name="alert" size={13}/><span id="whats-new-title">Update not installed</span></span>
      <button className="onboarding-close" aria-label="Dismiss" onClick={onDismiss}><Icon name="close" size={14}/></button>
    </header>
    <div className="whats-new-body"><p>The update to {failed.version} did not finish. Lazybox is still {failed.current}, and nothing else changed. Your agents kept running.</p></div>
    <footer><button onClick={onDismiss}>Not now</button><button className="primary" onClick={onUpdates}>Open Updates</button></footer>
  </aside>;
  if (!newest) return null;
  return <aside ref={card} className="whats-new" aria-labelledby="whats-new-title" tabIndex={-1}
    onKeyDown={event => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); onDismiss(); } }}>
    <header>
      <span className="whats-new-badge"><Icon name="sparkle" size={13}/><span id="whats-new-title">What's new</span></span>
      {updated && <span className="whats-new-updated" role="status">Updated to Lazybox {updated}</span>}
      <button className="onboarding-close" aria-label="Dismiss What's new" onClick={onDismiss}><Icon name="close" size={14}/></button>
    </header>
    <div className="whats-new-body">
      <ReleaseSection release={newest}/>
      {older.length > 0 && <details className="whats-new-older">
        <summary>{focus ? 'Earlier releases' : `${older.length} more since you last looked`}</summary>
        {older.map(release => <ReleaseSection key={release.id} release={release}/>)}
      </details>}
    </div>
    <footer><button className="primary" onClick={onDismiss}>Got it</button></footer>
  </aside>;
}

/**
 * One release's notes: its title and date, then each change, under its group's
 * heading when the release groups them (a big release, such as the first).
 * Also the install dialog's preview.
 */
export function ReleaseSection({ release }: { release: Release }) {
  const items = (list: readonly ReleaseNote[]) => <ul>{list.map((item, index) => <li key={index}><strong>{item.title}</strong><span>{item.body}</span></li>)}</ul>;
  return <section className="whats-new-release" aria-label={release.title}>
    <h3>{release.title}<small>{release.date}</small></h3>
    {releaseGroups(release.items).map(({ group, items: list }, index) => group
      ? <section key={index} className="whats-new-group" aria-label={group}><h4>{group}</h4>{items(list)}</section>
      : <div key={index}>{items(list)}</div>)}
  </section>;
}
