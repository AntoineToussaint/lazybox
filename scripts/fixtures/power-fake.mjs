// An explicitly fake actions file, hook runs and CLI window requests
// (POWER.md) for the browser fixture. It parses no YAML and runs nothing: a
// test sets what the Runner would have loaded, and posts the requests the
// `lazybox` CLI would make.
const now = () => String(Date.now());

export function createPowerFake({ frame, commands, runtimeCommands, sessions, mutations }) {
  const file = { path: '/fixture/config/lazybox/actions.yaml', exists: true, error: '', stale: false, issues: [], actions: [], hooks: [], revision: '1', loadedAtUnixMillis: now(), schema: '{"title":"Lazybox actions and hooks"}', schemaPath: '/fixture/config/lazybox/actions.schema.json', events: ['agent.needs-input', 'turn.done', 'turn.failed', 'pr.opened', 'pr.checks-failed', 'pr.checks-passed', 'pr.changes-requested', 'pr.approved', 'pr.merged', 'pr.closed', 'pr.conflicts', 'mission.accepted'] };
  const runs = [], outputs = new Map(), watchers = [], requests = new Map();
  /** Commands started by a command step: their exit status by command line (default 0). */
  const commandExits = new Map();
  const send = (watcher, value) => watcher.res.write(frame(value));
  const all = () => [...commands, ...runtimeCommands];
  const fake = {
    file, runs, requests, commandExits,
    get windows() { return watchers.map(w => w.surface); },
    /** What the Runner reloads after a save: a new revision with these fields. */
    setFile(change) { Object.assign(file, change, { revision: String(Number(file.revision) + 1), loadedAtUnixMillis: now() }); },
    /** A CLI request (open, shot, action) delivered to the newest window, desktop first; resolves with the window's answer. */
    post(request) {
      const target = [...watchers].sort((a, b) => (a.surface === 'desktop') - (b.surface === 'desktop') || a.at - b.at).at(-1);
      if (!target) throw new Error('no Lazybox window is open to do this');
      const id = `ui-${requests.size + 1}`;
      let settle; const answered = new Promise(resolve => { settle = resolve; });
      requests.set(id, { request: { id, state: 'pending', createdAtUnixMillis: now(), ...request }, settle, answered });
      send(target, { id, state: 'pending', createdAtUnixMillis: now(), ...request });
      return answered;
    },
    async waitForWindow(timeout = 10000) { const end = Date.now() + timeout; while (!watchers.length && Date.now() < end) await new Promise(r => setTimeout(r, 50)); if (!watchers.length) throw new Error('no window registered'); },
    /** Returns true when it wrote the response itself, a {value}/{error} answer, or undefined for another fake. */
    handle(method, body, res) {
      switch (method) {
        case 'GetUserActions': return { value: file };
        case 'InitUserActions': mutations.push({ method, body }); if (!file.exists) fake.setFile({ exists: true, actions: [{ id: 'continue', title: 'Tell the agent to continue', icon: 'play', key: '', where: ['palette', 'menu'], confirm: false, steps: [{ kind: 'send', text: 'Continue.', to: 'current', when: 'always' }] }] }); return { value: file };
        case 'ListHookRuns': return { value: { runs: runs.slice().reverse().slice(0, body.limit || 50) } };
        case 'TestHook': {
          mutations.push({ method, body });
          const hook = file.hooks.find(h => h.id === body.hookId);
          if (!hook) return { error: { code: 'not_found', message: 'no hook with that id in the actions file' } };
          const id = `hook-run-${runs.length + 1}`, commandId = `hook-command-${runs.length + 1}`;
          outputs.set(commandId, hook.webhook ? `POST ${hook.webhook}\n→ 200 OK in 42ms\n` : `fixture hook ${hook.id} ran for ${hook.on[0]}\n`);
          const run = { id, hookId: hook.id, event: hook.on[0], state: 'succeeded', commandId, exitCode: hook.webhook ? undefined : 0, httpStatus: hook.webhook ? 200 : 0, summary: 'Alpha feature: test from Settings', collectionId: 'alpha', startedAtUnixMillis: String(Date.now() - 1200), endedAtUnixMillis: now(), grants: hook.grants || [], test: true };
          runs.push(run);
          return { value: run };
        }
        case 'ReadCommand': {
          const output = outputs.get(body.commandId) ?? all().find(c => c.id === body.commandId)?.output;
          if (output === undefined) return undefined;
          const data = Buffer.from(output).subarray(Number(body.offset || 0));
          return { value: { data: data.toString('base64'), nextOffset: String(Number(body.offset || 0) + data.length), status: { id: body.commandId, done: true } } };
        }
        case 'StartCommand': {
          mutations.push({ method, body });
          const session = sessions.find(s => s.id === body.sessionId);
          if (!session) return { error: { code: 'not_found', message: 'Missing conversation' } };
          // Like the Runner: StartCommand is the person's (owner only), so a read-only conversation runs it too; the folder stays inside the conversation's.
          const root = session.workingDirectory || '';
          if (body.cwd && root && body.cwd !== root && !body.cwd.startsWith(root + '/')) return { error: { code: 'unknown', message: "the command folder must be inside the conversation's workspace" } };
          const line = (body.argv || []).at(-1) || '';
          const exit = commandExits.get(line) ?? 0;
          // fake.holdCommands: the command keeps running until the test finishes it (`done`, `exitCode`).
          const text = `$ ${line}\n${fake.holdCommands ? '' : `exit ${exit}\n`}`;
          const command = { id: `step-command-${commands.length}`, sessionId: session.id, title: body.title, argv: body.argv, cwd: body.cwd || root, mode: 'managed', done: !fake.holdCommands, ...(fake.holdCommands ? {} : { exitCode: exit }), offset: String(Buffer.byteLength(text)), output: text };
          commands.push(command);
          const { output, ...wire } = command;
          return { value: wire };
        }
        case 'WaitCommand': return (async () => {
          const command = all().find(c => c.id === body.commandId);
          if (!command) return { error: { code: 'not_found', message: 'Missing command' } };
          const end = Date.now() + 20000;
          while (!command.done && Date.now() < end) await new Promise(r => setTimeout(r, 50));
          const { output, ...wire } = command;
          return { value: wire };
        })();
        case 'PostUiRequest': {
          try { const answered = fake.post(body); void answered; return { value: [...requests.values()].at(-1).request }; }
          catch (error) { return { error: { code: 'unavailable', message: error.message } }; }
        }
        case 'WaitUiRequest': return (async () => { const entry = requests.get(body.id); if (!entry) return { error: { code: 'failed_precondition', message: 'no such request' } }; await Promise.race([entry.answered, new Promise(r => setTimeout(r, 10000))]); return { value: entry.request }; })();
        case 'CompleteUiRequest': {
          const entry = requests.get(body.id);
          if (!entry) return { error: { code: 'failed_precondition', message: 'no such request' } };
          if (entry.request.state !== 'pending') return { error: { code: 'failed_precondition', message: 'this request was already answered' } };
          Object.assign(entry.request, { state: body.error ? 'failed' : 'done', error: body.error || '', detail: body.detail || '', png: body.png || '' });
          entry.settle(entry.request);
          return { value: entry.request };
        }
        case 'WatchUiRequests': {
          res.writeHead(200, { 'Content-Type': 'application/connect+json' }); res.flushHeaders();
          const watcher = { res, surface: body.surface === 'desktop' ? 'desktop' : 'web', at: Date.now() };
          watchers.push(watcher);
          res.on('close', () => { const i = watchers.indexOf(watcher); if (i >= 0) watchers.splice(i, 1); });
          send(watcher, {});
          return true;
        }
      }
      return undefined;
    },
  };
  return fake;
}
