// The mocked world a scenario plays in: workspaces, repositories,
// conversations and their history, issues, pull requests, files, missions.
// Builders write the fake Runner's state (scripts/fixtures/runner-ui.mjs)
// before the app opens; handles (a conversation, a pull request) then make
// the steps that change it while the video runs, as a Runner would: journal
// events, session state, tracked links. Nothing here runs Git, an agent or
// GitHub.
import type { EventChannel, FixtureCollection, FixtureSession, JournalEvent, RunnerFixture } from '../../scripts/fixtures/runner-ui.mjs';
import { step, type Step, type StepContext } from './step.ts';
import type { ScenarioClock } from './clock.ts';
import { fakeSpeech } from './voice.ts';

const b64 = (value: unknown) => Buffer.from(JSON.stringify(value)).toString('base64');
const slug = (value: string) => value.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
const hex = (seed: string) => Array.from({ length: 40 }, (_, i) => '0123456789abcdef'[(seed.charCodeAt(i % seed.length) * (i + 7) + i * 13) % 16]).join('');
const MIN = 60_000;

export type AgentKind = 'claude' | 'codex';
export type OpenTarget = WorkspaceHandle | ConversationHandle | 'start' | 'phone';

/** A GitHub issue the fake Runner can quote (Start from an issue). */
export interface IssueSpec {
  /** owner/name, e.g. `acme/api`. */
  repo: string;
  number: number;
  title: string;
  body?: string;
  author?: string;
  labels?: string[];
  assignees?: string[];
  comments?: { author: string; body: string; daysAgo?: number }[];
  /** How long ago it last changed, in hours. */
  updatedHoursAgo?: number;
}

export interface WorkspaceOptions {
  /** owner/name of each repository, e.g. `['acme/api', 'acme/web']`. */
  repos: string[];
  /** The branch every repository works on (default `lazybox/<workspace>`). */
  branch?: string;
}

export interface ConversationOptions {
  agent?: AgentKind;
  model?: string;
  /** Messages sent during a turn wait in the queue, with Send now (QueuedMessages). */
  queue?: boolean;
  /** What a turn stopped by Send now says first. */
  sentNowReply?: string;
  /** Its permission mode (default `ask`). */
  permission?: 'ask' | 'auto' | 'yolo' | 'read-only';
  /** Minutes since it was created (default 30). */
  createdMinutesAgo?: number;
}

export interface PullRequestSpec {
  /** owner/name (default: the workspace's first repository). */
  repo?: string;
  number: number;
  title: string;
  /** The repository's name in the workspace (default: the repo's name). */
  member?: string;
  /** Its conversation (the one that delivers it). */
  conversation: ConversationHandle;
  /** How many checks run (for "2 of 14"). */
  checks?: number;
}

export interface TaskSpec { key: string; title: string; brief?: string; role?: string; permission?: string; workspace?: string; paths?: string[]; estimateUsd?: number; after?: string[] }

// ---- History ----------------------------------------------------------------

/**
 * A conversation's past, written the way the Runner journals it. Each call
 * appends a record a few seconds after the previous one; the whole history
 * ends `endedMinutesAgo` before the scenario starts.
 */
export class History {
  readonly records: { event: JournalEvent; seconds: number }[] = [];
  private tools = 0;
  private turn = 0;
  constructor(private root: string) {}
  private add(event: JournalEvent, seconds = 2) { this.records.push({ event: { turnId: `h${this.turn}`, ...event }, seconds }); return this; }
  /** The person's message: a new turn. */
  prompt(text: string) { this.turn++; this.add({ type: 'operation_accepted', operationId: `history-${this.turn}`, text }, 20); return this.add({ type: 'turn_started' }, 1); }
  /** The agent's reply (Markdown). */
  reply(markdown: string) { return this.add({ type: 'message_delta', text: markdown }, 4); }
  /** Any tool call and its result. */
  tool(name: string, input: Record<string, unknown>, output: unknown = 'ok', status: 'completed' | 'failed' = 'completed') {
    const id = `history-tool-${++this.tools}`;
    this.add({ type: 'tool_activity', providerDataJson: b64({ type: 'tool_call', id, name, status: 'running', input }) }, 1);
    return this.add({ type: 'tool_activity', providerDataJson: b64({ type: 'tool_call', id, status, output }) }, 2);
  }
  bash(command: string, output = '', options: { description?: string; failed?: boolean } = {}) { return this.tool('Bash', { command, ...(options.description ? { description: options.description } : {}) }, output, options.failed ? 'failed' : 'completed'); }
  read(...paths: string[]) { for (const path of paths) this.tool('Read', { file_path: `${this.root}/${path}` }, 'line one\nline two\n'); return this; }
  edit(path: string, from = 'old', to = 'new') { return this.tool('Edit', { file_path: `${this.root}/${path}`, old_string: from, new_string: to }); }
  plan(todos: { content: string; status: 'pending' | 'in_progress' | 'completed' }[]) { return this.tool('TodoWrite', { todos }); }
  /** The turn ends. */
  end() { return this.add({ type: 'turn_ended', status: 'completed' }, 1); }
}

// ---- The world --------------------------------------------------------------

/** The fake Runner's state, built for one scenario. */
export class World {
  opened: OpenTarget = 'start';
  readonly workspaces: WorkspaceHandle[] = [];
  readonly conversations = new Map<string, ConversationHandle>();
  readonly pullRequests = new Map<string, PullRequestRecord>();
  /** Turn bookkeeping per session. */
  readonly turns = new Map<string, { open: boolean; count: number; tools: number; text: string }>();
  /** Person's messages, answers and approvals already consumed by `receives`. */
  readonly consumed = new Map<string, number>();
  /** The phone pairs before the video starts (`open('phone')`). */
  phone = false;
  /** Scripts the page runs before the app (a fake speech engine, say). */
  readonly initScripts: (() => void)[] = [];
  constructor(readonly fixture: RunnerFixture, readonly clock: ScenarioClock) {
    const f = fixture;
    // A clean slate: the fixture's own two workspaces and their extras go.
    f.collections.splice(0); f.sessions.splice(0); f.processes.splice(0); f.commands.splice(0); f.library.splice(0);
    f.items.issues.splice(0); f.githubRepositories.splice(0);
    // The Runner journals what it accepts, and an answer resolves its request.
    f.runner.journalMessages = true;
    f.remote.resolveOnAnswer = true;
    f.versionInfo.version = '0.2.0';
  }

  /** The person's GitHub account is connected (needed for issues and pull requests). */
  connectGitHub(login = 'you') { this.fixture.completeGitHub(); Object.assign(this.fixture.github, { login }); return this; }

  /** A repository the Runner knows: `acme/api`. */
  repository(fullName: string) {
    const f = this.fixture, [owner, name] = fullName.split('/');
    const identity = `github:${fullName.toLowerCase()}`;
    let repo = f.library.find(r => r.gitIdentity === identity);
    if (!repo) {
      repo = { id: `${owner}-${name}`, name, path: `/Users/you/src/${name}`, gitIdentity: identity, sourceUrl: `https://github.com/${fullName}` };
      f.library.push(repo);
      f.githubRepositories.push({ id: String(500 + f.githubRepositories.length), name, fullName, private: true, defaultBranch: 'main', htmlUrl: `https://github.com/${fullName}` });
    }
    return repo;
  }

  /** A workspace: repositories on a branch, ready, with no conversation yet. */
  workspace(name: string, options: WorkspaceOptions): WorkspaceHandle {
    const id = slug(name), root = `/Users/you/Lazybox/workspaces/${id}`, branch = options.branch || `lazybox/${id}`;
    const collection: FixtureCollection = { id, name, workId: id, root, state: 'ready', members: options.repos.map(fullName => {
      const repo = this.repository(fullName);
      return { spec: { name: repo.name!, repositoryId: repo.id, baseRef: 'main', branch }, path: `${root}/${repo.name}`, baseCommit: hex(id + repo.name), ready: true };
    }) };
    this.fixture.collections.push(collection);
    for (const fullName of options.repos) {
      const name = fullName.split('/')[1];
      Object.assign(this.fixture.shipState(id, name), { githubRepository: fullName, upstream: `origin/${branch}`, head: hex(`${id}-${name}-head`), base: hex(id + name) });
    }
    const handle = new WorkspaceHandle(this, collection);
    this.workspaces.push(handle);
    return handle;
  }

  /** A GitHub issue (for Start from an issue). */
  issue(spec: IssueSpec) {
    this.repository(spec.repo);
    const now = this.clock.now;
    this.fixture.items.issues.push({ repo: spec.repo, number: spec.number, title: spec.title, body: spec.body || '', author: spec.author || 'alice', assignees: spec.assignees || [String(this.fixture.github.login || 'you')], labels: spec.labels || [],
      updated: String(now - (spec.updatedHoursAgo ?? 2) * 3_600_000), comments: (spec.comments || []).map(c => ({ author: c.author, body: c.body, at: now - (c.daysAgo ?? 1) * 86_400_000 })) });
    return `https://github.com/${spec.repo}/issues/${spec.number}`;
  }

  /** Dictation works without a microphone: the scenario says what it hears (`hears('…')`). */
  voice() { this.initScripts.push(fakeSpeech); return this; }

  /** Where the app opens: a conversation, a workspace, the start page, or a paired phone's inbox. */
  open(target: OpenTarget) { this.opened = target; if (target === 'phone') this.phone = true; return this; }

  // ---- At run time --------------------------------------------------------

  /** The live side of a conversation: its session, its journal, its turn. */
  live(ctx: StepContext, handle: ConversationHandle): LiveConversation {
    const id = handle.resolve(ctx);
    const session = this.fixture.sessions.find(s => s.id === id);
    if (!session) throw new Error(`No conversation ${id} in the fake Runner`);
    return new LiveConversation(this, session);
  }
  /** The session the page shows (its `session` URL parameter), else the newest. */
  currentSessionId(ctx: StepContext): string {
    const fromUrl = new URL(ctx.page.url()).searchParams.get('session');
    if (fromUrl && this.fixture.sessions.some(s => s.id === fromUrl)) return this.successor(fromUrl);
    const newest = this.fixture.sessions.at(-1);
    if (!newest) throw new Error('No conversation yet: the scenario must open or start one');
    return newest.id;
  }
  private successor(id: string): string { const next = this.fixture.sessions.find(s => s.previousExecutionId === id); return next ? this.successor(next.id) : id; }
  turnOf(sessionId: string) { if (!this.turns.has(sessionId)) this.turns.set(sessionId, { open: false, count: 0, tools: 0, text: '' }); return this.turns.get(sessionId)!; }
}

/** A workspace, to add conversations, files and pull requests to. */
export class WorkspaceHandle {
  constructor(private world: World, readonly collection: FixtureCollection) {}
  get id() { return this.collection.id; }
  get name() { return this.collection.name; }
  get root() { return this.collection.root; }
  /** A conversation in this workspace, its agent idle. */
  conversation(title: string, options: ConversationOptions = {}): ConversationHandle {
    const f = this.world.fixture, clock = this.world.clock;
    const agent = options.agent || 'claude', id = `${this.id}-${slug(title)}`.slice(0, 48);
    const created = clock.now - (options.createdMinutesAgo ?? 30) * MIN;
    const session: FixtureSession = {
      id, collectionId: this.id, workId: this.id, conversationId: id, title, profileId: agent === 'codex' ? 'codex-default' : 'claude-default', provider: agent,
      model: options.model || (agent === 'codex' ? 'gpt-5.4' : 'claude-opus-5'), modelReported: true, gatewayId: 'direct', workingDirectory: this.root,
      capabilities: { files: true, images: true, resume: true, livePermission: true, stopAgent: true, ...(agent === 'codex' ? { steer: true } : {}) },
      permission: options.permission || 'ask', instructionsPending: false, pending: [], status: 'ready', busy: false, createdAtUnixMillis: String(created),
      agents: [{ id: 'root', name: 'Main agent', status: 'done', updatedAtUnixMillis: String(created) }],
      ...(options.queue ? { queued: 0, queuedMessages: [] } : {}), ...(options.sentNowReply ? { sentNowReply: options.sentNowReply } : {}),
    };
    f.sessions.push(session);
    f.openEvents(id);
    const handle = new ConversationHandle({ id }, title, this.world);
    this.world.conversations.set(id, handle);
    return handle;
  }
  /** A file in the workspace, as an agent wrote it (`api/docs/PLAN.md`, or the path an agent names). */
  file(path: string, text: string) {
    this.world.fixture.workspaceFiles.set(`${this.id}/${path}`, { path, text, version: `v-${slug(path)}-1`, binary: false, truncated: false });
    return this;
  }
  /** The file changes on disk (an agent or an editor wrote it): a tracked artifact gets a new version. */
  changesFile(path: string, text: string): Step {
    return step('world', `${path} changes on disk`, async ctx => {
      ctx.fixture.artifacts.fileChanged(this.id, path, text);
      await ctx.nudge();
      await ctx.pause(0.3);
    });
  }
  /** A pull request of this workspace's branch, opened later by `agent.opensPR(pr)` (or now, with `open()`). */
  pullRequest(spec: PullRequestSpec): PullRequestHandle {
    const repo = spec.repo || this.collection.members[0] && this.world.fixture.library.find(r => r.id === this.collection.members[0].spec.repositoryId)?.gitIdentity?.slice(7);
    if (!repo) throw new Error('pullRequest: say which repository');
    const key = `${repo}#${spec.number}`, member = spec.member || repo.split('/')[1];
    const sessionId = spec.conversation.ref.id;
    if (!sessionId) throw new Error('pullRequest: give the conversation that delivers it');
    const branch = this.collection.members.find(m => m.spec.name === member)?.spec.branch || 'main';
    this.world.pullRequests.set(key, { key, repo, member, spec, workspace: this, sessionId, branch, link: undefined, heads: 0 });
    return new PullRequestHandle(key);
  }
}

type PullRequestRecord = { key: string; repo: string; member: string; spec: PullRequestSpec; workspace: WorkspaceHandle; sessionId: string; branch: string; link: any; heads: number };

// ---- Conversations ----------------------------------------------------------

export interface RunOptions {
  /** What the command printed (the console shows it; the agent gets `agentView` if set). */
  output?: string;
  exit?: number;
  /** How long it runs, in seconds (default 2). */
  seconds?: number;
  description?: string;
  /**
   * Run through `lazybox run`: the output streams to the console panel and
   * the agent gets a compact view (COMMAND_OUTPUT.md).
   */
  console?: { agentView: string; filter?: string; outputBytes?: number };
}

export interface QuestionSpec { question: string; header?: string; options: (string | { label: string; description?: string })[]; multiSelect?: boolean }

/** Who the step is about: a named conversation, or the one on screen (`agent`). */
type ConversationRef = { id?: string; current?: boolean; title?: string };

/**
 * A conversation, and the agent in it. Its methods make steps: what the agent
 * does (thinks, reads, runs, writes, asks, opens a pull request), journaled
 * as the Runner journals a real agent, so the app shows the real thing.
 */
export class ConversationHandle {
  constructor(readonly ref: ConversationRef, readonly title?: string, private world?: World) {}

  /** The session id this handle means now. */
  resolve(ctx: StepContext): string {
    if (this.ref.id) return this.ref.id;
    if (this.ref.title) {
      const found = ctx.fixture.sessions.filter(s => s.title === this.ref.title).at(-1);
      if (!found) throw new Error(`No conversation titled “${this.ref.title}” yet`);
      return found.id;
    }
    return ctx.world.currentSessionId(ctx);
  }

  /** Writes its past into the journal: a builder, or Runner events (e.g. `longConversation()`). */
  history(build: ((h: History) => void) | JournalEvent[], options: { endedMinutesAgo?: number } = {}) {
    const world = this.world;
    if (!world || !this.ref.id) throw new Error('history() is for a conversation made with workspace.conversation()');
    const session = world.fixture.sessions.find(s => s.id === this.ref.id)!;
    const records = Array.isArray(build) ? build.map(event => ({ event, seconds: event.type === 'operation_accepted' ? 25 : 0.2 })) : (() => { const h = new History(session.workingDirectory || ''); build(h); return h.records; })();
    const total = records.reduce((n, r) => n + r.seconds, 0) * 1000;
    let at = world.clock.now - (options.endedMinutesAgo ?? 3) * MIN - total;
    const channel = world.fixture.eventsOf(this.ref.id) || world.fixture.openEvents(this.ref.id);
    for (const { event, seconds } of records) { at += seconds * 1000; channel.push(event, new Date(at).toISOString()); }
    const last = records.filter(r => r.event.type === 'message_delta').at(-1)?.event.text;
    const ended = world.clock.now - (options.endedMinutesAgo ?? 3) * MIN;
    Object.assign(session, { status: 'done', busy: false, lastMessage: last ? String(last).slice(-400) : undefined, agents: [{ id: 'root', name: 'Main agent', status: 'done', updatedAtUnixMillis: String(ended), turnEndedAtUnixMillis: String(ended), lastTurnStatus: 'completed' }] });
    world.turnOf(this.ref.id).count = records.filter(r => r.event.type === 'turn_started').length;
    return this;
  }

  /** Sets fields on its session (an escape hatch: `status`, `fault`, …). */
  set(fields: Partial<FixtureSession>): Step { return step('world', `set ${Object.keys(fields).join(', ')}`, async ctx => { Object.assign(ctx.world.live(ctx, this).session, fields); await ctx.nudge(); }); }

  /** The agent thinks for a while (the live bar says so). */
  thinks(seconds = 1.5): Step {
    return step('agent', `agent thinks ${seconds}s`, async ctx => {
      const live = ctx.world.live(ctx, this);
      live.ensureTurn(); live.phase('thinking'); live.emit({ type: 'activity', status: 'thinking' });
      await ctx.pause(seconds);
    });
  }

  /** The agent reads files (one quiet "Read N files" line). */
  reads(...paths: string[]): Step {
    return step('agent', `agent reads ${paths.join(', ')}`, async ctx => {
      const live = ctx.world.live(ctx, this);
      live.ensureTurn(); live.phase('tool', 'Read');
      for (const path of paths) {
        const id = live.toolStart('Read', { file_path: `${live.session.workingDirectory}/${path}` });
        await ctx.pause(0.35);
        live.toolEnd(id, 'line one\nline two\n');
      }
      await ctx.pause(0.3);
    });
  }

  /** The agent changes a file. */
  edits(path: string, change: { from?: string; to?: string } = {}): Step {
    return step('agent', `agent edits ${path}`, async ctx => {
      const live = ctx.world.live(ctx, this);
      live.ensureTurn(); live.phase('tool', 'Edit');
      const id = live.toolStart('Edit', { file_path: `${live.session.workingDirectory}/${path}`, old_string: change.from || 'before', new_string: change.to || 'after' });
      await ctx.pause(0.6);
      live.toolEnd(id, 'ok');
      await ctx.pause(0.2);
    });
  }

  /** The agent runs a command; with `console`, through `lazybox run`, streamed to the console. */
  runs(command: string, options: RunOptions = {}): Step {
    const exit = options.exit ?? 0;
    return step('agent', `agent runs ${options.console ? 'lazybox run -- ' : ''}${command} → exit ${exit}`, async ctx => {
      const live = ctx.world.live(ctx, this), seconds = options.seconds ?? 2, output = options.output ?? '';
      live.ensureTurn(); live.phase('tool', 'Bash');
      const shown = options.console ? `lazybox run -- ${command}` : command;
      const id = live.toolStart('Bash', { command: shown, ...(options.description ? { description: options.description } : {}) });
      if (options.console) {
        const run = ctx.fixture.streamCommand(live.session.id, command, ['/bin/sh', '-lc', command]);
        Object.assign(run, { mode: 'pipe', canBackground: false });
        const lines = output.split(/(?<=\n)/);
        const per = (seconds * 1000) / Math.max(1, lines.length);
        let pending = '';
        for (const line of lines) {
          pending += line;
          // In a video the lines arrive over the run; in a test, at once.
          if (ctx.video && pending.length > 0) { run.write(pending); pending = ''; await ctx.pause(per / 1000); }
        }
        if (pending) run.write(pending);
        const outputBytes = options.console.outputBytes ?? Buffer.byteLength(output);
        run.report = { exitCode: exit, outputBytes: String(outputBytes), agentBytes: String(Buffer.byteLength(options.console.agentView)), filter: options.console.filter || 'passthrough', outputPath: `/Users/you/Library/Application Support/Lazybox/output/${live.session.id}/${slug(command)}-1.log`, agentView: options.console.agentView };
        // The console uses real polling intervals to discover a run and then
        // track its age. Keep the run live for its declared duration in tests too.
        if (!ctx.video) await ctx.hold(seconds);
        run.finish(exit);
        live.toolEnd(id, [{ type: 'text', text: (exit ? `Exit code ${exit}\n` : '') + options.console.agentView }], exit ? 'failed' : 'completed');
      } else {
        await ctx.pause(seconds);
        live.toolEnd(id, exit ? [{ type: 'text', text: `Exit code ${exit}\n${output}` }] : output || 'ok', exit ? 'failed' : 'completed');
      }
      // A mission's agent: the run is evidence on its task, as the Runner records it.
      if (live.session.lineage?.taskId) ctx.fixture.missions.recordRun(live.session.id, { title: command, exitCode: exit });
      await ctx.pause(0.3);
    });
  }

  /** The agent starts a long command and is still in it (a dev server, a slow build): the turn stays busy. */
  startsRunning(command: string, options: { description?: string } = {}): Step {
    return step('agent', `agent starts ${command}`, async ctx => {
      const live = ctx.world.live(ctx, this);
      live.ensureTurn(); live.phase('tool', 'Bash');
      live.toolStart('Bash', { command, ...(options.description ? { description: options.description } : {}) });
      await ctx.nudge();
      await ctx.pause(0.6);
    });
  }

  /** The agent writes Markdown; streamed word by word unless `stream: false`. */
  writes(markdown: string, options: { stream?: boolean } = {}): Step {
    const preview = markdown.replace(/\s+/g, ' ').slice(0, 48);
    return step('agent', `agent writes “${preview}${markdown.length > 48 ? '…' : ''}”`, async ctx => {
      const live = ctx.world.live(ctx, this);
      live.ensureTurn(); live.phase('responding'); live.emit({ type: 'activity', status: 'responding' });
      const stream = options.stream ?? true;
      if (stream && ctx.video) {
        const pieces = markdown.match(/\S+\s*|\s+/g) || [markdown];
        for (let i = 0; i < pieces.length; i += 3) { live.emit({ type: 'message_delta', text: pieces.slice(i, i + 3).join('') }, false); await ctx.beat(0.05 + ctx.random() * 0.05); }
      } else live.emit({ type: 'message_delta', text: markdown }, false);
      live.turn.text += markdown;
      // What the agent wrote is on screen: its last words, as a person reads them.
      const tail = markdown.replace(/\[([^\]]*)\]\([^)]*\)/g, '$1').replace(/[`*]|^#+\s|^>\s?/gm, '').trim().split('\n').filter(Boolean).at(-1)?.trim().slice(-40);
      // Checked when this conversation is the one on screen (the page's session).
      const url = new URL(ctx.page.url()), shown = url.searchParams.get('session');
      const onScreen = shown ? shown === live.session.id : url.searchParams.get('view') !== 'remote' && ctx.world.currentSessionId(ctx) === live.session.id;
      if (tail && onScreen) await ctx.check(`the conversation shows “…${tail}”`, () => import('@playwright/test').then(({ expect }) => expect(ctx.page.locator('.messages, .remote-transcript').last()).toContainText(tail, { timeout: ctx.timeout })));
      await ctx.pause(0.4);
    });
  }

  /** The agent asks the person to choose (AskUserQuestion): the turn waits for the answer. */
  asks(question: QuestionSpec): Step {
    return step('agent', `agent asks “${question.question}”`, async ctx => {
      const live = ctx.world.live(ctx, this), requestId = `question-${live.turn.count}-${live.turn.tools}`;
      live.ensureTurn();
      const event: JournalEvent = { type: 'question', requestId, text: 'AskUserQuestion', providerDataJson: b64({ tool: 'AskUserQuestion', toolUseId: `ask-${requestId}`, input: { questions: [{ question: question.question, header: question.header || '', multiSelect: !!question.multiSelect, options: question.options.map(o => typeof o === 'string' ? { label: o, description: '' } : { description: '', ...o }) }] } }) };
      live.session.pending = [event];
      live.waiting('waiting_input');
      live.emit(event);
      await ctx.nudge();
      await ctx.pause(0.5);
    });
  }

  /** The agent asks to run a command (an approval). */
  requests(command: string, options: { description?: string } = {}): Step {
    return step('agent', `agent asks to run ${command}`, async ctx => {
      const live = ctx.world.live(ctx, this), requestId = `approval-${live.turn.count}-${live.turn.tools}`;
      live.ensureTurn();
      const event: JournalEvent = { type: 'approval', requestId, text: 'Bash', providerDataJson: b64({ tool: 'Bash', toolUseId: `use-${requestId}`, input: { command, description: options.description || '' }, always: [] }) };
      live.session.pending = [event];
      live.waiting('waiting_approval');
      live.emit(event);
      await ctx.pause(0.5);
    });
  }

  /**
   * Waits until what the person did reached the Runner — a message, an answer,
   * an approval — and checks it (`agent.receives('message', /refund/)`).
   */
  receives(kind: 'message' | 'answer' | 'approval', matching?: string | RegExp): Step {
    return step('expect', `agent receives the ${kind}${matching ? ` ${matching}` : ''}`, async ctx => {
      const id = this.resolve(ctx), key = `${id}:${kind}`;
      const sent = () => ctx.fixture.mutations.filter(m => m.method === 'Apply' && m.body.sessionId === id && m.body[kind]);
      const used = ctx.world.consumed.get(key) || 0;
      await ctx.check(`the Runner got the ${kind}`, async () => {
        const { expect } = await import('@playwright/test');
        await expect.poll(() => sent().length, { timeout: ctx.timeout }).toBeGreaterThan(used);
      });
      const body = sent()[used].body;
      ctx.world.consumed.set(key, used + 1);
      if (matching !== undefined) {
        const value = JSON.stringify(body[kind]);
        await ctx.check(`the ${kind} matches ${matching}`, async () => { if (typeof matching === 'string' ? !value.includes(matching) : !matching.test(value)) throw new Error(`${kind} was ${value}`); });
      }
      const live = ctx.world.live(ctx, this);
      if (kind !== 'message') { live.session.pending = []; live.phase('thinking'); }
    });
  }

  /** The agent's turn ends: it is idle, its last words the conversation's. */
  done(): Step {
    return step('agent', 'agent finishes its turn', async ctx => {
      const live = ctx.world.live(ctx, this);
      live.endTurn();
      await ctx.nudge();
      await ctx.pause(0.4);
    });
  }

  /** The agent opens the pull request (`gh pr create`): Lazybox tracks it. */
  opensPR(pr: PullRequestHandle): Step {
    return step('agent', `agent opens ${pr.key}`, async ctx => {
      const record = pr.record(ctx), live = ctx.world.live(ctx, this);
      live.ensureTurn(); live.phase('tool', 'Bash');
      const id = live.toolStart('Bash', { command: `gh pr create --title "${record.spec.title}" --fill`, description: 'Open the pull request' });
      await ctx.pause(1.2);
      const url = `https://github.com/${record.repo}/pull/${record.spec.number}`;
      live.toolEnd(id, `Creating pull request for ${record.branch} into main in ${record.repo}\n\n${url}\n`);
      pr.create(ctx);
      await ctx.nudge();
      await ctx.pause(0.4);
    });
  }

  /** The agent pushes its branch (a new head for the pull request). */
  pushes(pr?: PullRequestHandle): Step {
    return step('agent', 'agent pushes the branch', async ctx => {
      const live = ctx.world.live(ctx, this);
      live.ensureTurn(); live.phase('tool', 'Bash');
      const id = live.toolStart('Bash', { command: 'git push', description: 'Push the fix' });
      await ctx.pause(0.9);
      const record = pr?.record(ctx);
      if (record) { record.heads++; const head = hex(`${record.key}-head-${record.heads}`); record.link.item.pullRequest.headSha = head; Object.assign(ctx.fixture.shipState(record.workspace.id, record.member), { head }); }
      live.toolEnd(id, `To github.com:${record?.repo || 'origin'}.git\n   a1b2c3d..e4f5a6b  ${record?.branch || 'HEAD'} -> ${record?.branch || 'HEAD'}\n`);
      await ctx.nudge();
      await ctx.pause(0.3);
    });
  }

  /** The agent publishes a plan or report (`lazybox artifact put`): a card in the conversation. */
  publishes(artifact: { title: string; kind?: 'plan' | 'report' | 'spec' | 'notes'; content: string }): Step {
    return step('agent', `agent publishes “${artifact.title}”`, async ctx => {
      const live = ctx.world.live(ctx, this);
      live.ensureTurn(); live.phase('tool', 'Bash');
      const id = live.toolStart('Bash', { command: `lazybox artifact put --title "${artifact.title}" --kind ${artifact.kind || 'plan'} plan.md`, description: 'Publish the plan' });
      await ctx.pause(0.6);
      const put = ctx.fixture.artifacts.agentPut({ collectionId: live.session.collectionId, sessionId: live.session.id, conversationId: live.session.conversationId || live.session.id, title: artifact.title, kind: artifact.kind || 'plan', content: artifact.content });
      live.toolEnd(id, [{ type: 'text', text: `Published “${artifact.title}” as artifact ${put.id}, version 1 (${artifact.kind || 'plan'}). The user sees it in Lazybox → Artifacts. Publish a new version with --id ${put.id}.\n` }]);
      await ctx.pause(0.3);
    });
  }

  // ---- Orchestration (ORCHESTRATION.md) ----------------------------------

  /** The person lets this conversation run agents for a mission (a grant). */
  granted(mission: { title: string; brief?: string }, ceiling: { maxChildren?: number; planRequired?: boolean; budgetUsd?: number } = {}) {
    const world = this.world, id = this.ref.id;
    if (!world || !id) throw new Error('granted() is for a conversation made with workspace.conversation()');
    const m = world.fixture.missions.create(mission.title, mission.brief || '');
    const answer = world.fixture.missions.handle('UpdateMission', { missionId: m.id, grant: { sessionId: id, ceiling: { maxChildren: 3, ...ceiling } } });
    if (answer?.error) throw new Error(answer.error.message);
    return this;
  }
  /** An orchestrator proposes how to split the work; the person approves it. */
  proposesPlan(plan: { summary?: string; tasks: TaskSpec[] }): Step {
    return step('agent', `agent proposes a plan of ${plan.tasks.length} tasks`, async ctx => {
      const live = ctx.world.live(ctx, this);
      live.ensureTurn(); live.phase('tool', 'Bash');
      const id = live.toolStart('Bash', { command: 'lazybox mission plan plan.json', description: 'Propose the plan' });
      await ctx.pause(0.6);
      ctx.fixture.missions.proposePlan(live.session.id, plan);
      live.toolEnd(id, 'Plan proposed; waiting for the person to approve it.\n');
      live.waiting('waiting_input');
      await ctx.nudge();
      await ctx.pause(0.4);
    });
  }
  /** An orchestrator starts an agent for a task (its own conversation, brief as first message). */
  spawns(task: { title: string; brief?: string; role?: string; permission?: string }): Step {
    return step('agent', `agent starts “${task.title}”`, async ctx => {
      const live = ctx.world.live(ctx, this);
      live.ensureTurn(); live.phase('tool', 'Bash');
      const id = live.toolStart('Bash', { command: `lazybox agents start --title "${task.title}" --role ${task.role || 'worker'}`, description: `Start “${task.title}”` });
      await ctx.pause(0.5);
      // The plan's task of that title, if the person approved one: the child takes it.
      const planned = ctx.fixture.missions.missions.flatMap((m: any) => m.tasks).find((t: any) => t.title === task.title && t.planId && !t.attempts.length);
      const child = ctx.fixture.missions.spawnChild(live.session.id, { ...task, ...(planned ? { taskId: planned.id } : {}) });
      Object.assign(child, { provider: 'claude', model: 'claude-sonnet-5', modelReported: true, agents: [{ id: 'root', name: 'Main agent', status: 'running', phase: 'thinking', phaseSinceUnixMillis: ctx.clock.ms(), updatedAtUnixMillis: ctx.clock.ms() }] });
      live.toolEnd(id, `Started “${task.title}” (${child.id}).\n`);
      await ctx.nudge();
      await ctx.pause(0.4);
    });
  }
  /** A task's agent reports it done (the person reviews and accepts it). */
  reports(summary: string): Step {
    return step('agent', `agent reports “${summary.slice(0, 40)}”`, async ctx => {
      const live = ctx.world.live(ctx, this);
      live.endTurn();
      ctx.fixture.missions.reportChild(live.session.id, summary);
      await ctx.nudge();
      await ctx.pause(0.4);
    });
  }
}

/** The live side of a conversation, for steps: journal events and session state. */
export class LiveConversation {
  constructor(private world: World, readonly session: FixtureSession) {}
  get turn() { return this.world.turnOf(this.session.id); }
  get channel(): EventChannel { return this.world.fixture.eventsOf(this.session.id) || this.world.fixture.openEvents(this.session.id); }
  /** Journals an event now (scenario time). */
  emit(event: JournalEvent, stamp = true) { this.channel.push({ turnId: `t${this.turn.count}`, ...event }, stamp ? this.world.clock.iso() : this.world.clock.iso()); }
  ensureTurn() {
    const turn = this.turn;
    if (turn.open) return;
    turn.open = true; turn.count++; turn.text = '';
    // What a Runner fills in for a conversation the app started: where it works, its name.
    const collection = this.world.fixture.collections.find(c => c.id === this.session.collectionId);
    if (!this.session.workingDirectory && collection) this.session.workingDirectory = collection.root;
    if (!this.session.title && collection) this.session.title = collection.name;
    if (!this.session.provider) this.session.provider = String(this.session.profileId || '').startsWith('codex') ? 'codex' : 'claude';
    if (!this.session.model) Object.assign(this.session, { model: this.session.provider === 'codex' ? 'gpt-5.4' : 'claude-opus-5', modelReported: true });
    const now = this.world.clock.ms();
    Object.assign(this.session, { busy: true, status: 'running', turnStartedAtUnixMillis: now });
    this.session.agents = [{ id: 'root', name: 'Main agent', status: 'running', phase: 'thinking', phaseSinceUnixMillis: now, turnStartedAtUnixMillis: now, updatedAtUnixMillis: now }];
    this.emit({ type: 'turn_started' });
  }
  phase(phase: string, tool?: string) {
    const now = this.world.clock.ms(), root = this.session.agents?.[0] || {};
    this.session.agents = [{ ...root, id: 'root', name: 'Main agent', status: 'running', phase, tool: tool || '', phaseSinceUnixMillis: now, updatedAtUnixMillis: now }];
    Object.assign(this.session, { busy: true, status: 'running' });
  }
  waiting(status: 'waiting_input' | 'waiting_approval') {
    const now = this.world.clock.ms();
    Object.assign(this.session, { status, busy: true });
    this.session.agents = [{ id: 'root', name: 'Main agent', status, updatedAtUnixMillis: now, phaseSinceUnixMillis: now }];
  }
  toolStart(name: string, input: Record<string, unknown>) {
    const id = `toolu_${this.session.id.replace(/[^a-z0-9]/gi, '').slice(0, 10)}_${++this.turn.tools}`;
    this.emit({ type: 'activity', status: 'tool', name });
    this.emit({ type: 'tool_activity', providerDataJson: b64({ type: 'tool_call', id, name, status: 'running', input }) });
    return id;
  }
  toolEnd(id: string, output: unknown, status: 'completed' | 'failed' = 'completed') {
    this.emit({ type: 'tool_activity', providerDataJson: b64({ type: 'tool_call', id, status, output }) });
    this.emit({ type: 'activity', status: 'working' });
  }
  endTurn() {
    const turn = this.turn, now = this.world.clock.ms();
    if (turn.open) this.emit({ type: 'turn_ended', status: 'completed' });
    turn.open = false;
    Object.assign(this.session, { busy: false, status: 'done', pending: [], lastMessage: turn.text ? turn.text.slice(-400) : this.session.lastMessage });
    this.session.agents = [{ id: 'root', name: 'Main agent', status: 'done', updatedAtUnixMillis: now, turnEndedAtUnixMillis: now, lastTurnStatus: 'completed' }];
  }
}

// ---- Pull requests ----------------------------------------------------------

/** A pull request Lazybox tracks: its checks and review change as steps. */
export class PullRequestHandle {
  constructor(readonly key: string) {}
  record(ctx: StepContext): PullRequestRecord {
    const record = ctx.world.pullRequests.get(this.key);
    if (!record) throw new Error(`No pull request ${this.key} in this world`);
    return record;
  }
  /** Lazybox starts tracking it (the agent's output named it). */
  create(ctx: StepContext) {
    const record = this.record(ctx);
    if (record.link) return record.link;
    const now = ctx.clock.ms(), head = hex(`${record.key}-head-0`);
    record.link = {
      collectionId: record.workspace.id, collectionName: record.workspace.name, role: 'delivers', source: 'agent-output', sessionId: record.sessionId, conversationId: record.sessionId, member: record.member, linkedAtUnixMillis: now,
      item: { ref: { provider: 'github', key: record.key }, kind: 'pull_request', title: record.spec.title, url: `https://github.com/${record.repo}/pull/${record.spec.number}`, repository: record.repo,
        state: { category: 'review', label: 'Open' }, updatedAtUnixMillis: now,
        pullRequest: { head: record.branch, base: 'main', headSha: head, checks: 'pending', review: '', mergeable: false, mergeableState: 'unstable', checksTotal: record.spec.checks ?? 6, closedAtUnixMillis: '0', failingChecks: [] } },
    };
    ctx.fixture.tracked.links.push(record.link);
    Object.assign(ctx.fixture.shipState(record.workspace.id, record.member), { head, pr: { number: String(record.spec.number), title: record.spec.title, body: '', url: record.link.item.url, state: 'open', draft: false, base: 'main', headSha: head, mergeableState: 'unstable', reviewDecision: '', checksState: 'pending', author: 'you', checks: [] } });
    return record.link;
  }
  private change(label: string, apply: (pr: any, record: PullRequestRecord, ctx: StepContext) => void): Step {
    return step('world', `${this.key}: ${label}`, async ctx => {
      const record = this.record(ctx), link = record.link || this.create(ctx);
      apply(link.item.pullRequest, record, ctx);
      link.item.updatedAtUnixMillis = ctx.clock.ms();
      const ship = ctx.fixture.shipState(record.workspace.id, record.member).pr;
      if (ship) Object.assign(ship, { checksState: link.item.pullRequest.checks, headSha: link.item.pullRequest.headSha, reviewDecision: link.item.pullRequest.review, mergeableState: link.item.pullRequest.mergeableState });
      await ctx.nudge();
      await ctx.pause(0.5);
    });
  }
  /** Its checks are running. */
  checksRunning(): Step { return this.change('checks running', pr => Object.assign(pr, { checks: 'pending', failingChecks: [], mergeable: false })); }
  /** These checks failed (names as GitHub shows them). */
  checksFail(names: string[]): Step {
    return this.change(`checks fail (${names.join(', ')})`, (pr, record) => Object.assign(pr, { checks: 'failure', mergeable: false, mergeableState: 'unstable', failingChecks: names.map(name => ({ name, url: `https://github.com/${record.repo}/actions/runs/41/job/${name}` })) }));
  }
  /** Every check passed, and it can merge. */
  checksPass(options: { approved?: boolean } = {}): Step {
    return this.change('checks pass', pr => Object.assign(pr, { checks: 'success', failingChecks: [], mergeable: true, mergeableState: 'clean', ...(options.approved ? { review: 'approved' } : {}) }));
  }
  /** A reviewer approved it. */
  approved(): Step { return this.change('approved', pr => Object.assign(pr, { review: 'approved' })); }
}
