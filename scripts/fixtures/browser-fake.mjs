// An explicitly fake agent browser (BROWSER.md) for the fake Runner: no
// Chromium, no network. A test drives it — states, actions, frames (JPEG bytes
// it supplies) and pending requests — and reads back what the UI asked:
// controls, input events, decisions and settings saves.

const now = () => String(Date.now());
const b64 = value => Buffer.from(JSON.stringify(value)).toString('base64');

export function createBrowserFake({ frame, sessions, mutations }) {
  const browsers = new Map();
  let seq = 0;
  const settings = { revision: '1', defaultPermission: 'ask', allowedSites: ['docs.python.org'], downloads: 'ask', showPanel: true, problem: '', availability: 'ready', installable: false, installCommandId: '',
    workspaces: [{ collectionId: 'alpha', name: 'Alpha feature', sites: ['github.com'], hasData: true, dataBytes: String(3_600_000), off: false }, { collectionId: 'beta', name: 'Beta release', sites: [], hasData: false, dataBytes: '0', off: false }] };
  // Whether Chromium is installed (Settings → Browser's "Install Chromium"); the test flips it.
  let chromium = true;
  const available = () => {
    const off = settings.defaultPermission === 'off';
    Object.assign(settings, off ? { availability: 'off', problem: 'The agent browser is off: agents get no browser.', installable: false }
      : !chromium ? { availability: 'not_installed', problem: 'Chromium for the agent browser isn’t installed.', installable: true }
      : { availability: 'ready', problem: '', installable: false });
    return settings;
  };
  const session = id => sessions.find(s => s.id === id);
  const entry = id => {
    if (!browsers.has(id)) {
      const s = session(id);
      browsers.set(id, { clients: new Set(), actions: [], frame: undefined, state: { conversationId: s?.conversationId || id, sessionId: id, collectionId: s?.collectionId || '', status: 'closed', controller: 'agent', url: '', title: '', loading: false, canGoBack: false, canGoForward: false, browseFreely: false, problem: '', viewportWidth: 1280, viewportHeight: 800, agentName: 'Claude', waiting: false, updatedAtUnixMillis: now() } });
    }
    return browsers.get(id);
  };
  const emit = (b, event) => { for (const res of b.clients) res.write(frame(event)); };
  // Session.browser mirrors the state while the browser is in use or stopped, as the Runner reports it.
  const mirror = (id, b) => { const s = session(id); if (!s) return; if (b.state.status === 'closed') delete s.browser; else s.browser = { ...b.state }; };
  const fake = {
    settings, browsers,
    /** Set by the fake Runner: a managed command whose output a test writes (runner-ui.mjs streamCommand). */
    streamCommand: undefined,
    /** The Chromium install the UI started, to write its output and finish it. */
    install: undefined,
    /** Chromium is (not) installed: Settings → Browser offers the install. */
    setChromium(installed) { chromium = installed; available(); },
    /** Every control, input and decision the UI sent, in order. */
    controls: [], inputs: [], decisions: [],
    state(id) { return entry(id).state; },
    /** Changes the browser's state, as the Runner would report it, to every watcher. */
    setState(id, change) { const b = entry(id); b.state = { ...b.state, ...change, updatedAtUnixMillis: now() }; mirror(id, b); emit(b, { state: b.state }); return b.state; },
    /** Adds an action (or updates one by seq) and tells every watcher. */
    act(id, action) {
      const b = entry(id);
      const value = { seq: String(action.seq ?? ++seq), actor: 'agent', status: 'done', atUnixMillis: now(), url: b.state.url, ...action };
      if (action.seq !== undefined) seq = Math.max(seq, Number(action.seq));
      const at = b.actions.findIndex(a => a.seq === value.seq);
      if (at >= 0) b.actions[at] = value; else b.actions.push(value);
      emit(b, { action: value });
      return value;
    },
    /** A screencast frame: JPEG bytes. */
    frame(id, jpeg, width = 1280, height = 800) { const b = entry(id); b.frame = { jpeg: Buffer.from(jpeg).toString('base64'), width, height, seq: String(++seq) }; emit(b, { frame: b.frame }); },
    /** A request the agent's browsing raised: a pending entry on the session, as the Runner lists it. */
    request(id, request) {
      const s = session(id), b = entry(id);
      const value = { id: request.id, sessionId: id, conversationId: s?.conversationId || id, collectionId: s?.collectionId || '', kind: 'site', agentName: 'Claude', permission: s?.permission || 'ask', createdAtUnixMillis: now(), ...request };
      const text = value.kind === 'download' ? `Download ${value.fileName}` : value.kind === 'sign_in' ? `Sign in on ${value.origin}` : `Open ${value.origin}`;
      s.pending = [...(s.pending || []).filter(p => p.requestId !== value.id), { type: 'browser_request', requestId: value.id, text, providerDataJson: b64(value) }];
      fake.setState(id, { waiting: true });
      return value;
    },
    handle(method, body, res) {
      switch (method) {
        case 'WatchBrowser': {
          const b = entry(body.sessionId);
          res.writeHead(200, { 'Content-Type': 'application/connect+json' }); res.flushHeaders();
          res.write(frame({ state: b.state, history: b.actions, ...(b.frame ? { frame: b.frame } : {}) }));
          b.clients.add(res); res.on('close', () => b.clients.delete(res));
          return true;
        }
        case 'ControlBrowser': {
          mutations.push({ method, body }); fake.controls.push(body);
          const b = entry(body.sessionId), s = b.state, agent = s.agentName || 'The agent';
          const person = s.status === 'open' && s.controller === 'person';
          switch (body.action) {
            case 'take_over':
              if (s.status !== 'open') return { error: { code: 'failed_precondition', message: 'No page is open to take over' } };
              fake.setState(body.sessionId, { controller: 'person' }); fake.act(body.sessionId, { kind: 'take_over', actor: 'person', summary: `You took over; ${agent} is paused` }); break;
            case 'give_back':
              if (!person) return { error: { code: 'failed_precondition', message: 'You do not hold the browser' } };
              fake.setState(body.sessionId, { controller: 'agent' }); fake.act(body.sessionId, { kind: 'give_back', actor: 'person', summary: `You gave the browser back to ${agent}` }); break;
            case 'stop': fake.setState(body.sessionId, { status: 'stopped', controller: 'agent', waiting: false, problem: '' }); fake.act(body.sessionId, { kind: 'stop', actor: 'person', summary: 'You stopped the browser' }); break;
            case 'resume': fake.setState(body.sessionId, { status: 'closed', problem: '' }); fake.act(body.sessionId, { kind: 'stop', actor: 'person', summary: `You let ${agent} browse again` }); break;
            case 'browse_freely': fake.setState(body.sessionId, { browseFreely: true }); break;
            case 'ask_sites': fake.setState(body.sessionId, { browseFreely: false }); break;
            case 'navigate': case 'back': case 'forward': case 'reload': {
              if (!person) return { error: { code: 'failed_precondition', message: 'Take over the browser first' } };
              const url = body.action === 'navigate' ? body.url : s.url;
              fake.setState(body.sessionId, { url, title: body.action === 'navigate' ? new URL(url).host : s.title, canGoBack: true });
              fake.act(body.sessionId, { kind: body.action, actor: 'person', summary: body.action === 'navigate' ? `You went to ${new URL(url).host}` : `You pressed ${body.action}`, url });
              break;
            }
            default: return { error: { code: 'invalid_argument', message: `unknown browser control ${body.action}` } };
          }
          return { value: b.state };
        }
        case 'SendBrowserInput': {
          const b = entry(body.sessionId);
          if (!(b.state.status === 'open' && b.state.controller === 'person')) return { error: { code: 'failed_precondition', message: 'Take over the browser first' } };
          fake.inputs.push(body); return { value: b.state };
        }
        case 'DecideBrowserRequest': {
          mutations.push({ method, body }); fake.decisions.push({ requestId: body.requestId, decision: body.decision, reason: body.reason || "" });
          const owner = sessions.find(s => (s.pending || []).some(p => p.requestId === body.requestId));
          if (!owner) return { error: { code: 'failed_precondition', message: 'This request was already answered' } };
          owner.pending = owner.pending.filter(p => p.requestId !== body.requestId);
          const state = fake.setState(owner.id, { waiting: owner.pending.some(p => p.type === 'browser_request') });
          return { value: state };
        }
        case 'GetBrowserSettings': return { value: available() };
        case 'InstallBrowser': {
          mutations.push({ method, body });
          if (fake.install && !fake.install.done) return { value: fake.install };
          if (chromium) return { error: { code: 'failed_precondition', message: 'Chromium for the agent browser is already installed' } };
          fake.install = fake.streamCommand('', 'Install Chromium for the agent browser', ['node', 'playwright-core/cli.js', 'install', '--only-shell', '--no-remove', 'chromium']);
          fake.install.origin = 'runner';
          settings.installCommandId = fake.install.id;
          const { output, live, write, finish, ...status } = fake.install;
          return { value: status };
        }
        // Waits for the install to end, as the Runner's WaitCommand does (other commands are the fake Runner's).
        case 'WaitCommand': {
          const command = fake.install;
          if (!command || command.id !== body.commandId) return undefined;
          const answer = () => { const { output, live, write, finish, ...status } = command; res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(status)); };
          const poll = setInterval(() => { if (command.done || res.destroyed) { clearInterval(poll); if (!res.destroyed) answer(); } }, 50);
          return true;
        }
        // "Give Claude the browser": the agent restarts with it, as the Runner's GiveBrowser does.
        case 'GiveBrowser': {
          mutations.push({ method, body });
          const s = session(body.sessionId);
          if (!s) return { error: { code: 'not_found', message: 'No such session' } };
          if (s.browserAccess !== 'can_give') return { error: { code: 'failed_precondition', message: 'Nothing to give' } };
          s.browserAccess = 'has';
          return { value: { session: s } };
        }
        case 'SaveBrowserSettings': {
          mutations.push({ method, body });
          if (body.revision !== settings.revision) return { error: { code: 'aborted', message: 'Browser settings changed' } };
          Object.assign(settings, { defaultPermission: body.defaultPermission, allowedSites: body.allowedSites || [], downloads: body.downloads, showPanel: !!body.showPanel, workspaces: (body.workspaces || []).map(w => ({ ...settings.workspaces.find(o => o.collectionId === w.collectionId), sites: w.sites || [], off: !!w.off })), revision: String(Number(settings.revision) + 1) });
          return { value: available() };
        }
        case 'ClearBrowserData': {
          mutations.push({ method, body });
          const w = settings.workspaces.find(w => w.collectionId === body.collectionId);
          if (!w) return { error: { code: 'not_found', message: 'No such workspace' } };
          Object.assign(w, { sites: [], hasData: false, dataBytes: '0' });
          for (const [id, b] of browsers) if (b.state.collectionId === body.collectionId && b.state.status !== 'closed') fake.setState(id, { status: 'closed', controller: 'agent', url: '', title: '' });
          settings.revision = String(Number(settings.revision) + 1);
          return { value: settings };
        }
      }
      return undefined;
    },
  };
  return fake;
}
