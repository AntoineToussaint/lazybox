import { useEffect, useState } from 'react';
import { useRunner } from './RunnerContext';
import { useRunnerSettings } from './RunnerSettings';
import { Icon } from './Icons';
import { accountNotices, shownNotices } from './claude-auth-model';
import { settingsError } from './settings-parts';
import './claude-auth.css';

/**
 * The one-time notices the Runner raised when automatic switching became an
 * extension and Claude sign-in in Lazybox a setting (CLAUDE_COMPLIANCE.md):
 * an existing setup kept working, and each says where to review it. Got it
 * dismisses it for good (the Runner keeps the dismissal, so every window and
 * device agrees).
 */
export function AccountNotices({ onNavigate }: { onNavigate(target: 'extensions' | 'agents' | 'disk-cleanup'): void }) {
  const { api, info, connection } = useRunner();
  const { settings, setSettings } = useRunnerSettings(true, 60_000);
  const [busy, setBusy] = useState(''), [error, setError] = useState('');
  const storageKey = 'lazybox:disk-warning:' + (info?.runnerId || connection.baseUrl);
  const [dismissed, setDismissed] = useState<{ key: string; id: string }>();
  const diskNotice = settings?.notices.find(id => id.startsWith('disk-pressure-')) || '';
  const loaded = !!settings;
  useEffect(() => {
    if (!loaded) return;
    try {
      const saved = sessionStorage.getItem(storageKey);
      if (saved && saved !== diskNotice) sessionStorage.removeItem(storageKey);
      setDismissed(saved && saved === diskNotice ? { key: storageKey, id: saved } : undefined);
    } catch { /* This window can still dismiss the warning in memory. */ }
  }, [storageKey, diskNotice, loaded]);
  const hideDiskNotice = (id: string) => {
    setDismissed({ key: storageKey, id });
    try { sessionStorage.setItem(storageKey, id); } catch { /* Kept for this view. */ }
  };
  const ids = shownNotices(settings?.notices).filter(id => dismissed?.key !== storageKey || dismissed.id !== id);
  if (!settings || !ids.length) return null;
  const dismiss = async (id: string) => {
    setBusy(id); setError('');
    try {
      setSettings(await api.saveRunnerSettings({ ...settings, notices: settings.notices.filter(n => n !== id) }, { timeoutMs: 10000 }));
      window.dispatchEvent(new Event('lazybox-settings-saved'));
    } catch (e) { setError(settingsError(e)); } finally { setBusy(''); }
  };
  return <>
    {ids.map(id => { const notice = accountNotices[id]; return <section key={id} className="account-notice" role="status" aria-label={notice.title}>
      <Icon name="bell" size={15}/>
      <div className="account-notice-body">
        <strong>{notice.title}</strong>
        <p>{notice.body}{notice.link && <> <a href={notice.link.href} target="_blank" rel="noreferrer">{notice.link.label}</a></>}</p>
        <div className="account-notice-actions">
          <button type="button" className="primary" onClick={() => onNavigate(notice.target)}>{notice.action}</button>
          {notice.target === 'disk-cleanup' ? <button type="button" onClick={() => hideDiskNotice(id)}>Dismiss</button> : <button type="button" disabled={busy === id} onClick={() => void dismiss(id)}>{busy === id ? 'Saving…' : 'Got it'}</button>}
        </div>
        {error && <p className="settings-error" role="alert">{error}</p>}
      </div>
    </section>; })}
  </>;
}
