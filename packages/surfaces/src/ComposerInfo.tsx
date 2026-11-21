import { PERMISSION_LABELS, type PermissionMode } from './slash-commands';
import { Icon } from './Icons';
import { agentModelText, modelName } from './conversation-model';
import { placeUsagePopover, useConversationContext } from './TelemetryPanel';
import { contextLevel, percentLabel } from './usage-model';
import './composer-info.css';

/**
 * The left of the message box's controls: who answers and how full their
 * context is. The words and the number are the chat header's (the same
 * model label, the same reading of the usage), so the two never disagree;
 * the agent opens the model picker as the header's chip does, the meter the
 * header's context popover. Narrow panes keep compact labels.
 */
export function ComposerInfo({ agentName, modelLabel, modelRequested, sessionID, onPickModel, onPickPermissions, permission, reportedPermission }: {
  permission?: string;
  reportedPermission?: string;
  agentName: string;
  /** The model as the header has it ("claude-opus-5-5", or "… (requested)"). */
  modelLabel: string;
  /** Asked for, not yet confirmed by the provider. */
  modelRequested: boolean;
  sessionID: string;
  /** The model picker (`/model`); without it the agent is plain text. */
  onPickModel?(): void;
  onPickPermissions?(): void;
}) {
  const context = useConversationContext(sessionID);
  const model = modelName(modelLabel);
  const text = agentModelText(agentName, modelLabel);
  const tooltip = `${modelLabel || agentName}${modelRequested ? ' · requested; the provider has not confirmed it yet' : ''}`;
  const inherited = !permission || permission === 'inherit';
  const mode = inherited ? 'CLI settings' : PERMISSION_LABELS[permission as PermissionMode] || permission;
  const native = reportedPermission === 'YOLO' ? 'Full access' : reportedPermission === 'workspaceWrite · on-request' ? 'Workspace access, asks as needed' : reportedPermission;
  const scope = inherited
    ? 'The agent uses its CLI settings. Lazybox-managed commands still follow Lazybox approval rules.'
    : 'This conversation’s setting applies to agent tools and ordinary Lazybox-managed commands.';
  const permissionHint = [scope, native && `${agentName} reported: ${native}`, onPickPermissions && 'Change permissions (/permissions)'].filter(Boolean).join('\n');
  const percent = context?.percent;
  const label = percent === undefined ? context?.tokensLabel : percentLabel(percent);
  const words = <><span className="composer-info-agent">{agentName}</span>{model && <span className="composer-info-model-name"> · {model}</span>}</>;
  return <div className="composer-info" role="group" aria-label="Agent and context">
    {onPickModel
      // tabIndex: WebKit (the desktop app) leaves buttons out of Tab unless they ask; this one is /model from the keyboard.
      ? <button type="button" tabIndex={0} className="composer-info-model" data-requested={modelRequested || undefined} title={`${tooltip}\nSwitch model (/model)`} aria-label={`${text}: change model`} onClick={onPickModel}>{words}<Icon name="chevron" size={11}/></button>
      : <span className="composer-info-model" data-requested={modelRequested || undefined} title={tooltip}>{words}</span>}
    {onPickPermissions
      ? <button type="button" tabIndex={0} className="composer-info-permission" title={permissionHint}
        aria-label={`Change permissions: ${mode}`} onClick={onPickPermissions}><Icon name="shield" size={13}/><span>{mode}</span><Icon name="chevron" size={10}/></button>
      : (permission || reportedPermission) && <span className="composer-info-permission" title={permissionHint}>{mode}</span>}
    {label && context && <button type="button" className="composer-info-context" data-level={percent === undefined ? undefined : contextLevel(percent)} popoverTarget={context.popover}
      title={`Context ${label} used · show context and cost`} aria-label={`Context ${label} used: show context and cost`}
      onClick={event => { const popover = document.getElementById(context.popover); if (popover) placeUsagePopover(popover, event.currentTarget.getBoundingClientRect(), 'below'); }}>
      {percent !== undefined && <span className="context-mini-bar composer-info-meter" aria-hidden="true" data-level={contextLevel(percent)}><i style={{ width: `${Math.max(0, Math.min(100, percent))}%` }}/></span>}
      <span className="composer-info-percent">{label}</span>
    </button>}
    {!label && <span className="composer-info-context" title="Waiting for the provider to report context usage">Context —</span>}
  </div>;
}
