import { useCallback, useEffect, useState, type FormEvent } from 'react';
import type { ClaudeAuth } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { useRunnerSettings } from './RunnerSettings';
import { useProviderAvailability } from './useProviderAvailability';
import { SettingsGroup, SettingsRow, settingsError } from './settings-parts';
import { Icon } from './Icons';
import { CLAUDE_TERMS_URL } from './extension-grants';
import { claudeSignInLine, CLAUDE_INSTALL_COMMAND, CLAUDE_KEYS_URL } from './claude-auth-model';
import './claude-auth.css';

/**
 * Settings → Agents & workspaces → Claude (CLAUDE_COMPLIANCE.md): how Claude
 * agents sign in, the Anthropic API key Anthropic asks apps built on its
 * Agent SDK to use, where Claude Code is (and Install Claude Code when it is
 * not), and Claude sign-in in Lazybox, off unless the person turns it on.
 */
export function ClaudeAuthSettings({ onRunInShell }: { onRunInShell?(command: string): Promise<void> }) {
  const { api } = useRunner();
  const { settings, setSettings } = useRunnerSettings();
  const { report, refresh, checking } = useProviderAvailability();
  const program = report?.providers.find(p => p.provider === 'claude');
  const [auth, setAuth] = useState<ClaudeAuth>();
  const [key, setKey] = useState('');
  const [busy, setBusy] = useState(''), [error, setError] = useState(''), [saved, setSaved] = useState('');
  const load = useCallback(async () => { try { setAuth(await api.getClaudeAuth({}, { timeoutMs: 5000 })); } catch (e) { setError(settingsError(e, 'load')); } }, [api]);
  useEffect(() => { void load(); }, [load]);
  const act = async (what: string, work: () => Promise<void>) => {
    if (busy) return;
    setBusy(what); setError(''); setSaved('');
    try { await work(); } catch (e) { setError(settingsError(e)); } finally { setBusy(''); }
  };
  const save = (event: FormEvent) => { event.preventDefault(); void act('key', async () => { setAuth(await api.setClaudeApiKey({ apiKey: key.trim(), clear: false }, { timeoutMs: 10000 })); setKey(''); setSaved('Saved. New Claude conversations use this key.'); }); };
  const forget = () => void act('forget', async () => { setAuth(await api.setClaudeApiKey({ apiKey: '', clear: true }, { timeoutMs: 10000 })); setSaved('Forgotten. New Claude conversations use your Claude Code sign-in.'); });
  const signIn = (on: boolean) => void act('sign-in', async () => {
    if (!settings) return;
    const request = { ...settings, claudeSubscriptionSignIn: on };
    setSettings(request);
    try { setSettings(await api.saveRunnerSettings(request, { timeoutMs: 10000 })); }
    finally { window.dispatchEvent(new Event('lazybox-settings-saved')); }
  });
  const install = () => void act('install', async () => {
    if (!onRunInShell) throw new Error(`Run this in a terminal: ${CLAUDE_INSTALL_COMMAND}`);
    await onRunInShell(CLAUDE_INSTALL_COMMAND);
  });
  const line = claudeSignInLine(auth);
  return <SettingsGroup id="claude-settings" className="claude-settings" title="Claude" description="How Claude agents sign in. Each conversation keeps the credential it started with.">
    <SettingsRow label="Claude agents sign in with" description={line.detail}>
      <span className={`claude-auth-mode tone-${line.tone}`} role="status"><Icon name={line.icon} size={13}/>{line.title}</span>
    </SettingsRow>
    <SettingsRow label="Use an Anthropic API key" htmlFor="claude-api-key" stacked description={<>Recommended. Anthropic asks apps built on its Agent SDK, like Lazybox, to use an API key; usage is billed to your Claude Console account. The key stays on this computer, in the Runner’s private folder, and is never shown again. <a href={CLAUDE_KEYS_URL} target="_blank" rel="noreferrer">Create a key</a></>}>
      {auth?.apiKeySet
        ? <div className="claude-key-saved"><span className="mono">sk-ant-…{auth.apiKeyHint}</span><button type="button" disabled={!!busy} onClick={forget}>{busy === 'forget' ? 'Forgetting…' : 'Forget key'}</button></div>
        : <form className="claude-key-form" onSubmit={save}>
          <input id="claude-api-key" type="password" autoComplete="off" spellCheck={false} placeholder="sk-ant-…" value={key} disabled={!!busy || !auth} onChange={event => setKey(event.target.value)}/>
          <button className="primary" disabled={!!busy || !key.trim()}>{busy === 'key' ? 'Saving…' : 'Save key'}</button>
        </form>}
    </SettingsRow>
    <SettingsRow label="Claude Code" description={program?.installed ? `${program.source === 'bundled' ? 'The copy that ships with Lazybox' : program.source === 'configured' ? 'Set in the Runner configuration' : 'Installed on this computer'}${program.path ? `: ${program.path}` : ''}.` : <>Lazybox runs Anthropic’s Claude Code, unmodified. Install it with Anthropic’s installer, in a shell you can watch: <code>{CLAUDE_INSTALL_COMMAND}</code></>}>
      {!report ? <small className="muted">Checking…</small>
        : program?.installed ? <span className="claude-program"><Icon name="check" size={13}/>{program.version || 'Installed'}</span>
        : <span className="claude-program-actions">
          <button type="button" className="primary" disabled={!!busy} onClick={install}>{busy === 'install' ? 'Opening a shell…' : 'Install Claude Code'}</button>
          <button type="button" disabled={checking} onClick={() => void refresh()}>{checking ? 'Checking…' : 'Check again'}</button>
        </span>}
    </SettingsRow>
    <SettingsRow label="Claude sign-in in Lazybox" htmlFor="claude-sign-in" description={<>Off: Lazybox offers no Claude sign-in, and Claude subscriptions saved in Settings → Subscriptions do not start. Anthropic does not allow apps built on its Agent SDK to offer claude.ai sign-in without its approval, so turning this on is at your own risk with your own accounts. <a href={CLAUDE_TERMS_URL} target="_blank" rel="noreferrer">Anthropic’s terms</a></>}>
      <span className="settings-switch"><input id="claude-sign-in" type="checkbox" role="switch" checked={!!settings?.claudeSubscriptionSignIn} disabled={!settings || !!busy} onChange={event => signIn(event.target.checked)}/></span>
    </SettingsRow>
    {settings?.claudeSubscriptionSignIn && <p className="settings-callout tone-warn" role="note"><Icon name="alert" size={14}/>{settings.claudeSignInCarriedOver ? 'Kept on so the Claude accounts you had saved keep working. ' : ''}Claude sign-in in Lazybox is on. Anthropic has not confirmed that this is allowed; an API key is the way it documents.</p>}
    {error && <p className="settings-error" role="alert">{error}</p>}
    {saved && <p className="settings-saved" role="status">{saved}</p>}
  </SettingsGroup>;
}
