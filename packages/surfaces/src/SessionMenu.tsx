import { useEffect, useRef, useState, type RefObject } from 'react';
import type { Session } from '@lazybox/protocol';
import { useConfirmCard } from './confirm-card';
import { ActionMenu } from './ActionMenu';
import { ActionMenuItems } from './ActionMenuItems';
import type { MenuItem } from './context-menu-model';
import { ACTIVITY_LABELS, type ActivityDetailState } from './activity-detail';
import { ACTIVITY_DETAILS } from './tool-lines';
import { permissionBlocked, shownModelChoices, type SlashChoice } from './slash-commands';
import { showMenuNotice } from './ContextMenu';
import { requestRename } from './ConversationTitle';
import { conversationHandoffItems, useHandoffMenus } from './handoff';
import { useRunner } from './RunnerContext';
import { streamLabel } from './human-error';
import { Icon } from './Icons';
import { agentTitle } from './providers';

/** The model picker: the same choices and switch as `/model` (useSlashCommands.tsx). */
export type ModelChoices = { choices: SlashChoice[]; efforts?: SlashChoice[]; live: boolean; disabled?: string; choose(value: string): void; /** Any model id: `/model ` in the message box. */ pick(): void };
const copySessionId = (id: string) => { void navigator.clipboard?.writeText(id).then(() => showMenuNotice('Session ID copied'), () => showMenuNotice('Could not copy the session ID')); };

export const PERMISSION_CHOICES: [string, string][] = [['ask', 'Ask for approval'], ['read-only', 'Read only'], ['yolo', 'YOLO']];

/** Ends a live session, or starts a new one from a saved conversation. */
export function SessionLifecycleButton({ session, allowStart, onNewSession, onClose }: { session?: Session; allowStart: boolean; onNewSession(): void; onClose(): void }) {
  if (session?.closed) return allowStart ? <button onClick={onNewSession}>New session</button> : <span>Saved conversation</span>;
  return <button disabled={!session} onClick={onClose}>End session</button>;
}

type SessionActions = {
  session?: Session;
  allowStart: boolean;
  onChangePermission(mode: string): void;
  /** For an agent that cannot change permissions live: ask to restart it in this mode. */
  onRestartWithPermission(mode: string): void;
  onSwitchSubscription(): void;
  /** Give the agent the browser: it restarts with it, the conversation kept (BROWSER.md). */
  onGiveBrowser?(): void;
  onNewSession(): void;
  onClose(): void;
  onDelete(): void;
  /** How much tool activity this conversation shows. */
  activity?: ActivityChoice;
  /** The models it can switch to. */
  models?: ModelChoices;
  sessionID?: string;
};
export type ActivityChoice = Pick<ActivityDetailState, 'detail' | 'fallback' | 'overridden' | 'choose'>;
const takesPermissions = (session?: Session): session is Session => !!session && !session.closed && !['help', 'planning', 'summary'].includes(session.purpose);
/** Choose a permission mode: the current one changes nothing, a blocked one says why, else live or with a restart. */
function choosePermission(session: Session, mode: string, live: (mode: string) => void, restart: (mode: string) => void) {
  const blocked = permissionBlocked(session.workflow, mode);
  if (blocked) { showMenuNotice(blocked); return; }
  if (session.permission === mode) return;
  if (session.capabilities?.livePermission) live(mode); else restart(mode);
}

/** What can be done to a conversation: the chat's ⋯ menu and its header's context menu, one list. */
export function sessionMenuItems({ session, allowStart, onChangePermission, onRestartWithPermission, onSwitchSubscription, onGiveBrowser, onNewSession, onClose, onDelete, activity, models, sessionID }: SessionActions): MenuItem[] {
  return [
    activity && { label: 'Activity detail', icon: 'activity', submenu: [
      ...ACTIVITY_DETAILS.map(detail => ({ label: ACTIVITY_LABELS[detail].label + (detail === activity.fallback ? ' (default)' : ''), checked: activity.detail === detail, run: () => activity.choose(detail) })),
    ] },
    // The current choice is checked, never disabled: disabled is for a choice that cannot be taken (YOLO in plan mode).
    takesPermissions(session) && { label: 'Permissions', icon: 'sliders', submenu: PERMISSION_CHOICES.map(([mode, label]) => ({
      label, checked: session.permission === mode, disabled: permissionBlocked(session.workflow, mode) || undefined,
      run: () => choosePermission(session, mode, onChangePermission, onRestartWithPermission),
    })) },
    models && session && !session.closed && { label: 'Model', icon: 'sparkle', disabled: models.disabled, submenu: [
      ...models.choices.map(choice => ({ label: choice.label + (models.live || choice.current ? '' : ' (restarts the agent)'), checked: !!choice.current, run: () => models.choose(choice.value) })),
      'separator', { label: 'Other model…', run: models.pick },
    ] },
    models && session && !session.closed && !!models.efforts?.length && { label: 'Reasoning effort', disabled: models.disabled, submenu:
      models.efforts.map(choice => ({ label: choice.effort || choice.label, checked: !!choice.current, run: () => models.choose(choice.value) })),
    },
    session && !session.closed && { label: 'Switch subscription…', run: onSwitchSubscription },
    session && !session.closed && onGiveBrowser && session.browserAccess === 'can_give' && { label: `Give ${agentTitle(session.provider, [], 'the agent')} the browser`, icon: 'globe', run: onGiveBrowser },
    session?.closed ? allowStart && { label: 'New session', run: onNewSession } : { label: 'End session', disabled: !session, run: onClose },
    'separator',
    session && sessionID && { label: 'Rename…', icon: 'pencil', run: () => { if (!requestRename(sessionID)) showMenuNotice('Open the conversation to rename it'); } },
    sessionID && { label: 'Copy session ID', icon: 'copy', run: () => copySessionId(sessionID) },
    { label: 'Delete conversation…', danger: true, disabled: !session, run: onDelete },
  ];
}

/** Handing the conversation to your own tools goes before Delete, which stays last. */
export function withHandoff(items: MenuItem[], handoff: MenuItem[]): MenuItem[] {
  if (!handoff.length) return items;
  const danger = items.findIndex(item => !!item && item !== 'separator' && !!item.danger);
  return danger < 0 ? [...items, 'separator', ...handoff] : [...items.slice(0, danger), ...handoff, ...items.slice(danger)];
}

/** The ⋯ menu of a chat: what it runs on, its permissions and subscription, and ending or deleting it. */
export function SessionMenu({ session, sessionID, accountName, stream, allowStart, onChangePermission, onRestartWithPermission, onSwitchSubscription, onGiveBrowser, onNewSession, onClose, onDelete, activity, models, extra = [] }: {
  session?: Session;
  /** Actions of the header that fold here in a slim panel (Review), listed first. */
  extra?: MenuItem[];
  sessionID: string;
  accountName: string;
  stream: string;
  allowStart: boolean;
  onChangePermission(mode: string): void;
  /** For an agent that cannot change permissions live: ask to restart it in this mode. */
  onRestartWithPermission(mode: string): void;
  onSwitchSubscription(): void;
  onGiveBrowser?(): void;
  onNewSession(): void;
  onClose(): void;
  onDelete(): void;
  activity?: ActivityChoice;
  models?: ModelChoices;
}) {
  const live = stream === 'Live';
  // Open in Terminal and Continue in Claude Code / Codex…, here and in the header's and rows' menus.
  useHandoffMenus();
  const { api, info } = useRunner();
  const handoff = conversationHandoffItems(api, session, info);
  const profile = session?.behaviorProfile || '';
  const behavior = profile.startsWith('balanced-') ? 'Balanced' : profile.startsWith('hands-on-') ? 'Hands-on' : profile === 'check-in-first-v1' ? 'Check in first' : profile;
  const gateway = session?.gatewayId && session.gatewayId !== 'direct' ? session.gatewayId : '';
  return <ActionMenu className="session-menu" label="Session actions">
    <strong>Session{live ? '' : <span className="session-connection" role="status"><span aria-hidden="true"/>{streamLabel(stream)}</span>}</strong>
    {/* Only facts worth reading: a subscription Lazybox can name, and a gateway other than the direct one. */}
    {(accountName || gateway || behavior) && <dl className="session-facts">
      {accountName && <div><dt>Subscription</dt><dd>{accountName}</dd></div>}
      {gateway && <div><dt>Gateway</dt><dd>{gateway}</dd></div>}
      {behavior && <div><dt>Behavior</dt><dd>{behavior}</dd></div>}
    </dl>}
    {takesPermissions(session) && <div className="session-permission" role="group" aria-label="Permissions">
      <strong>Permissions{session.permission ? '' : ' · CLI settings'}{session.capabilities?.livePermission ? '' : ' · restart needed'}</strong>
      {/* The current mode is selected (pressed), like Smart below: never greyed. Greyed is only a mode it cannot take, with why. */}
      {PERMISSION_CHOICES.map(([mode, label]) => {
        const current = session.permission === mode, blocked = permissionBlocked(session.workflow, mode);
        return <button key={mode} aria-pressed={current} aria-disabled={blocked ? true : undefined} data-blocked={blocked ? true : undefined}
          title={blocked || (current ? 'The current mode' : session.capabilities?.livePermission ? undefined : 'This agent started before live permission changes: it restarts in this mode and the conversation continues')}
          onClick={() => choosePermission(session, mode, onChangePermission, onRestartWithPermission)}>{label}</button>;
      })}
    </div>}
    {models && session && !session.closed && <>
      <ModelChoiceGroup models={models}/>
      {!!models.efforts?.length && <div className="session-permission" role="group" aria-label="Reasoning effort">
        <strong>Reasoning effort</strong>
        {models.efforts.map(choice => <button key={choice.value} type="button" aria-pressed={!!choice.current} disabled={!!models.disabled} onClick={() => models.choose(choice.value)}>{choice.effort}</button>)}
      </div>}
    </>}
    {activity && <ActivityDetailChoice activity={activity}/>}
    {session && !session.closed && session.permissionGrants.length > 0 && <AlwaysAllowed session={session}/>}
    {session?.closed && !allowStart && <span>Saved conversation</span>}
    <ActionMenuItems items={[...extra, 'separator', ...withHandoff(sessionMenuItems({ session, allowStart, onChangePermission, onRestartWithPermission, onSwitchSubscription, onGiveBrowser, onNewSession, onClose, onDelete, sessionID }).filter(item => !item || item === 'separator' || !item.submenu), handoff)]}/>
  </ActionMenu>;
}

/**
 * An agent whose worker predates live permission changes restarts in the new
 * mode between turns; the same provider conversation continues.
 */
export function PermissionRestartPrompt({ mode, busy, returnTo, onRestart, onCancel }: { mode: string; busy: boolean; /** The message box: the keyboard goes back there once answered. */ returnTo?: RefObject<HTMLElement | null>; onRestart(): void; onCancel(): void }) {
  // Enter restarts, Esc cancels: no pointer needed (`/permissions yolo` asks here when the agent restarts to switch).
  const { cardProps } = useConfirmCard(onCancel, returnTo, () => { if (!busy) onRestart(); });
  return <div className="restart-permission" role="alert" {...cardProps} aria-keyshortcuts="Enter Escape">
    <span>Switch to {PERMISSION_CHOICES.find(([choice]) => choice === mode)?.[1] || mode}? An older Lazybox started this conversation's agent, so switching restarts it and your conversation continues.</span>
    <button className="primary" disabled={busy} title={busy ? 'Wait for the current turn to finish' : undefined} onClick={onRestart}>Restart</button>
    <button onClick={onCancel}>Cancel</button>
  </div>;
}

/** A grant's label with its `code` spans: "commands starting with `npm run`". */
const grantLabel = (label: string) => label.split('`').map((part, i) => i % 2 ? <code key={i}>{part}</code> : part);

/**
 * The "always allow" choices made in this conversation, which its agent holds
 * also after a restart or a resume, each with Remove: the agent asks again.
 */
function AlwaysAllowed({ session }: { session: Session }) {
  const { api } = useRunner();
  const revoke = async (grantId: string, label: string) => {
    try { await api.apply({ sessionId: session.id, operationId: crypto.randomUUID(), operation: { case: 'revokeGrant', value: { grantId } } }); showMenuNotice(`Removed: the agent asks again for ${label.replaceAll('`', '')}`); }
    catch (error) { showMenuNotice(`Could not remove it: ${error instanceof Error ? error.message : String(error)}`); }
  };
  return <div className="session-permission session-grants" role="group" aria-label="Always allowed">
    <strong>Always allowed</strong>
    <ul>{session.permissionGrants.map(grant => <li key={grant.id} title="Allowed for this conversation, also after the agent restarts">
      <span>{grantLabel(grant.label)}</span>
      <button type="button" aria-label={`Remove always allow: ${grant.label.replaceAll('`', '')}`} title="Remove: the agent asks again" onClick={() => void revoke(grant.id, grant.label)}><Icon name="close" size={11}/></button>
    </li>)}</ul>
  </div>;
}

/**
 * The model picker of the ⋯ menu: the current model selected (the provider's
 * Default when the conversation names none), the first of the provider's
 * list, and "More models" for the rest, in place; "Other…" types any id.
 */
function ModelChoiceGroup({ models }: { models: ModelChoices }) {
  const [expanded, setExpanded] = useState(false);
  const { shown, more } = shownModelChoices(models.choices, expanded);
  // "More" goes away as it opens: the keyboard moves to the first model it revealed.
  const group = useRef<HTMLDivElement>(null);
  const revealed = useRef<string>('');
  useEffect(() => { if (expanded && revealed.current) group.current?.querySelector<HTMLButtonElement>(`button[data-model="${CSS.escape(revealed.current)}"]`)?.focus(); }, [expanded]);
  const expand = () => { revealed.current = models.choices.find(choice => !shown.includes(choice))?.value || ''; setExpanded(true); };
  const restart = models.live ? '' : 'Restarts the agent between turns; the conversation continues';
  return <div ref={group} className="session-permission session-model" role="group" aria-label="Model">
    <strong>Model{models.live ? '' : ' · restart needed'}</strong>
    {shown.map(choice => <button key={choice.value} data-model={choice.value} aria-pressed={!!choice.current} disabled={!!models.disabled}
      title={[choice.detail, choice.current ? 'The current model' : restart].filter(Boolean).join(' · ') || undefined}
      onClick={() => models.choose(choice.value)}>{choice.label}</button>)}
    {more > 0 && <button type="button" data-keep-open aria-expanded={false} title={`${more} more of this agent's models`} onClick={expand}>{`More (${more})`}</button>}
    <button disabled={!!models.disabled} title="Any model id, typed in the message box (/model)" onClick={models.pick}>Other…</button>
  </div>;
}

/** Quiet, Smart or Verbose for this conversation, in its ⋯ menu; the default is marked. */
function ActivityDetailChoice({ activity }: { activity: ActivityChoice }) {
  return <div className="session-permission session-activity" role="group" aria-label="Activity detail">
    <strong>Activity detail{activity.overridden ? ' · this conversation' : ' · default'}</strong>
    {ACTIVITY_DETAILS.map(detail => <button key={detail} aria-pressed={activity.detail === detail} title={ACTIVITY_LABELS[detail].detail + (detail === activity.fallback ? ' (The default for every conversation.)' : '')}
      onClick={() => activity.choose(detail)}>{ACTIVITY_LABELS[detail].label}</button>)}
  </div>;
}
