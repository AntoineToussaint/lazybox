import { keySource, sharedCredentialGroups } from './providers';
import { SettingsGroup } from './settings-parts';
import { useProviderAvailability } from './useProviderAvailability';
import './acp-agents.css';

/**
 * Settings → Subscriptions: agents that share one account. Antigravity uses
 * Gemini CLI's API key when it has none of its own (ACP.md), so one key set
 * on Gemini CLI serves both. Names only: values never leave the Runner.
 */
export function SharedCredentials({ enabled }: { enabled: boolean }) {
  const { report } = useProviderAvailability(enabled);
  const groups = sharedCredentialGroups(report?.providers || []);
  return <>{groups.map(group => <SettingsGroup key={group.source.provider} title={group.title} className="shared-credentials"
    description={`${group.agents.map(a => a.title).join(' and ')} share one ${group.title} account: an API key set on ${group.source.title} (Settings → Agents) serves both. Google sign-ins are each agent’s own.`}>
    <ul className="acp-agent-list">
      {group.agents.map(a => <li key={a.provider} className="acp-agent shared-credential" aria-label={a.title}>
        <div className="acp-agent-text">
          <div className="acp-agent-name"><strong>{a.title}</strong>{!a.installed && !a.acp?.custom && <span className="acp-agent-state tone-muted">Not installed</span>}</div>
          <small className="shared-credential-key">{keySource(a, group.source)}</small>
          {a.acp?.signInCommand && <small>Sign in on the Runner: <code className="acp-agent-command">{a.acp.signInCommand}</code></small>}
          {a.acp?.credentialsNote && <small className="shared-credential-note">{a.acp.credentialsNote}</small>}
        </div>
      </li>)}
    </ul>
  </SettingsGroup>)}</>;
}
