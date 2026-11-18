import { useEffect, useMemo, useRef, useState } from 'react';
import type { Profile, Session } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { useRunner } from './RunnerContext';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { readPreference, writePreference } from './view-state';
import { useRunnerSettings } from './RunnerSettings';
import { backWhen, bestChoice, limitHeadline, resetTime, sessionLimit, subscriptionChoices, subscriptionName, switchesItself, switchLabel, type SubscriptionChoice } from './subscription-model';
import { groupByProvider } from './subscription-groups';
import { ProviderGroups } from './SubscriptionGroups';
import './subscription-switch.css';

const askedKey = 'auto-switch-asked';
const waitKey = (session: string) => `subscription-wait:${session}`;
const providerName = (provider: string) => provider === 'codex' ? 'Codex' : 'Claude';

type Props = {
  session: Session;
  profiles: readonly Profile[];
  viewState?: ViewStateStore;
  /** The ⋯ menu's "Switch subscription…" dialog. */
  dialogOpen: boolean;
  onCloseDialog(): void;
  onSession(session: Session): void;
  onError(message: string): void;
};

/**
 * Everything a conversation shows about its subscription: one card when a
 * limit stops it, and the "Switch subscription…" dialog. Switching is the
 * person's choice and continues the same conversation; a message sent
 * meanwhile is delivered once, after it. Switching by itself at a limit is an
 * extension's account policy (EXTENSIONS.md): only when one covers the
 * provider do "Always switch automatically" and its one-time question show.
 */
export function SubscriptionControls({ session, profiles, viewState, dialogOpen, onCloseDialog, onSession, onError }: Props) {
  const { api, refresh } = useRunner();
  const [askEverywhere, setAskEverywhere] = useState(false);
  const provider = session.provider || profiles.find(p => p.id === session.profileId)?.provider || '';
  const { settings } = useRunnerSettings();
  const automatic = !!settings?.automaticSwitchingProviders.includes(provider);
  const limit = sessionLimit(session);
  const switchTo = async (profileId: string, always: boolean) => {
    const result = await api.switchSubscription({ sessionId: session.id, profileId, enableRotation: always });
    if (result.session) onSession(result.session);
    refresh();
    if (always && automatic && profileId !== session.profileId && !readPreference(viewState, askedKey)) setAskEverywhere(true);
  };
  useEffect(() => setAskEverywhere(false), [session.id]);
  return <>
    {limit ? <LimitCard key={session.id} session={session} profiles={profiles} provider={provider} automatic={automatic} viewState={viewState} switchTo={switchTo} onError={onError}/>
      : askEverywhere ? <EverywherePrompt provider={provider} onAnswer={async everywhere => {
        try { writePreference(viewState, askedKey, '1'); } catch { /* asked again next time */ }
        setAskEverywhere(false);
        if (!everywhere) return;
        try { const result = await api.setSessionRotation({ sessionId: session.id, enabled: true, allSessions: true }); if (result.session) onSession(result.session); refresh(); }
        catch (error) { onError(String(error)); }
      }}/> : null}
    {dialogOpen && <SwitchDialog session={session} profiles={profiles} provider={provider} automatic={automatic} onClose={onCloseDialog} onSession={onSession} switchTo={switchTo}/>}
  </>;
}

/**
 * Subscriptions to choose from, in the one order (subscription-groups.ts): by
 * provider under a small header when there is more than one, the current one
 * first. Arrow keys move through every choice, across groups.
 */
function ChoiceList({ choices, value, onChange, name, disabled }: { choices: SubscriptionChoice[]; value: string; onChange(id: string): void; name: string; disabled?: boolean }) {
  return <div className="subscription-choices" role="radiogroup" aria-label="Subscriptions">
    <ProviderGroups groups={groupByProvider(choices, choice => choice.provider)} className="subscription-choice-group">{items => items.map(choice => <label key={choice.id} className="subscription-choice" data-ready={choice.ready} data-current={choice.current || undefined}>
      <input type="radio" name={name} value={choice.id} checked={value === choice.id} disabled={disabled || choice.current} onChange={() => onChange(choice.id)}/>
      <span className="subscription-choice-name">{choice.name}{choice.current && <small> · current</small>}</span>
      <small className="subscription-choice-detail">{choice.detail}</small>
    </label>)}</ProviderGroups>
  </div>;
}

function LimitCard({ session, profiles, provider, automatic, viewState, switchTo, onError }: { session: Session; profiles: readonly Profile[]; provider: string; automatic: boolean; viewState?: ViewStateStore; switchTo(id: string, always: boolean): Promise<void>; onError(message: string): void }) {
  const limit = sessionLimit(session) as Exclude<ReturnType<typeof sessionLimit>, ''>;
  const [now, setNow] = useState(Date.now);
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 15000); return () => clearInterval(timer); }, []);
  const choices = useMemo(() => subscriptionChoices(profiles, provider, session.profileId, now), [profiles, provider, session.profileId, now]);
  const others = choices.filter(c => !c.current);
  const best = bestChoice(choices);
  const [choice, setChoice] = useState('');
  const selected = others.find(c => c.id === choice) || best || others[0];
  // A pool only switches while an account policy covers the provider.
  const rotating = switchesItself(session, automatic ? [provider] : [], provider);
  const [always, setAlways] = useState(true);
  const [busy, setBusy] = useState('');
  const current = subscriptionName(profiles.find(p => p.id === session.profileId), session.profileId);
  const resetAt = Number(session.account?.resetAt || 0n) * 1000;
  const reset = resetTime(session.account?.resetAt, now);
  const waitable = (limit === 'exhausted' || limit === 'throttled') && !!reset;
  const [waiting, setWaiting] = useState(() => { try { return Number(readPreference(viewState, waitKey(session.id)) || 0) === resetAt && resetAt > 0; } catch { return false; } });
  const act = async (label: string, work: () => Promise<void>) => {
    if (busy) return;
    setBusy(label); onError('');
    try { await work(); } catch (error) { onError(String(error)); } finally { setBusy(''); }
  };
  const forgetWait = () => { try { writePreference(viewState, waitKey(session.id), ''); } catch { /* nothing kept */ } };
  // Waiting resumes on its own at the reset while the conversation is open (or
  // the next time it is opened): the same request continues, once.
  const resumed = useRef(false);
  useEffect(() => {
    if (!waiting || resumed.current || !resetAt) return;
    const resume = () => { if (resumed.current) return; resumed.current = true; forgetWait(); void act('resume', () => switchTo(session.profileId, false)).finally(() => setWaiting(false)); };
    const delay = resetAt + 2000 - Date.now();
    if (delay <= 0) { resume(); return; }
    const timer = setTimeout(resume, Math.min(delay, 2 ** 31 - 1));
    return () => clearTimeout(timer);
  }, [waiting, resetAt]);
  const headline = limitHeadline(current, limit, session.account?.resetAt, now);
  const continueWith = selected && <button type="button" className="primary" disabled={!!busy} onClick={() => void act('switch', async () => { forgetWait(); await switchTo(selected.id, automatic && always && !rotating); })}>{busy === 'switch' ? `Switching to ${selected.name}…` : switchLabel(selected.name)}</button>;
  const queued = session.queued > 0 && <p className="limit-card-note">Your {session.queued === 1 ? 'message is' : `${session.queued} messages are`} saved and will be sent once, when the conversation continues.</p>;
  if (waiting) return <section className="limit-card is-waiting" aria-label="Subscription limit" data-limit={limit}>
    <p className="limit-card-line"><strong>Waiting for {current}: {backWhen(session.account?.resetAt, now)}.</strong> The conversation continues on its own then.</p>
    {queued}
    <div className="limit-card-actions">{continueWith}<button type="button" disabled={!!busy} onClick={() => { forgetWait(); setWaiting(false); }}>Don’t wait</button></div>
  </section>;
  return <section className="limit-card" aria-label="Subscription limit" data-limit={limit}>
    <h3>{headline}</h3>
    {others.length ? <p>{rotating && !best ? `Every other ${providerName(provider)} subscription is unavailable right now. ` : ''}Continue this conversation on {others.length > 1 ? 'another subscription' : selected!.name}: nothing is lost, and the interrupted request picks up where it stopped.</p>
      : <p>No other {providerName(provider)} subscription is signed in. Add one in Settings → Subscriptions to continue right away{waitable ? ', or wait for the reset' : ''}.</p>}
    {others.length > 1 ? <ChoiceList choices={others} value={selected?.id || ''} onChange={setChoice} name={`limit-${session.id}`} disabled={!!busy}/> : selected && <p className="limit-card-detail">{selected.name} · {selected.detail}</p>}
    {queued}
    <div className="limit-card-actions">
      {continueWith}
      {waitable ? <button type="button" disabled={!!busy} onClick={() => { try { writePreference(viewState, waitKey(session.id), String(resetAt)); } catch { /* waits while open */ } resumed.current = false; setWaiting(true); }}>Wait for reset</button>
        : limit === 'throttled' || limit === 'exhausted' ? <button type="button" disabled={!!busy} onClick={() => void act('retry', () => switchTo(session.profileId, false))}>{busy === 'retry' ? 'Trying again…' : `Try again on ${current}`}</button> : null}
    </div>
    {others.length > 0 && automatic && !rotating && <label className="limit-card-auto"><input type="checkbox" checked={always} disabled={!!busy} onChange={event => setAlways(event.target.checked)}/>Always switch automatically</label>}
  </section>;
}

function EverywherePrompt({ provider, onAnswer }: { provider: string; onAnswer(everywhere: boolean): void }) {
  return <section className="limit-card is-question" aria-label="Automatic switching">
    <p className="limit-card-line">This conversation now switches subscription by itself at a limit. Do the same for every {providerName(provider)} conversation?</p>
    <div className="limit-card-actions"><button type="button" className="primary" onClick={() => onAnswer(true)}>Every conversation</button><button type="button" onClick={() => onAnswer(false)}>Just this one</button></div>
  </section>;
}

function SwitchDialog({ session, profiles, provider, automatic, onClose, onSession, switchTo }: { session: Session; profiles: readonly Profile[]; provider: string; automatic: boolean; onClose(): void; onSession(session: Session): void; switchTo(id: string, always: boolean): Promise<void> }) {
  const { api, refresh } = useRunner();
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => { dialog.current?.showModal(); }, []);
  // Where the conversation is now comes first, then where it can go (subscriptionChoices).
  const choices = useMemo(() => subscriptionChoices(profiles, provider, session.profileId), [profiles, provider, session.profileId]);
  const others = choices.filter(c => !c.current);
  const [choice, setChoice] = useState(() => (bestChoice(choices) || others[0])?.id || '');
  const target = others.find(c => c.id === choice);
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  // Shown at once; put back if the Runner refuses.
  const [rotating, setRotating] = useState(switchesItself(session, automatic ? [provider] : [], provider));
  const toggle = async (enabled: boolean) => {
    setBusy(true); setError(''); setRotating(enabled);
    try { const result = await api.setSessionRotation({ sessionId: session.id, enabled }); if (result.session) onSession(result.session); refresh(); }
    catch (e) { setRotating(!enabled); setError(String(e)); } finally { setBusy(false); }
  };
  return <DraggableDialog ref={dialog} className="switch-subscription-dialog" aria-labelledby="switch-subscription-title" onCancel={event => { if (busy) event.preventDefault(); else onClose(); }}>
    <DialogHeader><h2 id="switch-subscription-title">Switch subscription</h2><button disabled={busy} aria-label="Close switch subscription" onClick={onClose}>×</button></DialogHeader>
    <p>The conversation continues on the subscription you choose, with everything so far. {session.busy ? 'The current turn moves with it.' : 'Your next message goes to the new subscription.'}</p>
    {choices.length > 1 ? <ChoiceList choices={choices} value={choice} onChange={setChoice} name={`switch-${session.id}`} disabled={busy}/> : <p className="limit-card-detail">No other {providerName(provider)} subscription is signed in. Add one in Settings → Subscriptions.</p>}
    {automatic ? <label className="limit-card-auto"><input type="checkbox" checked={rotating} disabled={busy || (!rotating && others.every(c => profiles.find(p => p.id === c.id)?.isolation !== 'environment'))} onChange={event => void toggle(event.target.checked)}/>Switch automatically when a subscription reaches its limit</label>
      : others.length > 0 && <p className="limit-card-note">Lazybox switches only when you ask. Switching by itself at a limit is an extension: Settings → Extensions.</p>}
    {error && <p className="settings-error" role="alert">{error}</p>}
    <footer><button disabled={busy} onClick={onClose}>Cancel</button><button className="primary" data-autofocus disabled={busy || !target} onClick={async () => {
      if (!target) return;
      setBusy(true); setError('');
      try { await switchTo(target.id, false); onClose(); } catch (e) { setError(String(e)); setBusy(false); }
    }}>{busy && target ? `Switching to ${target.name}…` : target ? switchLabel(target.name) : 'Switch'}</button></footer>
  </DraggableDialog>;
}
