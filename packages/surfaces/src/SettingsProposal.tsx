import { useEffect, useState } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { RunnerSettings } from '@lazybox/protocol';

/** The help assistant's settings update tool, as Claude (MCP) and Codex (dynamic tool) name it. */
export const isSettingsUpdate = (tool: unknown) => tool === 'mcp__lazybox_settings__update_settings' || tool === 'lazybox_update_settings';

const fields: Record<string, { label: string; values?: Record<string, string> }> = {
  naming: { label: 'Suggested workspace names', values: { repositories: 'Repository names', generated: 'Memorable generated names' } },
  permission: { label: 'Default permissions', values: { '': 'Current CLI settings', ask: 'Ask for approval', 'read-only': 'Read only', yolo: 'YOLO · full access, no approval prompts' } },
  context: { label: 'Lazybox context for every new agent' },
  claudeContext: { label: 'Additional Claude context' },
  codexContext: { label: 'Additional Codex context' },
};

const shown = (key: string, value: unknown) => {
  if (typeof value !== 'string') return JSON.stringify(value);
  return fields[key]?.values?.[value] ?? (value || '(empty)');
};

/** A proposed change to Lazybox's settings, beside the values it replaces. Allowing it is the user's confirmation. */
export function SettingsProposal({ input, api }: { input: any; api: RunnerClient }) {
  const [current, setCurrent] = useState<RunnerSettings>();
  const [error, setError] = useState('');
  useEffect(() => {
    const control = new AbortController();
    api.getRunnerSettings({}, { signal: control.signal, timeoutMs: 5000 }).then(setCurrent, e => { if (!control.signal.aborted) setError(String(e)); });
    return () => control.abort();
  }, [api]);
  const changes: Record<string, unknown> = input?.changes && typeof input.changes === 'object' ? input.changes : {};
  const stale = current && input?.revision !== undefined && String(input.revision) !== String(current.revision);
  return <section className="settings-proposal" aria-label="Proposed settings change">
    {typeof input?.reason === 'string' && <p className="approval-description">{input.reason}</p>}
    <dl>{Object.entries(changes).map(([key, value]) => {
      const before = current ? (current as unknown as Record<string, unknown>)[key] : undefined;
      const long = !fields[key]?.values;
      return <div key={key} className={long ? 'long' : ''}>
        <dt>{fields[key]?.label ?? key}</dt>
        <dd>{current && <span className="before" aria-label="Current value">{long ? <pre>{shown(key, before)}</pre> : shown(key, before)}</span>}
          <span className="after" aria-label="Proposed value">{long ? <pre>{shown(key, value)}</pre> : shown(key, value)}</span></dd>
      </div>;
    })}</dl>
    {stale && <p role="alert">Settings changed since the assistant read them (revision {String(input.revision)}, now {String(current!.revision)}). Lazybox will refuse this change; decline it and ask the assistant to read the settings again.</p>}
    {error && <p role="alert">Could not load the current settings: {error}</p>}
    <small>Applies to new sessions. Existing and resumed conversations keep their saved settings.</small>
  </section>;
}
