// ACP agents for the fake Runner (runner-ui.mjs): what GetProviderAvailability
// says of them, and SaveAcpAgent / RemoveAcpAgent. Explicitly fake: nothing
// is probed or started. A test adds agents with `install(id)`; a conversation
// replays what the real ACP adapter journals (acp-journal.json, `replay`).
import { readFileSync } from 'node:fs';

const BUILTINS = [
  { id: 'gemini', title: 'Gemini CLI', icon: 'agent-gemini', command: 'gemini', args: ['--acp'], install: 'npm install -g @google/gemini-cli', homepage: 'https://github.com/google-gemini/gemini-cli' },
  { id: 'goose', title: 'Goose', icon: 'agent-goose', command: 'goose', args: ['acp'], install: 'brew install block-goose-cli', homepage: 'https://goose-docs.ai' },
  { id: 'opencode', title: 'OpenCode', icon: 'agent-opencode', command: 'opencode', args: ['acp'], install: 'npm install -g opencode-ai', homepage: 'https://opencode.ai' },
];

export function createAcpFake({ profiles, mutations }) {
  /** Installed built-ins, by id, with their version. */
  const installed = new Map();
  /** What a built-in's --help says of its launch flags, by id (the Runner's launch check). */
  const launchProblems = new Map();
  /** The person's agents (and changed built-ins), by id: { title, command, args, env }. */
  const custom = new Map();
  const agents = () => {
    const out = BUILTINS.map(b => custom.has(b.id) ? { ...b, ...custom.get(b.id), custom: true } : { ...b, custom: false });
    for (const [id, a] of custom) if (!BUILTINS.some(b => b.id === id)) out.push({ id, icon: 'agent', install: '', homepage: '', ...a, custom: true });
    return out;
  };
  const syncProfiles = () => {
    for (let i = profiles.length - 1; i >= 0; i--) if (profiles[i].provider.startsWith('acp:')) profiles.splice(i, 1);
    for (const a of agents()) if (a.custom || installed.has(a.id)) profiles.push({ id: `acp-${a.id}`, provider: `acp:${a.id}`, label: a.title, isolation: 'inherited' });
  };
  const providers = () => agents().map(a => {
    const present = a.custom || installed.has(a.id);
    const profile = profiles.find(p => p.provider === `acp:${a.id}`);
    return {
      provider: `acp:${a.id}`, title: a.title, state: present ? (profile ? 'ready' : 'no_subscription') : 'not_installed', installed: present,
      path: present ? `/fixture/bin/${a.command.split('/').pop()}` : '', version: installed.get(a.id) || '', source: a.custom ? 'configured' : 'path',
      error: present ? '' : `${a.title} is not installed on the Runner's computer (no ${a.command} on its PATH).`, installCommand: present ? '' : a.install,
      subscriptions: profile ? [{ profileId: profile.id, label: a.title, state: 'unverified', resetAt: '0', detail: 'Signs in with its own account', managed: false, isolation: 'inherited' }] : [],
      acp: { id: a.id, icon: a.icon, command: a.command, args: a.args, envNames: Object.keys(a.env || {}).sort(), homepage: a.homepage, custom: a.custom, launchProblem: present && !a.custom ? launchProblems.get(a.id) || '' : '' },
    };
  });
  return {
    providers,
    custom,
    /** A built-in agent found on the Runner's PATH; `launchProblem`, what its --help check found. */
    install(id, version, launchProblem = '') { installed.set(id, version); if (launchProblem) launchProblems.set(id, launchProblem); syncProfiles(); },
    /** RPCs this fake answers, or undefined. `report` is the whole availability report. */
    handle(method, body, report) {
      if (method === 'SaveAcpAgent') {
        mutations.push({ method, body });
        const id = body.id || String(body.title || '').toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
        if (!id || !body.command) return { error: { code: 'invalid_argument', message: 'the agent needs a name and the command that starts it' } };
        const env = { ...(custom.get(id)?.env || {}) };
        for (const key of Object.keys(env)) if (!(body.env || []).some(e => e.name === key)) delete env[key];
        for (const e of body.env || []) if (e.value) env[e.name] = e.value;
        custom.set(id, { title: body.title, command: body.command, args: body.args || [], env });
        syncProfiles();
        return { value: report() };
      }
      if (method === 'RemoveAcpAgent') {
        mutations.push({ method, body });
        if (!custom.delete(body.id)) return { error: { code: 'not_found', message: 'no such custom ACP agent' } };
        syncProfiles();
        return { value: report() };
      }
      return undefined;
    },
  };
}

const b64 = value => Buffer.from(JSON.stringify(value)).toString('base64');

/**
 * The real ACP adapter's journal for the fake agent's tour and a pending
 * permission (acp-journal.json, from TestACPJournalFixtureIsCurrent), as the
 * fake Runner's wire events. `root` replaces the workspace path.
 */
export function acpJournal(root = '/fixture/acp-workspace') {
  const golden = JSON.parse(readFileSync(new URL('./acp-journal.json', import.meta.url), 'utf8').replaceAll('/fixture/acp-workspace', root));
  const wire = ev => ({ type: ev.type, text: ev.text || '', status: ev.status || '', turnId: ev.turnId || '', requestId: ev.requestId || '', operationId: ev.operationId || '', agentId: ev.agentId || '', name: ev.name || '', sessionReference: ev.sessionId || '', ...(ev.data ? { providerDataJson: b64(ev.data) } : {}) });
  return { events: golden.events.map(wire), pending: golden.pending.map(wire) };
}
