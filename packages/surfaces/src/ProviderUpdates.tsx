import { useEffect, useRef } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import type { ProviderStatus } from '@lazybox/protocol';
import { useProviderAvailability } from './useProviderAvailability';
import { SettingsGroup, SettingsRow } from './settings-parts';
import { showToast } from './toasts';
import { isNewer, parseVersion } from './update-model';
import { readPreference, writePreference } from './view-state';

const instructions: Record<string, string> = {
  claude: 'https://code.claude.com/docs/en/setup#update-claude-code',
  codex: 'https://learn.chatgpt.com/docs/codex/cli',
};

function updateAvailable(program: ProviderStatus) {
  return program.installed && !!parseVersion(program.version) && isNewer(program.latestVersion, program.version);
}

export function ProviderUpdateNotices({ store, onOpen }: { store?: ViewStateStore; onOpen(): void }) {
  const { report } = useProviderAvailability(true, true);
  const notified = useRef(new Map<string, string>());
  useEffect(() => {
    for (const program of report?.providers || []) {
      if (!updateAvailable(program)) continue;
      const key = `provider-updates.notified.${program.provider}`;
      const version = `${program.version} → ${program.latestVersion}`;
      if (notified.current.get(key) === version) continue;
      notified.current.set(key, version);
      try {
        if (readPreference(store, key) === version) continue;
        writePreference(store, key, version);
      } catch { /* The notice still works when preferences cannot be saved. */ }
      showToast({
        id: key,
        tone: 'info',
        title: `${program.title} update available`,
        body: version,
        timeout: 10_000,
        button: { label: 'Review update', run: onOpen },
      });
    }
  }, [report, store, onOpen]);
  return null;
}

export function ProviderUpdates({ onOpenAbout }: { onOpenAbout(): void }) {
  const { report, error, checking, refresh } = useProviderAvailability(true, true);
  return <SettingsGroup title="Agent updates" description="Lazybox checks official releases for the programs installed on this Runner. Choose when to update.">
    {report?.providers.filter(program => program.provider in instructions).map(program => {
      const update = updateAvailable(program);
      return <SettingsRow key={program.provider} label={program.title}
        description={update ? `Installed ${program.version} · New release ${program.latestVersion}`
          : program.installed ? [program.version && `Installed ${program.version}`, parseVersion(program.version) && program.latestVersion ? `Latest release ${program.latestVersion}` : 'Update status unavailable'].filter(Boolean).join(' · ') : 'Not installed'}>
        {update && (program.source === 'bundled'
          ? <button type="button" onClick={onOpenAbout}>Check Lazybox updates</button>
          : <a href={instructions[program.provider]} target="_blank" rel="noreferrer">Update instructions</a>)}
      </SettingsRow>;
    })}
    {error && <p role="alert" className="settings-error">{error}</p>}
    <SettingsRow label="Official releases"><button type="button" disabled={checking} onClick={() => void refresh()}>{checking ? 'Checking…' : 'Check versions'}</button></SettingsRow>
  </SettingsGroup>;
}
