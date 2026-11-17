import { useEffect, useRef, useState } from 'react';
import { Code, ConnectError } from '@lazybox/client-core';
import type { GitHubConnection, GitHubSignIn } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import './github.css';

type Props = { onConnectionChange?():void; openSignIn?(url: string): Promise<void>; copySignInCode?(code: string): Promise<void> };
const waiting = (flow?: GitHubSignIn) => flow?.state === 'awaiting_login';
// GitHub pre-fills a classic token's scopes and note from these query parameters.
const TOKEN_URL = 'https://github.com/settings/tokens/new?scopes=repo&description=Lazybox';
export function GitHubSettings({ openSignIn, copySignInCode, onConnectionChange }: Props) {
  const { api } = useRunner();
  const [connection, setConnection] = useState<GitHubConnection>();
  const [flow, setFlow] = useState<GitHubSignIn>();
  const [error, setError] = useState('');
  const [unsupported, setUnsupported] = useState(false);
  const [busy, setBusy] = useState('');
  const busyRef = useRef(false);
  const requestKey = useRef('');
  const [copied, setCopied] = useState(false);
  const [disconnecting, setDisconnecting] = useState(false);
  const [personalToken, setPersonalToken] = useState('');
  const personal = connection?.authMethod === 'personal_token';
  const lifetime = useRef<AbortController | null>(null);
  useEffect(() => { const control = new AbortController(); lifetime.current = control; return () => control.abort(); }, []);
  const readConnection = async (signal?: AbortSignal) => {
    const state = await api.getGitHubConnection({}, { signal });
    if (signal?.aborted) return;
    setConnection(state); setFlow(state.pendingSignIn);
    return state;
  };
  useEffect(() => {
    const control = new AbortController();
    void readConnection(control.signal).catch(error => {
      if (control.signal.aborted) return;
      if (ConnectError.from(error).code === Code.Unimplemented) setUnsupported(true);
      else setError(String(error));
    });
    return () => control.abort();
  }, [api]);
  useEffect(() => {
    if (!flow?.id || !waiting(flow)) return;
    const control = new AbortController(); let pending = false;
    const poll = async () => {
      if (pending) return; pending = true;
      try {
        const next = await api.getGitHubSignIn({ id: flow.id }, { signal: control.signal });
        if (control.signal.aborted) return;
        if (next.state === 'connected') {
          // Keep the polling effect alive until the connected identity has loaded.
          // Changing flow state first would abort this request in effect cleanup.
          await readConnection(control.signal); onConnectionChange?.(); requestKey.current = '';
        } else setFlow(next);
      } catch (error) { if (!control.signal.aborted) setError(String(error)); }
      finally { pending = false; }
    };
    const timer = setInterval(() => void poll(), 1500); void poll();
    return () => { control.abort(); clearInterval(timer); };
  }, [api, flow?.id, flow?.state]);
  const run = async (name: string, action: (signal: AbortSignal) => Promise<void>) => {
    if (busyRef.current) return; busyRef.current = true; setBusy(name); setError('');
    const signal = lifetime.current!.signal;
    try { await action(signal); } catch (error) { if (!signal.aborted) setError(String(error)); }
    finally { busyRef.current = false; if (!signal.aborted) setBusy(''); }
  };
  const open = async (url: string) => {
    const parsed = new URL(url);
    if (parsed.protocol !== 'https:' || parsed.hostname !== 'github.com' || parsed.username || parsed.password || parsed.port) throw new Error('Expected an official GitHub URL');
    if (openSignIn) await openSignIn(url); else window.open(url, '_blank', 'noopener,noreferrer');
  };
  const begin = () => run('Connecting', async signal => {
    requestKey.current ||= crypto.randomUUID();
    const next = await api.beginGitHubSignIn({ requestKey: requestKey.current }, { signal });
    if (signal.aborted) return; setFlow(next); setCopied(false);
    if (next.signInUrl) await open(next.signInUrl);
  });
  if (unsupported) return <p className="settings-empty">This Runner needs an update to support GitHub connections. Your running sessions are unchanged.</p>;
  return <section className="github-settings" aria-label="GitHub connection">
    <div className="subscriptions-heading"><div><h2>GitHub</h2><p>Manage the connection used to discover and prepare repositories.</p></div>{connection?.state === 'connected' && <span className="github-connected">Connected as {connection.login}</span>}</div>
    {connection?.state === 'connected' && <p>{personal ? 'Connected with a personal token.' : 'GitHub App access: only repositories allowed by its installations. This can be less than your personal GitHub access.'}</p>}
    {connection?.state==='connected'&&!personal&&!disconnecting&&<button disabled={!!busy} onClick={()=>setDisconnecting(true)}>Use personal token…</button>}
    {error && <p role="alert" className="settings-error">{error}<button onClick={() => { setError(''); void run('Refreshing', async signal => { await readConnection(signal); }); }}>Retry</button></p>}
    {!connection && !error && <p>Loading GitHub connection…</p>}
    {connection && <>
      {connection.error && <p className="settings-error" role="alert">{connection.error}</p>}
      {connection.state !== 'connected' && !waiting(flow) && (personal || !connection.authMethod && connection.state !== 'needs_attention') && <div className="github-setup">
        <h3>Connect with a personal token</h3>
        <p>Sees every repository your GitHub account can see, including organization repositories. No GitHub App installation is needed.</p>
        <ol className="github-token-steps">
          <li><button disabled={!!busy} onClick={() => void run('Opening GitHub', () => open(TOKEN_URL))}>Create a token on GitHub ↗</button> This opens a classic token with the <code>repo</code> scope and a name already filled in. Pick an expiration, then click <strong>Generate token</strong> at the bottom.</li>
          <li>Copy the token (it starts with <code>ghp_</code>). GitHub shows it only once.</li>
          <li>If your organization uses single sign-on, open <button disabled={!!busy} onClick={() => void run('Opening GitHub', () => open('https://github.com/settings/tokens'))}>your tokens ↗</button>, click <strong>Configure SSO</strong> next to the new token and <strong>Authorize</strong> each organization. Without this, that organization’s repositories stay hidden.</li>
          <li>Paste it below.</li>
        </ol>
        <p>Already signed in with the GitHub CLI? Run <code>gh auth token</code> in a terminal and paste the output. It already has your organization and SSO access. Signing out of the CLI invalidates it.</p>
        <form onSubmit={event => { event.preventDefault(); const token = personalToken.trim(); setPersonalToken(''); void run('Checking token', async signal => {
          const state = await api.connectGitHubPersonalToken({ token }, { signal });
          if (!signal.aborted) { setConnection(state); setFlow(undefined); onConnectionChange?.(); }
        }); }}>
          <label>Personal access token<input type="password" autoComplete="off" spellCheck={false} value={personalToken} disabled={!!busy} onChange={event => setPersonalToken(event.target.value)} /></label>
          <button className="primary" disabled={!!busy || !personalToken.trim()} type="submit">Connect with token</button>
        </form>
        <details><summary>Why a classic token?</summary><p>The <code>repo</code> scope is the only GitHub token that reaches private repositories across several organizations. It also permits writes, but Lazybox only reads and clones. A fine-grained token is limited to one owner per token and to the repositories you select. It needs read access to Contents, Issues and Pull requests, and the organization may have to approve it.</p></details>
        <p>Saved privately on this Runner. Your system Git, GitHub CLI and agent sign-ins stay unchanged. Tokens are cleared from this form after submission.</p>
      </div>}
      {connection.state !== 'connected' && !waiting(flow) && !personal && connection.clientId && <button disabled={!!busy} onClick={() => void begin()}>Connect with GitHub App</button>}
      {waiting(flow) && <div className="github-sign-in"><h3>Finish signing in on GitHub</h3><p>Enter this code in your browser:</p><div className="device-code-control"><input aria-label="GitHub sign-in code" readOnly value={flow!.userCode} onFocus={event => event.currentTarget.select()}/><button disabled={!!busy} onClick={() => void run('Copying code', async () => { if (copySignInCode) await copySignInCode(flow!.userCode); else await navigator.clipboard.writeText(flow!.userCode); setCopied(true); })}>{copied ? 'Copied' : 'Copy code'}</button></div><div className="github-actions"><button onClick={() => void run('Opening GitHub', () => open(flow!.signInUrl))}>Open GitHub sign-in ↗</button><button disabled={!!busy} onClick={() => void run('Cancelling', async signal => { const result = await api.cancelGitHubSignIn({ id: flow!.id }, { signal }); if (!signal.aborted) { setFlow(result); requestKey.current = ''; } })}>Cancel sign-in</button></div><p role="status">Waiting for authorization…</p></div>}
      {flow && !waiting(flow) && flow.state !== 'connected' && <p role="status">{flow.error || `Sign-in ${flow.state}`}<button onClick={() => { requestKey.current = ''; setFlow(undefined); }}>Dismiss</button></p>}
      <div className="github-actions">{!personal && connection.appSlug && <button disabled={!!busy} onClick={() => void run('Opening GitHub', () => open(`https://github.com/apps/${connection.appSlug}/installations/new`))}>Manage repository access ↗</button>}{['connected', 'needs_attention'].includes(connection.state) && <button disabled={!!busy} onClick={() => setDisconnecting(true)}>Disconnect</button>}</div>
      {disconnecting && <div className="github-confirm"><p>Disconnect this Runner to connect with a personal token or another account. Imported repositories and conversations stay here. You can revoke the token or App authorization separately in your GitHub settings.</p><button onClick={() => void run('Disconnecting', async signal => { const state = await api.disconnectGitHub({}, { signal }); if (!signal.aborted) { setConnection(state); setFlow(undefined); setDisconnecting(false); onConnectionChange?.(); } })}>Disconnect this Runner</button><button onClick={() => setDisconnecting(false)}>Keep connected</button></div>}
    </>}
    {busy && <p role="status">{busy}…</p>}
  </section>;
}
