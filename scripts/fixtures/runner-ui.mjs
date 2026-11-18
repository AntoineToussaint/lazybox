// An explicitly fake RPC authority for isolated browser/native surface checks.
// It neither loads credentials nor runs Git, commands, or model inference.
import { createServer } from 'node:http';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHash, createPublicKey, generateKeyPairSync, randomUUID, verify } from 'node:crypto';
import { readFileSync } from 'node:fs';

const orchestrationProcess = JSON.parse(readFileSync(new URL('./orchestration-process.json', import.meta.url), 'utf8'));
import { createMissionFake } from './missions-fake.mjs';
import { createPowerFake } from './power-fake.mjs';
import { createConfigFake } from './config-fake.mjs';
import { createItemsFake } from './items-fake.mjs';
import { tailPage } from './session-tail.mjs';
import { createExtensionsFake, extensionAllows } from './extensions-fake.mjs';
import { createBrowserFake } from './browser-fake.mjs';
import { createAcpFake } from './acp-ui-fake.mjs';
import { artifactFixture } from './artifacts.mjs';

const frame = (value, flag = 0) => { const data = Buffer.from(JSON.stringify(value)); const head = Buffer.alloc(5); head[0] = flag; head.writeUInt32BE(data.length, 1); return Buffer.concat([head, data]); };
/** Text as the Runner reads it (agent.IsText): valid UTF-8, no NUL, not a PDF. */
const isText=data=>{if(data.includes(0)||data.subarray(0,5).toString('latin1')==='%PDF-')return false;try{new TextDecoder('utf-8',{fatal:true}).decode(data);return true;}catch{return false;}};
/** The provider data of a message Lazybox wrote (its `origin`), as the Runner journals it. */
const originData = origin => Buffer.from(JSON.stringify({ files: [], origin: { detail: '', copy: '', from: '', ...origin } })).toString('base64');
// The Runner's built-in rate card (telemetry.DefaultRates), as GetRunnerSettings reports it.
export function defaultModelPrices() {
  const rate = (provider, model, version, input, output, cacheRead, cacheWrite5m = 0, cacheWrite1h = 0, maxInput = 0) => ({ provider, model, version, input: String(input), output: String(output), cacheRead: String(cacheRead), cacheWrite5m: String(cacheWrite5m), cacheWrite1h: String(cacheWrite1h), maxInput: String(maxInput), custom: false });
  return [
    rate('claude', 'claude-fable-5-1', 'anthropic-standard-2026-11-03', 10000, 50000, 250, 12500, 20000),
    ...['claude-opus-4-6', 'claude-opus-4-7', 'claude-opus-4-8', 'claude-opus-5'].map(m => rate('claude', m, 'anthropic-standard-2026-11-02', 5000, 25000, 500, 6250, 10000)),
    rate('claude', 'claude-opus-5-5', 'anthropic-standard-2026-11-02', 4000, 20000, 200, 5000, 8000),
    rate('claude', 'claude-sonnet-4-6', 'anthropic-standard-2026-11-02', 3000, 15000, 300, 3750, 6000),
    rate('claude', 'claude-sonnet-5', 'anthropic-standard-2026-11-02', 2000, 10000, 200, 2500, 4000),
    rate('claude', 'claude-sonnet-5-5', 'anthropic-standard-2026-11-02', 2000, 10000, 100, 2500, 4000),
    rate('codex', 'gpt-5.4', 'openai-standard-2026-11-02', 2500, 15000, 250, 0, 0, 272000),
    rate('codex', 'gpt-5.3-codex', 'openai-standard-2026-11-02', 1750, 14000, 175),
  ];
}
const stream = values => Buffer.concat([...values.map(value => frame(value)), frame({}, 2)]);
export async function startRunnerFixture() {
  // A restarted Runner has a new bearer, and a launcher restart a new port: both change.
  let token = randomUUID(), url = '';
  const calls = [], mutations = [], pins = [];
  // Tracked pull requests (ListLinks): links the test sets, the reactions sent.
  const tracked = { links: [], events: [], trackingError: '', refusal: '', requests: [], sent: [] };
  const workspaceFiles=new Map();
  // Message attachments the fake Runner retained, by the path its journal lists.
  const attachments=new Map();let attachmentReadDelayMs=0;
  // Every workspace member holds the same small tree, so the Files panel, Go
  // to file and link resolution have folders and names to work with. Paths
  // outside it still read as a generic file, as before.
  const memberTree=['README.md','package.json','config.toml','src/main.go','src/routes.ts','src/components/App.tsx','scripts/build.sh'];
  const uniqueFiles={api:['scripts/undeclared-routes.json']};
  const treeFiles=collectionId=>{const collection=collections.find(c=>c.id===collectionId);return (collection?.members||[]).flatMap(m=>[...memberTree,...(uniqueFiles[m.spec.name]||[])].map(path=>m.spec.name+'/'+path));};
  const treeFolders=collectionId=>{const out=new Set(['.']);for(const file of treeFiles(collectionId)){const parts=file.split('/');for(let i=1;i<parts.length;i++)out.add(parts.slice(0,i).join('/'));}return out;};
  const listFolder=(collectionId,folder)=>{const files=treeFiles(collectionId),folders=treeFolders(collectionId),prefix=folder==='.'?'':folder+'/',seen=new Map();
    for(const path of [...folders,...files]){if(path==='.'||!path.startsWith(prefix))continue;const name=path.slice(prefix.length).split('/')[0];const full=prefix+name;if(!seen.has(full))seen.set(full,{name,path:full,directory:folders.has(full)});}
    return [...seen.values()].sort((a,b)=>a.directory!==b.directory?(a.directory?-1:1):a.name.localeCompare(b.name));};
  const fuzzy=(path,query)=>{const lower=path.toLowerCase(),q=query.toLowerCase().replace(/\s+/g,'');const base=path.lastIndexOf('/')+1;
    const match=from=>{const positions=[];let j=0;for(let i=from;i<lower.length&&j<q.length;i++)if(lower[i]===q[j]){positions.push(i);j++;}return j===q.length?positions:null;};
    const inName=q.includes('/')?null:match(base);const positions=inName||match(0);if(!positions)return null;return {path,positions,score:(inName?100:0)-path.length};};
  const eventStreams=new Map();
  const artifactStore=artifactFixture({workspaceFiles,mutations});
  // Messages a conversation accepted while at a subscription limit, by operation.
  const waitingMessages=new WeakMap();
  // A saved conversation without a channel: one line per execution of its lineage, and a sub-agent's line.
  const lineageEvents=sessionId=>{const lineage=[];let id=sessionId;while(id&&!lineage.includes(id)){lineage.unshift(id);id=sessions.find(s=>s.id===id)?.previousExecutionId;}return lineage.flatMap((id,index)=>[{cursor:String(index*2+1),event:{type:'message_delta',text:`Retained conversation for ${id}`,sessionReference:id}},{cursor:String(index*2+2),event:{type:'message_delta',text:'CHILD_PRIVATE_TRANSCRIPT',agentId:'child-review',parentAgentId:'root'}}]);};
  // ReadSessionTail's answer, as the Runner pages a timeline. `tailReadDelayMs` slows each read.
  const readTail=(events,body)=>{const page=tailPage(events,body);return page&&{events:page.events.map(event=>({...event,retained:String(page.retained)})),start:String(page.start),retained:String(page.retained)};};
  let tailReadDelayMs=0;
  // A saved conversation's journal is finite: after `complete()` the stream ends
  // like the Runner's does at EOF, instead of staying open for live events.
  // `cut()` ends the open streams the way a restarting Runner does (Unavailable):
  // the conversation goes on in the next Runner. An event's `receipt` is the
  // record's, as the Runner sends a receipt.
  const frozenSessions=new Map();
  const openEvents=id=>{const channel={events:[],clients:new Set(),done:false,push({receipt,...event},capturedAt){const record={cursor:String(this.events.length+1),event,...(receipt?{receipt}:{}),...(capturedAt?{capturedAt}:{})};this.events.push(record);for(const res of this.clients)res.write(frame({...record,retained:record.cursor}));},complete(){this.done=true;for(const res of this.clients)res.end(frame({},2));this.clients.clear();},cut(){for(const res of this.clients)res.end(frame({error:{code:'unavailable',message:'the Runner is restarting; reconnect'}},2));this.clients.clear();}};eventStreams.set(id,channel);return channel;};
  const collections = ['alpha', 'beta'].map((id, index) => ({ id, name: index ? 'Beta release' : 'Alpha feature', workId: id, root: `/fixture/collections/${id}`, state: 'ready', ownerExecutionId: `${id}-session`, members: ['api', 'web'].map((name, i) => ({ spec: { name, repositoryId: i ? 'frontend' : 'backend', baseRef: 'main', branch: `lazybox/${id}/${name}` }, path: `/fixture/collections/${id}/${name}`, baseCommit: (index ? 'b' : 'a').repeat(40), ready: true })) }));
  const arrangement = [];
  const arranged = () => { if (!arrangement.length) return collections; const at = new Map(arrangement.map((id, i) => [id, i])); return [...collections.filter(c => !at.has(c.id)).reverse(), ...collections.filter(c => at.has(c.id)).sort((a, b) => at.get(a.id) - at.get(b.id))]; };
  const sessions = collections.map(collection => ({ id: `${collection.id}-session`, collectionId: collection.id, workId: collection.id, profileId: 'claude-default', gatewayId: 'direct', workingDirectory: collection.root, capabilities: { files: true, images: true, resume: true, livePermission: true }, permission: 'ask', instructionsPending: collection.id === 'alpha', pending: [] }));
  sessions[0].status = 'running'; sessions[0].agents = [{ id: 'root', name: 'Main agent', status: 'done' }, { id: 'child-review', parentId: 'root', name: 'Review', status: 'running' }];
  const missions = createMissionFake({ sessions, collections, events: { open: openEvents, get: id => eventStreams.get(id) } });
  // The agent browser (BROWSER.md): explicitly fake, driven by the test (browser-fake.mjs).
  const browser = createBrowserFake({ frame, sessions, mutations });
  const commands = sessions.map(session => ({ id: `${session.collectionId}-command`, sessionId: session.id, title: `${session.collectionId} fixture build`, mode: 'managed', done: true, exitCode: 0, offset: '24' }));
  // startRun stands in for `lazybox run` in a session: a finished pipe with the
  // report of what its agent received.
  const startRun = (sessionId, title, report) => { const command = { id: `run-${commands.length}`, sessionId, title, mode: 'pipe', done: true, offset: '24', report }; commands.push(command); return command; };
  // startTerminalRun stands in for `cmd | lazybox log` in a workspace terminal:
  // a workspace run, optionally shared with a conversation (`--to`).
  const claimedShares = new Set();
  const startTerminalRun = (collectionId, title, report, { shareWith = '', done = true, output = 'fixture retained output\n' } = {}) => { const command = { id: `terminal-run-${commands.length}`, sessionId: '', collectionId, origin: 'terminal', shareWith, title, mode: 'pipe', done, offset: String(Buffer.byteLength(output)), report, output }; commands.push(command); return command; };
  const wireCommand = ({ output, live, ...command }) => command;
  // streamCommand stands in for a managed command whose output arrives while it is watched:
  // write(text) sends a chunk to every console following it, finish(exitCode) ends it.
  const streamCommand = (sessionId, title, argv = ['/bin/sh', '-lc', title]) => {
    const command = { id: `stream-${commands.length}`, sessionId, title, argv, mode: 'managed', done: false, offset: '0', output: '', live: { events: [], clients: new Set() } };
    const send = event => { command.live.events.push(event); for (const res of command.live.clients) res.write(frame(event)); };
    command.write = text => { const data = Buffer.from(text), offset = Buffer.byteLength(command.output); command.output += text; command.offset = String(offset + data.length); send({ cursor: String(command.live.events.length + 1), offset: String(offset), channel: 'stdout', data: data.toString('base64'), status: wireCommand(command) }); };
    command.finish = (exitCode = 0) => { Object.assign(command, { done: true, exitCode }); send({ cursor: String(command.live.events.length + 1), offset: command.offset, status: wireCommand(command) }); for (const res of command.live.clients) res.end(frame({}, 2)); command.live.clients.clear(); };
    commands.push(command); return command;
  };
  browser.streamCommand = streamCommand;
  const listedCommand = command => { const value = wireCommand(command); return value.report ? { ...value, report: { ...value.report, agentView: undefined } } : value; };
  const requests = new Map();
  const library = [{ id: 'backend', path: '/fixture/backend' }, { id: 'frontend', path: '/fixture/frontend' }];
  // Home checkouts by repository id (WORKSPACES.md, "Home checkouts"): explicitly fake, nothing is cloned.
  const homes = new Map(), homeRoot = '/fixture/home/lazybox/repos';
  const homeView = r => { const h = homes.get(r.id); return { repositoryId: r.id, name: r.name || r.id, path: h?.path || '', kind: h ? 'configured' : '', exists: !!h, branch: h ? (h.branch || 'main') : '', dirty: h?.dirty || 0, suggestedPath: `${homeRoot}/fixture-team/${r.name || r.id}`, head: h ? (h.head || 'abc1234') : '', defaultBranch: h ? 'main' : '', behind: h?.behind || 0 }; };
  const github = { state: 'setup_required', clientId: '', appSlug: '', login: '', userId: '0', pendingSignIn: undefined };
  const githubFlows = new Map();
  const discovery = { roots: [] };
  const githubRepositories = [
    { id: '101', name: 'api', fullName: 'fixture-team/api', private: true, defaultBranch: 'main', htmlUrl: 'https://github.com/fixture-team/api' },
    { id: '102', name: 'web', fullName: 'fixture-team/web', private: false, defaultBranch: 'main', htmlUrl: 'https://github.com/fixture-team/web' },
  ];
  let catalogRemote=[], catalogUpdated='0';const preparationFailures=new Set();const prepared=[];
  // How agents' requests for a repository were answered, by request id.
  const repositoryRequests=new Map();
  const catalogEntries=()=>{const rows=new Map();for(const r of catalogRemote)rows.set('github:'+r.fullName.toLowerCase(),{identity:'github:'+r.fullName.toLowerCase(),name:r.fullName});for(const r of discovery.roots.flatMap(root=>root.repositories||[]))rows.set(r.gitIdentity||r.path,{identity:r.gitIdentity||r.path,name:r.gitIdentity?.startsWith('github:')?r.gitIdentity.slice(7):r.name,path:r.path});for(const r of library){const identity=r.gitIdentity||'local:'+r.id;rows.set(identity,{...rows.get(identity),identity,name:r.gitIdentity?.startsWith('github:')?r.gitIdentity.slice(7):r.name||r.id,repositoryId:r.id});}return [...rows.values()];};
  const completeGitHub = () => { github.state = 'connected'; github.authMethod = 'github_app'; github.login = 'fixture-user'; github.userId = '42'; if (github.pendingSignIn) { github.pendingSignIn.state = 'connected'; github.pendingSignIn.userCode = ''; github.pendingSignIn.signInUrl = ''; } github.pendingSignIn = undefined; };

  const profiles = [{ id: 'claude-default', provider: 'claude', isolation: 'inherited' }, { id: 'codex-default', provider: 'codex', isolation: 'inherited', availability: { state: 'available', plan: 'plus', observedAtUnixMillis: String(Date.now()), windows: [{ name: 'codex:primary', durationMinutes: '300', usedPercent: 45, resetAt: String(Math.floor(Date.now()/1000)+7200) }, { name: 'codex:secondary', durationMinutes: '10080', usedPercent: 60, resetAt: String(Math.floor(Date.now()/1000)+172800) }] } }];
  profiles.push({ id:'codex-saved', provider:'codex', label:'Saved Codex', managed:true, isolation:'environment', availability:{...structuredClone(profiles[1].availability),email:'codex-fixture@example.invalid'} });
  const enrollments = new Map();
  // What GetProviderAvailability reports: each provider's program, and each
  // profile's state by id (default available and signed in). Explicitly fake:
  // nothing is probed. A test sets a program missing or a profile limited.
  const availability = {
    claude: { installed: true, path: '/fixture/runtime/claude', version: '2.1.311', source: 'bundled', error: '' },
    codex: { installed: true, path: '/fixture/bin/codex', version: '0.98.0', source: 'path', error: '' },
    states: {}, refreshes: 0,
    // Each provider's models (/model's list when an agent lists none): the Runner's built-in Claude list, what Codex lists.
    models: {
      claude: [{ value: 'claude-opus-5-5', displayName: 'Opus 5.5', description: 'For complex work and everyday tasks' }, { value: 'claude-sonnet-5-5', displayName: 'Sonnet 5.5', description: 'Most efficient for simpler tasks' }, { value: 'claude-haiku-5-5', displayName: 'Haiku 5.5', description: 'Fastest for quick answers' }, { value: 'claude-fable-5-2', displayName: 'Fable 5.2', description: 'For your toughest challenges' }],
      codex: [{ value: 'gpt-6.1-sol', displayName: 'GPT-6.1-Sol', description: 'Workhorse model for coding' }, { value: 'gpt-6-astra', displayName: 'GPT-6-Astra', description: 'Frontier intelligence' }, { value: 'gpt-6-luna', displayName: 'GPT-6-Luna', description: 'Fast and affordable' }],
    },
  };
  const availabilityRank = { available: 0, unverified: 1, limited: 2, billing: 3, sign_in: 4 };
  // ACP agents (acp-ui-fake.mjs): none installed until a test says so.
  const acp = createAcpFake({ profiles, mutations });
  const availabilityReport = () => ({ checkedAtUnixMillis: String(Date.now()), providers: ['claude', 'codex'].map(provider => {
    const program = availability[provider];
    const subscriptions = profiles.filter(p => p.provider === provider).map(p => { const s = availability.states[p.id] || { state: 'available', detail: 'Signed in' }; return { profileId: p.id, label: p.label || '', isolation: p.isolation, managed: !!p.managed, state: s.state, resetAt: String(s.resetAt || 0), detail: s.detail || '' }; })
      .sort((a, b) => availabilityRank[a.state] - availabilityRank[b.state] || Number(b.managed) - Number(a.managed));
    const best = subscriptions[0]?.state;
    const state = !program.installed ? 'not_installed' : program.error ? 'unusable' : !best ? 'no_subscription' : best === 'available' || best === 'unverified' ? 'ready' : best;
    return { provider, title: provider === 'claude' ? 'Claude Code' : 'Codex', state, installed: program.installed, path: program.installed ? program.path : '', version: program.installed ? program.version : '', source: program.installed ? program.source : '', error: program.installed ? program.error : `${provider === 'claude' ? 'Claude Code' : 'Codex'} is not installed on the Runner's computer.`, installCommand: program.installed ? '' : provider === 'claude' ? 'curl -fsSL https://claude.ai/install.sh | bash' : 'npm install -g @openai/codex', subscriptions, models: availability.models[provider] || [] };
  }).concat(acp.providers()) });
  const completeEnrollment = id => { const flow = enrollments.get(id); if (!flow) throw new Error('Unknown fixture enrollment'); if (flow.state === 'added') return; flow.state = 'added'; flow.signInUrl = ''; flow.userCode = ''; profiles.push({ id: flow.profileId, provider: flow.provider, label: flow.label, managed:true, isolation: 'environment', availability: flow.provider === 'codex' ? { state: 'available', plan: 'pro', observedAtUnixMillis: String(Date.now()), windows: [{ name: 'codex:primary', durationMinutes: '300', usedPercent: 73, resetAt: String(Math.floor(Date.now()/1000)+3600) }] } : { state: 'unknown' } }); };
  // The provider refused the sign-in: errorCode is what the client offers a way out of.
  const failEnrollment = (id, error, errorCode = '') => { const flow = enrollments.get(id); if (!flow) throw new Error('Unknown fixture enrollment'); Object.assign(flow, { state: 'failed', error, errorCode, signInUrl: '', userCode: '' }); };

  // Terminals echo what is typed and answer each line, like a minimal zsh with
  // Lazybox's integration: its prompt marks (OSC 7, then 133;B) once it can
  // read a line, 133;C when a line runs, 133;D;<status> when it ends.
  // `terminalStartDelayMs` makes a new shell start slowly, as a heavy .zshrc
  // does, and its prompt theme take a moment after the precmd mark: until its
  // prompt, the terminal echoes what is typed (tty echo), and the prompt
  // shows it again, as zsh's line editor does with typeahead.
  // A line starting `watch ` keeps running until Ctrl+C.
  const terminals=new Map();let terminalStartDelayMs=0;
  const terminalStatus=t=>({id:t.id,collectionId:t.collectionId,directory:t.directory,title:t.title,shell:'/bin/fixture-sh',cols:t.cols,rows:t.rows,offset:String(t.output.length),retained:String(t.output.length),done:t.done,...(t.done?{exitCode:0}:{}),createdAtUnixMillis:String(t.createdAt),...(t.place?{place:t.place,repositoryId:t.repositoryId||'',cwd:t.cwd}:{}),...shellMarks(t)});
  // As the Runner does: the shell's command-end marks (OSC 133;D;<status>) counted, with the latest status; prompts ready (133;B); idle at the last one.
  const shellMarks=t=>{const text=t.output.toString();const ends=[...text.matchAll(/\x1b\]133;D;(\d+)\x07/g)];const marks=[...text.matchAll(/\x1b\]133;([BCD])/g)];const ready=marks.filter(m=>m[1]==='B').length;
    return {promptMarks:true,...(ends.length?{commandEnds:String(ends.length),lastCommandStatus:Number(ends.at(-1)[1])}:{}),...(ready?{promptsReady:String(ready)}:{}),idle:!t.done&&marks.at(-1)?.[1]==='B'};};
  const terminalOutput=(t,text)=>{const offset=t.output.length;const data=Buffer.from(text);t.output=Buffer.concat([t.output,data]);for(const res of t.clients)res.write(frame({offset:String(offset),data:data.toString('base64'),status:terminalStatus(t)}));};
  const promptOf=t=>`\x1b]133;D;${t.lastStatus??0}\x07$ \x1b]7;file://fixture${t.cwd||'/fixture/workspace/'+(t.directory||'')}\x07\x1b]133;B\x07`;
  // The shell's line editor reads keys: echoed onto its line, Return runs it, Ctrl+U clears it, Ctrl+C stops what runs.
  const terminalKeys=(t,text)=>{
    for(const character of text){
      if(!t.ready){t.typeahead+=character;terminalOutput(t,character==='\r'?'\r\n':character);continue;}
      if(t.running){if(character==='\x03'){t.running=false;t.lastStatus=130;terminalOutput(t,'^C\r\n'+promptOf(t));}continue;}
      if(character==='\x15'){t.line='';terminalOutput(t,'\r$ \x1b[K');continue;}
      if(character!=='\r'){t.line+=character;terminalOutput(t,character);continue;}
      const line=t.line;t.line='';
      if(line==='exit'){terminalOutput(t,'\r\nexit\r\n');t.done=true;for(const client of t.clients)client.end(Buffer.concat([frame({offset:String(t.output.length),status:terminalStatus(t)}),frame({},2)]));break;}
      if(line.startsWith('watch ')){t.running=true;terminalOutput(t,`\r\n\x1b]133;C\x07watching: ${line.slice(6)}\r\n`);continue;}
      t.lastStatus=/^(?:false|exit [1-9])|\bfail/.test(line)?1:0;terminalOutput(t,`\r\n\x1b]133;C\x07ran: ${line}\r\n`+promptOf(t));
    }
  };
  // The first prompt: zsh's precmd marks (133;D) before a theme draws the prompt; only then does the line editor read keys.
  const shellReady=t=>{if(t.done||t.ready)return;const [precmd,prompt]=promptOf(t).split(/(?<=\x07)(?=\$ )/);terminalOutput(t,precmd);
    const draw=()=>{if(t.done||t.ready)return;t.ready=true;terminalOutput(t,prompt);const typeahead=t.typeahead;t.typeahead='';terminalKeys(t,typeahead);};
    if(terminalStartDelayMs)setTimeout(draw,400);else draw();};

  // The PR loop per workspace member: uncommitted files, branch commits, push
  // state and its pull request. Nothing here runs Git or contacts GitHub.
  const ship=new Map();
  const hex=()=>randomUUID().replaceAll('-','').padEnd(40,'0').slice(0,40);
  const shipState=(collectionId,member)=>{const key=collectionId+'/'+member;if(!ship.has(key)){const spec=collections.find(c=>c.id===collectionId)?.members.find(m=>m.spec.name===member)?.spec;ship.set(key,{branch:spec?.branch||'feature/'+member,head:hex(),files:[],commits:[],committed:[],upstream:'',ahead:0,githubRepository:'',pr:undefined,mergeRefusal:''});}return ship.get(key);};
  // Review comments, per workspace, as the Runner keeps them. A test sets
  // diffs (what ReadWorkspaceDiff returns per workspace/member/path) and
  // workingFiles (a file's current text, for freshness; absent: unchanged).
  const reviews=new Map(),diffs=new Map(),workingFiles=new Map();
  const reviewOf=collectionId=>{if(!reviews.has(collectionId))reviews.set(collectionId,{autoResolve:false,comments:[],runs:[]});return reviews.get(collectionId);};
  const anchorOf=(collectionId,c)=>{
   const text=workingFiles.get(`${collectionId}/${c.member}/${c.path}`);
   if(text===undefined)return {freshness:'current',currentStartLine:c.side==='old'?0:c.startLine,currentEndLine:c.side==='old'?0:c.endLine,freshnessReason:''};
   if(text===null)return {freshness:'outdated',currentStartLine:0,currentEndLine:0,freshnessReason:'The file no longer exists.'};
   if(c.side==='old')return {freshness:'outdated',currentStartLine:0,currentEndLine:0,freshnessReason:'The file changed since; the removed lines are quoted as they were.'};
   const lines=text.replace(/\n$/,'').split('\n'),n=c.snippet.length;let best=0;
   for(let i=0;i+n<=lines.length;i++)if(c.snippet.every((line,k)=>lines[i+k].trimEnd()===line.trimEnd())&&(!best||Math.abs(i+1-c.startLine)<Math.abs(best-c.startLine)))best=i+1;
   if(!best)return {freshness:'outdated',currentStartLine:0,currentEndLine:0,freshnessReason:'These lines changed since the comment was made.'};
   return {freshness:best===c.startLine?'current':'moved',currentStartLine:best,currentEndLine:best+n-1,freshnessReason:best===c.startLine?'':'The same lines moved; the comment follows them.'};
  };
  const reviewReply=(collectionId,addedId='')=>{const r=reviewOf(collectionId);return {autoResolve:r.autoResolve,addedId,comments:r.comments.map(c=>({...c,...anchorOf(collectionId,c)})),runs:[...r.runs].reverse().map(run=>wireRun(collectionId,run))};};
  // Agent reviews (GITHUB.md, "Agent review"), as the Runner keeps them: a
  // read-only reviewer conversation per review, its findings as comments by
  // that agent. Explicitly fake: no agent runs; a test files findings with
  // fileFinding(review, …) as `lazybox review add` would, and ends the
  // reviewer's turn with finishReview(review).
  const reviewBriefs=new Map();
  const liveFinding=c=>!['resolved','wont_fix','dismissed'].includes(c.disposition);
  const runCounts=(collectionId,run)=>{const live=reviewOf(collectionId).comments.filter(c=>c.reviewId===run.id&&liveFinding(c));const n=severity=>live.filter(c=>c.severity===severity).length;return {blockers:n('blocker'),majors:n('major'),minors:n('minor'),nits:n('nit')};};
  const wireRun=(collectionId,run)=>({...run,collectionId,...runCounts(collectionId,run)});
  const providerOf=session=>session?.provider||profiles.find(p=>p.id===session?.profileId)?.provider||'claude';
  const providerName=provider=>provider==='codex'?'Codex':'Claude';
  const reviewEvent=(type,collectionId,run)=>{const counts=runCounts(collectionId,run);const lower=run.scopeLabel.startsWith('PR ')?run.scopeLabel:run.scopeLabel[0].toLowerCase()+run.scopeLabel.slice(1);const summary=counts.blockers+counts.majors+counts.minors+counts.nits?[counts.blockers&&`${counts.blockers} blocker${counts.blockers>1?'s':''}`,counts.majors&&`${counts.majors} major`,counts.minors&&`${counts.minors} minor`,counts.nits&&`${counts.nits} nit${counts.nits>1?'s':''}`].filter(Boolean).join(', '):'no open findings';
    eventStreams.get(run.sourceSessionId)?.push({type,requestId:run.id,status:run.state,text:type==='review_started'?`${run.reviewerLabel} is reviewing ${lower}`:`${run.reviewerLabel} reviewed ${lower} · ${summary}`,providerDataJson:Buffer.from(JSON.stringify({review:run.id,workspace:collectionId,reviewerSession:run.reviewerSessionId,reviewerLabel:run.reviewerLabel,sourceSession:run.sourceSessionId,sourceLabel:run.sourceLabel,scope:run.scope,scopeLabel:run.scopeLabel,turn:run.turn,...counts})).toString('base64')});};
  const runOf=id=>{for(const [collectionId,r] of reviews){const run=r.runs.find(x=>x.id===id);if(run)return {collectionId,run};}throw new Error(`fixture review ${id} not found`);};
  /** `lazybox review add` from the reviewer: a proposed finding, idempotent by key. */
  // The loop (GITHUB.md, "The review loop"), as the Runner runs it: rounds,
  // still-open accounting, stop rules, the person's controls. Explicitly
  // fake: a test plays the agents with authorFixed, resolveFinding,
  // reopenFinding, finishReview and exchange.
  const unix=()=>String(Math.floor(Date.now()/1000));
  const loopNote=(run,kind,actor,text,finding='')=>run.timeline.push({atUnix:unix(),kind,actor,text,findingId:finding,round:run.round});
  const plural=(n,one,many)=>n===1?`1 ${one}`:`${n} ${many}`;
  const escalate=(collectionId,run,text)=>eventStreams.get(run.sourceSessionId)?.push({type:'review_escalated',requestId:run.id,status:run.stopReason,text,providerDataJson:Buffer.from(JSON.stringify({review:run.id,workspace:collectionId,reviewerSession:run.reviewerSessionId,reviewerLabel:run.reviewerLabel,sourceLabel:run.sourceLabel})).toString('base64')});
  const stopLoop=(collectionId,run,reason,text,escalates)=>{Object.assign(run,{stopReason:reason,phase:'done',detail:text});loopNote(run,escalates?'escalated':'stopped','Lazybox',text);if(escalates)escalate(collectionId,run,text);};
  const active=run=>!!run.phase&&run.phase!=='done'&&!run.stopReason;
  const exchangeLine=(run,text)=>{for(const id of [run.reviewerSessionId,run.sourceSessionId])eventStreams.get(id)?.push({type:'review_exchange',requestId:run.id,text,providerDataJson:Buffer.from(JSON.stringify({review:run.id,reviewerSession:run.reviewerSessionId,sourceSession:run.sourceSessionId})).toString('base64')});};
  /** `lazybox ask`: one agent asks, the other answers; both conversations show it. */
  const exchange=(id,{from='reviewer',question,answer})=>{const {run}=runOf(id);const [asker,answerer]=from==='reviewer'?[run.reviewerLabel,run.sourceLabel]:[run.sourceLabel,run.reviewerLabel];
    loopNote(run,'asked',asker,`${asker} asked ${answerer}: ${question}`);exchangeLine(run,`${asker} asked ${answerer}: ${question}`);
    if(answer!==undefined){loopNote(run,'answered',answerer,`${answerer} answered ${asker}: ${answer}`);exchangeLine(run,`${answerer} answered ${asker}: ${answer}`);}};
  const findingsSent=(collectionId,ids,by,to)=>{for(const run of reviewOf(collectionId).runs){const mine=reviewOf(collectionId).comments.filter(c=>c.reviewId===run.id&&ids.includes(c.id));if(!mine.length)continue;for(const c of mine)c.delivered=true;
    loopNote(run,'sent',by,`Sent ${plural(mine.length,'finding','findings')} to ${to||run.sourceLabel}.`);
    if(active(run)&&run.loop!=='off'&&run.phase!=='reviewing'){run.phase='fixing';loopNote(run,'fixing','Lazybox',`Waiting for ${run.sourceLabel} to change files; then ${run.reviewerLabel} re-reviews.`);}}};
  /** The author's turn ended with changes after the findings were sent: the next round starts. */
  const authorFixed=(id,turn=2,files=1)=>{const {collectionId,run}=runOf(id);if(!active(run)||run.phase!=='fixing')return run;
    if(run.paused){loopNote(run,'fixing',run.sourceLabel,`Turn ${turn} changed ${plural(files,'file','files')}; the loop is paused, so no re-review.`);return run;}
    run.round++;run.rounds.push({number:run.round,trigger:'author_turn',authorTurn:turn,startedAtUnix:unix()});Object.assign(run,{phase:'reviewing',state:'running'});
    loopNote(run,'round_started','Lazybox',`Round ${run.round}: ${run.sourceLabel} changed ${plural(files,'file','files')} in turn ${turn}; ${run.reviewerLabel} re-reviews that turn against its open findings.`);
    eventStreams.get(run.reviewerSessionId)?.push({type:'operation_accepted',operationId:`review-${run.id}-round-${run.round}`,text:`[Lazybox] Round ${run.round} of ${run.maxRounds}. ${run.sourceLabel} changed files in turn ${turn} after your findings. Re-review only that change, against your findings still open.`,
     providerDataJson:originData({kind:'review_round',summary:`Re-review asked · ${run.sourceLabel} changed files`,detail:`Round ${run.round} of ${run.maxRounds}`,from:run.sourceLabel})});
    return run;};
  /** `lazybox review resolve|reopen`. */
  const verdict=(findingId,state,note='')=>{for(const [collectionId,r] of reviews){const c=r.comments.find(x=>x.id===findingId);if(!c)continue;const run=r.runs.find(x=>x.id===c.reviewId);
    if(state==='resolved'){Object.assign(c,{disposition:'resolved',resolvedBy:'',note:`Resolved by ${run.reviewerLabel}${note?': '+note:''}`});loopNote(run,'resolved',run.reviewerLabel,`Resolved “${c.title}”${note?': '+note:''}`,c.id);}
    else{Object.assign(c,{disposition:'open',sentTo:'',sentToTitle:'',sentAtUnix:'0',sentHead:'',note:`Still open (${run.reviewerLabel})${note?': '+note:''}`});loopNote(run,'reopened',run.reviewerLabel,`Still open: “${c.title}”${note?': '+note:''}`,c.id);}
    return c;}throw new Error(`fixture finding ${findingId} not found`);};
  const resolveFinding=(id,note)=>verdict(id,'resolved',note), reopenFinding=(id,note)=>verdict(id,'open',note);
  const fileFinding=(id,{member='api',path,startLine,endLine=startLine,snippet,severity,title,body='',key})=>{
    const {collectionId,run}=runOf(id),r=reviewOf(collectionId);key=key||`k-${path}:${startLine}:${title.toLowerCase()}`;
    const old=r.comments.find(c=>c.reviewId===run.id&&c.findingKey===key);const now=String(Math.floor(Date.now()/1000));
    if(old){Object.assign(old,{severity,title,body:body||title,startLine,endLine,snippet,updatedAtUnix:now});return old;}
    const c={id:'rc-'+randomUUID().slice(0,8),member,path,side:'new',startLine,endLine,snippet,hash:'fixture',body:body||title,head:shipState(collectionId,member).head,view:'uncommitted',base:'',createdAtUnix:now,updatedAtUnix:now,disposition:'proposed',
      author:'agent',authorLabel:run.reviewerLabel,authorSessionId:run.reviewerSessionId,severity,title,reviewId:run.id,findingKey:key,round:run.round||1};
    r.comments.push(c);loopNote(run,'filed',run.reviewerLabel,`${severity[0].toUpperCase()+severity.slice(1)}: ${title} (${member}/${path}:${startLine})`,c.id);return c;};
  /** The reviewer's turn ends: the review is done, and the author's conversation is told. */
  const finishReview=id=>{const {collectionId,run}=runOf(id);run.state='done';run.updatedAtUnix=unix();
    if(run.phase==='reviewing'){const r=reviewOf(collectionId),round=run.rounds.at(-1);let filed=0,stillOpen=0,blockers=0;
      for(const c of r.comments.filter(c=>c.reviewId===run.id)){if(c.round===run.round)filed++;else if(c.round<run.round&&liveFinding(c)&&c.delivered){c.stillOpenRounds=(c.stillOpenRounds||0)+1;stillOpen++;}if(liveFinding(c)&&c.severity==='blocker')blockers++;}
      const resolved=run.timeline.filter(e=>e.round===run.round&&e.kind==='resolved').length;Object.assign(round,{endedAtUnix:unix(),filed,resolved,stillOpen,blockers});
      loopNote(run,'round_ended',run.reviewerLabel,run.round===1?`Round 1 ended: ${plural(filed,'finding','findings')}; ${plural(blockers,'blocker','blockers')} open.`:`Round ${run.round} ended: ${filed} filed, ${resolved} resolved, ${stillOpen} still open; ${plural(blockers,'blocker','blockers')} open.`);
      const stuck=r.comments.find(c=>c.reviewId===run.id&&liveFinding(c)&&c.severity==='blocker'&&(c.stillOpenRounds||0)>=2);
      if(stuck)stopLoop(collectionId,run,'stuck',`The blocker “${stuck.title}” (${stuck.id}) is still open after two fixes: decide how to proceed.`,true);
      else if(!blockers){const majors=r.comments.filter(c=>c.reviewId===run.id&&liveFinding(c)&&c.severity==='major').length;if(run.loop==='off')run.phase='done';else stopLoop(collectionId,run,'clean',majors?`No blocker open after ${plural(run.round,'round','rounds')}; ${majors} major still open.`:`Clean: no blocker open after ${plural(run.round,'round','rounds')}.`,false);}
      else if(run.loop==='off')run.phase='done';
      else if(run.round>=run.maxRounds)stopLoop(collectionId,run,'max_rounds',`${plural(blockers,'blocker','blockers')} still open after ${plural(run.round,'round','rounds')}, the limit: decide how to proceed.`,true);
      else if(run.loop==='auto'&&!run.paused){const ids=r.comments.filter(c=>c.reviewId===run.id&&['proposed','open'].includes(c.disposition)).map(c=>c.id);run.phase='triage';
        for(const c of r.comments.filter(c=>ids.includes(c.id)))Object.assign(c,{disposition:'sent',sentTo:run.sourceSessionId,sentToTitle:run.sourceTitle,sentAtUnix:unix()});
        eventStreams.get(run.sourceSessionId)?.push({type:'operation_accepted',operationId:`review-${run.id}-findings-${run.round}`,text:`Please address ${ids.length===1?'this review comment':`these ${ids.length} review comments`} on the workspace’s changes.`,
         providerDataJson:originData({kind:'review_findings',summary:`${run.reviewerLabel} review · ${plural(ids.length,'finding','findings')} sent`,detail:run.maxRounds>1?`Round ${run.round} of ${run.maxRounds}`:'',from:run.reviewerLabel})});
        findingsSent(collectionId,ids,'Lazybox',run.sourceLabel);}
      else if(!r.comments.some(c=>c.reviewId===run.id&&liveFinding(c)&&['proposed','open'].includes(c.disposition))&&r.comments.some(c=>c.reviewId===run.id&&liveFinding(c)&&c.disposition==='sent')){run.phase='fixing';loopNote(run,'fixing','Lazybox',`The findings were sent; waiting for ${run.sourceLabel} to change files, then ${run.reviewerLabel} re-reviews.`);}
      else{run.phase='triage';loopNote(run,'triage','Lazybox','Waiting for you: accept the findings to send, dismiss the others.');}
      if(!run.stopReason)run.detail=`${run.reviewerLabel} reviewed ${run.scopeLabel.toLowerCase()}.`;}
    reviewEvent('review_finished',collectionId,run);return run;};
  const pullRequest=state=>state.pr?{...state.pr,githubRepository:state.githubRepository,branch:state.branch,defaultBranch:'main',found:true}:{githubRepository:state.githubRepository,branch:state.branch,defaultBranch:'main',found:false,...(state.githubRepository?{}:{unavailable:'origin is not a GitHub repository; push works, pull requests need GitHub'})};

  // Per-turn code checkpoints by session: what each turn changed, what a revert
  // would change, and the reverts made. Explicitly fake: no Git behind them.
  // setCheckpoints(sessionId, {turns:[{turn,operationId,prompt,files:[{member,path,status,additions,deletions}]}]}).
  const checkpoints=new Map();
  const checkpointState=sessionId=>{if(!checkpoints.has(sessionId))checkpoints.set(sessionId,{turns:[],reverts:[],notePending:false});return checkpoints.get(sessionId);};
  const setCheckpoints=(sessionId,{turns})=>{const state=checkpointState(sessionId);state.turns=turns.map((t,i)=>({ended:true,...t,turn:t.turn||i+1}));return state;};
  const wireTurn=t=>{const members=[...new Set(t.files.map(f=>f.member).concat(t.members||['api']))];return {turn:t.turn,operationId:t.operationId,prompt:t.prompt||'',startedAtUnixMillis:String(Date.now()-60000),endedAtUnixMillis:t.ended?String(Date.now()-30000):'0',files:t.files.length,additions:t.files.reduce((n,f)=>n+(f.additions||0),0),deletions:t.files.reduce((n,f)=>n+(f.deletions||0),0),before:members.map(member=>({member,commit:'c'.repeat(40),branch:'lazybox/'+member,skipped:[],skippedCount:t.skipped?.[member]||0,error:''}))};};
  const turnRepositories=files=>{const byMember=new Map();for(const f of files){if(!byMember.has(f.member))byMember.set(f.member,[]);byMember.get(f.member).push({path:f.path,originalPath:f.originalPath||'',status:f.status,additions:f.additions||0,deletions:f.deletions||0,binary:!!f.binary});}return [...byMember].map(([member,files])=>({member,files}));};
  const flip={A:'D',D:'A',M:'M',R:'R'};
  // The user's themes folder as ListThemes reports it. Explicitly fake: no
  // files and no TOML parser behind it, so a test sets a file's json (or
  // error) itself. The revision is derived from the files, so it changes on
  // every mutation, the test's own included.
  const themeFolder={directory:'/fixture/state/themes',files:[]};
  const themeReply=()=>({directory:themeFolder.directory,files:themeFolder.files,revision:createHash('sha256').update(JSON.stringify(themeFolder.files)).digest('hex').slice(0,16)});
  let runnerSettings={revision:'1',naming:'repositories',permission:'',context:'You are running inside Lazybox.',claudeContext:'',codexContext:'',suggestedName:'gentle-copper-otter',prices:[],processBackend:'',rotationExcludedProfileIds:[],claudeSubscriptionSignIn:false,claudeSignInCarriedOver:false,notices:[]};
  // How Claude agents sign in (GetClaudeAuth), explicitly fake: a key is kept in memory and only its last four characters are answered.
  const claudeAuth={key:'',cloudProvider:''};
  const claudeAuthReply=()=>({apiKeySet:!!claudeAuth.key,apiKeyHint:claudeAuth.key.slice(-4),cloudProvider:claudeAuth.cloudProvider});
  // The fake Runner process. RestartRunner answers, then the "process" is down
  // for restartMs (every RPC fails as unavailable) before a new instance serves.
  // listDelayMs holds ListCollections answers (a slow first response);
  // refusedBearers records every bearer the fake Runner did not accept.
  const runner = { instanceId: randomUUID(), restartAvailable: true, restartError: '', restartMs: 1500, down: false, restarts: 0, listDelayMs: 0, preparationDelayMs: 0, refusedBearers: [] };
  // Hosted processes as ListProcesses reports them, for the Processes view.
  // Explicitly fake: no session host, tmux or process table behind them. Open
  // terminals are listed too, as the Runner lists their hosts.
  const processHosting = { configured: 'sessionhost', tmuxVersion: 'tmux 3.4', tmuxProblem: '', fallback: '' };
  const versionInfo = { version: 'dev' };
  const usage = (cpuPercent, megabytes, processCount) => ({ cpuPercent, memoryBytes: String(megabytes * 1048576), processCount });
  const processes = [
    { id: 'host-agent-alpha', kind: 'agent', backend: 'sessionhost', state: 'running', attached: true, hostPid: 4101, programPid: 4102, uptimeSeconds: '8100', label: 'Fix the parser', workspaceId: 'alpha', workspaceName: 'Alpha feature', sessionId: 'alpha-session', conversationTitle: 'Fix the parser', provider: 'claude', usage: usage(3.2, 412, 5) },
    { id: 'host-command-beta', kind: 'command', backend: 'tmux', state: 'running', attached: true, hostPid: 4201, programPid: 4202, uptimeSeconds: '95', label: 'go test ./...', workspaceId: 'beta', workspaceName: 'Beta release', sessionId: 'beta-session', conversationTitle: 'Ship the release', provider: 'claude', commandId: 'beta-command', attachCommand: 'tmux -S /private/tmp/tmux-501/lazybox-fixture attach -t host-command-beta', usage: usage(97.5, 220, 3) },
    { id: 'host-orphan', kind: 'agent', backend: 'sessionhost', state: 'running', attached: false, hostPid: 4301, programPid: 4302, uptimeSeconds: '90061', label: 'codex · old-checkout', usage: usage(0, 96, 2) },
  ];
  // A `lazybox run` that went on in the background has no session host: the
  // Runner lists it from its command (backend "run", id "run:<command id>").
  const backgroundRuns = () => commands.filter(c => c.background && !c.done).map(c => { const session = sessions.find(s => s.id === c.sessionId); const workspace = collections.find(w => w.id === session?.collectionId); return { id: 'run:' + c.id, kind: 'command', backend: 'run', state: 'running', attached: true, hostPid: 0, programPid: c.pid || 0, uptimeSeconds: '42', label: c.title, workspaceId: workspace?.id || '', workspaceName: workspace?.name || '', sessionId: c.sessionId, conversationTitle: session?.title || '', provider: '', commandId: c.id, ports: c.ports || [], usage: usage(0.4, 64, 2) }; });
  const processInventory = () => ({
    processes: [...processes, ...backgroundRuns(), ...[...terminals.values()].filter(t => !t.done).map(t => ({ id: 'host-' + t.id, kind: 'terminal', backend: 'sessionhost', state: 'running', attached: true, hostPid: 4400, programPid: 4401, uptimeSeconds: '30', label: t.title, workspaceId: t.collectionId, workspaceName: collections.find(c => c.id === t.collectionId)?.name || '', terminalId: t.id, usage: usage(0.1, 8, 2) }))],
    hosting: { selected: runnerSettings.processBackend || processHosting.configured, ...processHosting },
  });

  // The workspace runtime (WORKSPACE_RUNTIME.md), explicitly fake: no script
  // runs. A tiny local page stands in for the dev server an action would start,
  // so a preview really loads; every other port is only a number.
  const previewServer = createServer((req, res) => {
    // The Web panel's fixture of a page that never answers (scripts/test-web-viewer.mjs).
    if (req.url.startsWith('/stall')) return;
    const page = req.url.startsWith('/about') ? 'About the fixture app' : 'Fixture app home';
    res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
    res.end(`<!doctype html><title>${page}</title><body style="font-family:sans-serif"><h1>${page}</h1><p>Served at ${req.url}</p><a href="/about">About</a></body>`);
  });
  await new Promise(resolve => previewServer.listen(0, '127.0.0.1', resolve));
  const previewPort = previewServer.address().port;
  const now = () => String(Date.now());
  // Runtime runs are kept apart from the conversation consoles' list, so the
  // other surface tests see the commands they always saw; their logs stream.
  const runtimeCommands = [];
  const power = createPowerFake({ frame, commands, runtimeCommands, sessions, mutations });
  const config = createConfigFake({ mutations });
  const items = createItemsFake({ collections, github });
  const extensions = await createExtensionsFake({ mutations });
  // GetTelemetry's answer for a request; a test sets its own (f.telemetry = body => report).
  let telemetry = () => ({ groups: [], contexts: [], recent: [] });
  const runCommand = (collectionId, origin, title, output) => { const command = { id: `${origin}-${collectionId}-${runtimeCommands.length}`, sessionId: '', collectionId, origin, title, mode: 'managed', done: false, offset: String(Buffer.byteLength(output)), output, argv: ['/bin/sh', '-lc', title] }; runtimeCommands.push(command); return command; };
  const runState = (command, extra = {}) => ({ state: 'running', commandId: command.id, startedAtUnixMillis: now(), lastLine: command.output.trim().split('\n').at(-1), ...extra });
  const finished = (command, state, extra = {}) => { command.done = true; Object.assign(command, extra.exitCode !== undefined ? { exitCode: extra.exitCode } : {}, extra.signal ? { signal: extra.signal } : {}); return { state, commandId: command.id, startedAtUnixMillis: String(Date.now() - 60000), ...extra }; };
  const apiConfig = base => `# yaml-language-server: $schema=https://lazybox.ai/schemas/workspace.v1.json
setup: npm ci
files:
  - .env
actions:
  dev:
    run: npm run dev -- --port $LAZYBOX_PORT
    default: true
    concurrent: false
    url: http://127.0.0.1:$LAZYBOX_PORT
  test:
    run: npm test
`;
  const webConfig = `setup: |
  pnpm install
  pnpm build:icons
actions:
  storybook:
    run: pnpm storybook -p $LAZYBOX_PORT_1
    port: 1
    url: http://127.0.0.1:$LAZYBOX_PORT_1
`;
  const runtimes = {};
  const runtimeFor = collectionId => {
    if (runtimes[collectionId]) return runtimes[collectionId];
    const index = collections.findIndex(c => c.id === collectionId);
    const base = index === 0 ? previewPort : index === 1 ? previewPort + 10 : 0;
    const runtime = { collectionId, ports: base ? { base, count: 10 } : undefined, sources: [], actions: [], setups: [], defaultAction: '' };
    if (collectionId === 'alpha' || collectionId === 'beta') {
      const apiSetup = runCommand(collectionId, 'setup', 'npm ci', 'added 812 packages in 9s\n');
      runtime.sources.push({ member: 'api', repositoryId: 'backend', path: '.lazybox/workspace.yaml', approval: 'approved', hash: `api-${collectionId}-1`, content: apiConfig(base), changed: [] });
      runtime.setups.push({ member: 'api', run: 'npm ci', source: 'repository', runState: finished(apiSetup, 'succeeded', { exitCode: 0 }), copiedFiles: ['.env'], missingFiles: [] });
      runtime.actions.push(
        { key: 'api/dev', member: 'api', name: 'dev', run: 'npm run dev -- --port $LAZYBOX_PORT', cwd: '', default: true, concurrent: false, url: `http://127.0.0.1:${base}`, port: base, source: 'repository', blocked: '', runState: { state: 'idle' }, runningElsewhere: [] },
        { key: 'api/test', member: 'api', name: 'test', run: 'npm test', cwd: '', default: false, concurrent: true, url: '', port: base, source: 'repository', blocked: '', runState: { state: 'idle' }, runningElsewhere: [] },
      );
      runtime.defaultAction = 'api/dev';
    }
    if (collectionId === 'alpha') {
      runtime.sources.push({ member: 'web', repositoryId: 'frontend', path: '.lazybox/workspace.yaml', approval: 'changed', hash: 'web-alpha-2', content: webConfig, changed: ['setup', 'action:storybook'] });
      runtime.setups.push({ member: 'web', run: 'pnpm install\npnpm build:icons', source: 'repository', pendingApproval: true, runState: { state: 'idle' }, copiedFiles: [], missingFiles: ['.env.local'] });
      runtime.actions.push({ key: 'web/storybook', member: 'web', name: 'storybook', run: 'pnpm storybook -p $LAZYBOX_PORT_1', cwd: '', default: false, concurrent: true, url: `http://127.0.0.1:${base + 1}`, port: base + 1, source: 'repository', blocked: 'Approve web’s scripts first', runState: { state: 'idle' }, runningElsewhere: [] });
    }
    if (collectionId === 'beta') {
      // Beta's dev server is already running: alpha's is non-concurrent with it.
      const dev = runCommand('beta', 'action', 'npm run dev -- --port $LAZYBOX_PORT', `beta dev listening on ${base}\n`);
      runtime.actions[0].runState = runState(dev);
    }
    runtimes[collectionId] = runtime;
    return runtime;
  };
  const isLiveAction = action => action.runState?.state === 'running' || action.runState?.state === 'stopping';
  const runtimeView = collectionId => {
    const runtime = runtimeFor(collectionId);
    for (const action of runtime.actions) action.runningElsewhere = action.concurrent ? [] : collections.filter(c => c.id !== collectionId && runtimes[c.id]?.actions.some(other => other.key === action.key && isLiveAction(other))).map(c => c.name);
    return structuredClone(runtime);
  };
  const stopAction = (collectionId, action) => {
    if (!isLiveAction(action)) return;
    const command = runtimeCommands.find(c => c.id === action.runState.commandId);
    action.runState = { ...action.runState, state: 'stopping' };
    setTimeout(() => { if (action.runState.state !== 'stopping') return; action.runState = { ...finished(command, 'stopped', { signal: 'SIGTERM' }), startedAtUnixMillis: action.runState.startedAtUnixMillis }; }, runtimeTiming.stopMs);
  };
  const runtimeTiming = { stopMs: 600 };
  // Ends a running setup, as its script would.
  const finishSetup = (collectionId, member, exitCode = 0) => { const setup = runtimeFor(collectionId).setups.find(s => s.member === member); const command = runtimeCommands.find(c => c.id === setup.runState.commandId); setup.runState = { ...finished(command, exitCode ? 'failed' : 'succeeded', { exitCode }), startedAtUnixMillis: setup.runState.startedAtUnixMillis }; };
  const startSetup = (collectionId, setup) => { const command = runCommand(collectionId, 'setup', setup.run.split('\n')[0], `$ ${setup.run.replaceAll('\n', '\n$ ')}\nresolving packages…\n`); setup.pendingApproval = false; setup.runState = runState(command); };
  // Beta's dev server runs from the start, as it would have before this window opened.
  runtimeFor('alpha'); runtimeFor('beta');
  let runtimeSettings = { revision: '1', portRangeStart: 20000, portRangeEnd: 29999, repositories: [] };
  const runtimeSettingsView = () => ({ ...runtimeSettings, repositories: library.map(repo => ({ repositoryId: repo.id, name: repo.name || repo.id, config: '', defaultAction: '', skipSetup: false, error: '', ...runtimeSettings.repositories.find(r => r.repositoryId === repo.id) })) });
  // Remote control (REMOTE.md): a fake Tailscale route, pairing offers checked
  // with the phone's real WebCrypto signatures, paired devices, and short
  // device credentials. deviceRPCs mirrors the Runner's device allowlist.
  const deviceRPCs = { Info: 'read', ListHistory: 'read', GetSession: 'read', ListCollections: 'read', GetCollection: 'read', ListWorkspaceChanges: 'read', ReadWorkspaceDiff: 'read', ListCommands: 'read', ReadCommand: 'read', ReadAttachment: 'read', ReadSessionTail: 'read', WatchEvents: 'read', WatchOutput: 'read', Apply: 'respond', PinConversation: 'respond', OpenSession: 'start', ResumeSession: 'start', GetPushSettings: 'read', SetPushSettings: 'read', SendTestPush: 'read', ListLinks: 'read', SendPullRequestFailures: 'respond', ImportPullRequestReview: 'respond' };
  // Web Push (REMOTE.md): the Runner's VAPID key (a real P-256 key, so a browser accepts it
  // as applicationServerKey), each device's subscription and preferences, and what was
  // pushed — payloads exactly as the Runner's notifier builds them.
  const vapid = (() => { const jwk = generateKeyPairSync('ec', { namedCurve: 'prime256v1' }).publicKey.export({ format: 'jwk' }); return Buffer.concat([Buffer.from([4]), Buffer.from(jwk.x, 'base64url'), Buffer.from(jwk.y, 'base64url')]).toString('base64url'); })();
  const pushDefaults = { needsInput: true, failed: true, review: false, quietStart: '', quietEnd: '', timeZone: '' };
  const remote = {
    phoneUrl: '', localUrl: 'http://127.0.0.1:4100', offerLifetimeMs: 300_000,
    tailscale: { state: 'available', dnsName: 'laptop.example.ts.net', httpsPort: 8443, enabled: false, detail: '' },
    offers: new Map(), devices: new Map(), challenges: new Map(), tokens: new Map(), deviceCalls: [], deviceDenied: [], pairRequests: [],
    vapid, pushes: [], pushFailure: '',
    /** Like the Runner: an approval answered (or a question) leaves the conversation's pending requests. */
    resolveOnAnswer: false,
    pushView(d, withKey) { const push = d.push || { preferences: { ...pushDefaults } }; return { ...(withKey ? { applicationServerKey: vapid } : {}), subscribed: !!push.subscription, preferences: push.preferences, service: push.subscription ? new URL(push.subscription.endpoint).host : '', ...(push.lastSentAt ? { lastSentAtUnixMillis: String(push.lastSentAt) } : {}), ...(push.lastError ? { lastError: push.lastError } : {}) }; },
    /** What the Runner's notifier would push to each subscribed device for this conversation. */
    notify(kind, session) {
      const wants = { approval: 'needsInput', question: 'needsInput', failed: 'failed', review: 'review' }[kind];
      const sent = [];
      for (const d of remote.devices.values()) {
        if (!d.push?.subscription || (wants && !d.push.preferences[wants])) continue;
        const payload = { title: (session.title || 'Untitled conversation').slice(0, 80), kind, url: `/?view=remote&session=${session.id}`, tag: session.conversationId || session.id };
        remote.pushes.push({ device: d.id, endpoint: d.push.subscription.endpoint, payload }); d.push.lastSentAt = Date.now(); sent.push(payload);
      }
      return sent;
    },
    access() { const t = remote.tailscale; return { localUrl: remote.localUrl, phoneUrl: t.enabled && t.state === 'serving' ? remote.phoneUrl : '', tailscale: { ...t } }; },
    state(offer) { if (offer.state === 'pending' && Date.now() >= Number(offer.expiresAtUnixMillis)) offer.state = 'expired'; return offer.state; },
    wireDevice(d) { return { id: d.id, name: d.name, grants: d.grants, pairedAtUnixMillis: String(d.pairedAt), lastSeenAtUnixMillis: String(d.lastSeenAt), fingerprint: d.fingerprint, push: remote.pushView(d, false) }; },
    wireOffer(offer) { const device = offer.device && remote.devices.get(offer.device); return { id: offer.id, state: remote.state(offer), expiresAtUnixMillis: offer.expiresAtUnixMillis, grants: offer.grants, ...(device ? { device: remote.wireDevice(device) } : {}) }; },
    deviceFor(credential) { const id = remote.tokens.get(credential); const d = id && remote.devices.get(id); if (d) d.lastSeenAt = Date.now(); return d; },
    /** Expire an offer now, as five minutes passing would. */
    expire(id) { const offer = remote.offers.get(id); if (offer) offer.expiresAtUnixMillis = String(Date.now() - 1); },
  };
  const remoteEndpoint = async (req, res) => {
    const reply = (status, value) => { res.writeHead(status, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' }); res.end(JSON.stringify(value)); };
    if (req.method !== 'POST' || !req.headers['content-type']?.startsWith('application/json')) return reply(405, { error: 'JSON POST only' });
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    let body; try { body = JSON.parse(Buffer.concat(chunks).toString()); } catch { return reply(400, { error: 'malformed request' }); }
    const verified = (key, message, signature) => { try { return verify('sha256', Buffer.from(message), { key: createPublicKey({ key: Buffer.from(key, 'base64url'), format: 'der', type: 'spki' }), dsaEncoding: 'ieee-p1363' }, Buffer.from(signature, 'base64url')); } catch { return false; } };
    const path = req.url.slice('/remote/v1/'.length);
    if (path === 'pair') {
      remote.pairRequests.push(body);
      const offer = remote.offers.get(body.pairing);
      if (!offer || offer.secret !== body.secret) return reply(403, { error: 'this pairing code is not valid; show a new one on your computer' });
      const state = remote.state(offer);
      if (state !== 'pending') return reply(403, { error: { paired: 'this pairing code was already used; show a new one on your computer', expired: 'this pairing code expired; show a new one on your computer', cancelled: 'this pairing code was cancelled on your computer' }[state] || 'this pairing code is not valid' });
      if (!verified(body.publicKey, `lazybox-device-pair-v1\0${body.pairing}\0${body.publicKey}`, body.signature)) return reply(403, { error: 'the device key did not sign this request' });
      const digest = createHash('sha256').update(Buffer.from(body.publicKey, 'base64url')).digest('hex').slice(0, 16).toUpperCase();
      const d = { id: randomUUID().slice(0, 12), name: (body.name || '').trim() || 'Phone', grants: offer.grants, publicKey: body.publicKey, pairedAt: Date.now(), lastSeenAt: Date.now(), fingerprint: digest.match(/.{4}/g).join(' ') };
      remote.devices.set(d.id, d); offer.state = 'paired'; offer.device = d.id;
      return reply(200, { device: { id: d.id, name: d.name, grants: d.grants, fingerprint: d.fingerprint } });
    }
    if (path === 'challenge') {
      if (!remote.devices.has(body.device)) return reply(401, { error: 'this device is not paired with this Runner', code: 'revoked' });
      const challenge = randomUUID(); remote.challenges.set(challenge, body.device);
      return reply(200, { challenge, expiresAt: Date.now() + 60_000 });
    }
    if (path === 'session') {
      const owner = remote.challenges.get(body.challenge); remote.challenges.delete(body.challenge);
      const d = remote.devices.get(body.device);
      if (!d) return reply(401, { error: 'this device is not paired with this Runner', code: 'revoked' });
      if (owner !== body.device || !verified(d.publicKey, `lazybox-device-session-v1\0${body.device}\0${body.challenge}`, body.signature)) return reply(401, { error: 'the device key did not sign this request' });
      const credential = `lbx-device.${d.id}.${randomUUID()}`; remote.tokens.set(credential, d.id); d.lastSeenAt = Date.now();
      return reply(200, { token: credential, expiresAt: Date.now() + 900_000, device: { id: d.id, name: d.name, grants: d.grants, fingerprint: d.fingerprint } });
    }
    return reply(404, { error: 'unknown endpoint' });
  };
  const handle = async (req, res) => {
    // A desktop window calls the Runner's own origin (no proxy), as the native app does.
    if (req.headers.origin) {
      res.setHeader('Access-Control-Allow-Origin', req.headers.origin);
      res.setHeader('Access-Control-Allow-Headers', req.headers['access-control-request-headers'] || 'authorization, content-type, connect-protocol-version, connect-timeout-ms');
      res.setHeader('Access-Control-Allow-Methods', 'POST');
      res.setHeader('Access-Control-Expose-Headers', '*');
      if (req.method === 'OPTIONS') { res.writeHead(204); res.end(); return; }
    }
    // The device gateway's pairing and sign-in endpoints (REMOTE.md), checked as the Runner checks them.
    if (req.url.startsWith('/remote/v1/')) return remoteEndpoint(req, res);
    let device, extension;
    if (req.headers.authorization?.startsWith('Bearer lbx-ext.')) {
      // An extension panel's credential (EXTENSIONS.md): only what its grants allow, as the Runner checks it.
      const method = req.url.split('/').at(-1);
      extension = extensions.scope(req.headers.authorization.slice(7));
      if (!extension) { res.writeHead(401, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ code: 'unauthenticated', message: 'not a live extension credential' })); return; }
      const chunks = []; for await (const chunk of req) chunks.push(chunk);
      req.extensionBody = Buffer.concat(chunks);
      let parsed = {}; try { parsed = JSON.parse(req.extensionBody.toString() || '{}'); } catch { /* refused below */ }
      extensions.calls.push({ method, extension: extension.id });
      if (!extensionAllows(extension.grants, method, parsed)) { extensions.denied.push(method); res.writeHead(403, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ code: 'permission_denied', message: 'this extension was not given that' })); return; }
    } else
    if (req.headers.authorization?.startsWith('Bearer lbx-device.')) {
      const method = req.url.split('/').at(-1);
      device = remote.deviceFor(req.headers.authorization.slice(7));
      remote.deviceCalls.push({ method, device: device?.id || null });
      if (!device) { res.writeHead(401, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ code: 'unauthenticated', message: "a paired device's credential is required" })); return; }
      const grant = deviceRPCs[method];
      if (!grant || !device.grants.includes(grant)) { remote.deviceDenied.push(method); res.writeHead(403, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ code: 'permission_denied', message: 'this device is not allowed to do that' })); return; }
    } else
    if (req.headers.authorization !== `Bearer ${token}`) { runner.refusedBearers.push({ bearer: req.headers.authorization ?? null, method: req.url.split('/').at(-1) }); res.writeHead(401, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ code: 'unauthenticated', message: 'local test bench bearer required' })); return; }
    if (runner.down) { res.writeHead(503, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ code: 'unavailable', message: 'Fixture Runner is restarting' })); return; }
    const method = req.url.split('/').at(-1); let body;
    try {
      const chunks = []; if (!req.extensionBody) for await (const chunk of req) chunks.push(chunk);
      let data = req.extensionBody || Buffer.concat(chunks);
      if (req.headers['content-type']?.startsWith('application/connect+json')) data = data.subarray(5, 5 + data.readUInt32BE(1));
      body = JSON.parse(data.toString() || '{}');
    }
    catch { res.writeHead(400); res.end(); return; }
    calls.push({ method, body, ...(device ? { device: device.id } : {}), ...(extension ? { extension: extension.id } : {}) });
    if (device && method === 'Apply' && body.permission) { remote.deviceDenied.push('Apply:permission'); res.writeHead(403, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ code: 'permission_denied', message: 'this device is not allowed to do that' })); return; }
    const json = value => { res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(device && method === 'Info' ? { ...value, profiles: value.profiles.map(({ id, provider, label, managed, isolation }) => ({ id, provider, label, managed, isolation })), pendingEnrollments: [], gateways: [], restartAvailable: false } : value)); };
    const error = (code, message) => { res.writeHead(400, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ code, message })); };
    if (method.includes('Artifact')) { if (artifactStore.handle(method, body, json, error) === false) error('unimplemented', `Unsupported fixture RPC ${method}`); return; }
    switch (method) {
      // The Runner's version and which lazybox agents' shells reach (Settings → About).
      case 'Version': return json(versionInfo);
      case 'GetClaudeAuth': return json(claudeAuthReply());
      case 'RemoveSubscription': {
        const i=profiles.findIndex(p=>p.id===body.profileId);if(i<0)return error('not_found','no such subscription');
        if(!profiles[i].managed)return error('failed_precondition','only a subscription saved in Lazybox can be removed here');
        mutations.push({method,body});profiles.splice(i,1);
        return json({restartAvailable:runner.restartAvailable,instanceId:runner.instanceId,profiles,sessions});
      }
      case 'SetClaudeApiKey': {
        mutations.push({method,body:{...body,apiKey:body.apiKey?'(a key)':''}});
        if(body.clear){claudeAuth.key='';return json(claudeAuthReply());}
        const key=(body.apiKey||'').trim();
        if(key.startsWith('sk-ant-oat'))return error('invalid_argument','that is a Claude subscription token, not an API key; Lazybox does not store subscription credentials (create a key at platform.claude.com)');
        if(!/^sk-ant-[\w-]{8,}$/.test(key))return error('invalid_argument','that is not an Anthropic API key (it starts with sk-ant-)');
        claudeAuth.key=key;return json(claudeAuthReply());
      }
      case 'GetRunnerSettings': return json({...runnerSettings,automaticSwitchingProviders:await extensions.switching(),review:{reviewer:'',template:'',autoStart:false,...runnerSettings.review,defaultTemplate:'You are an independent reviewer ({{reviewer}}) of work {{author}} did.\n\n{{goal}}\n\n## What to review: {{scope}}\n\n{{diff}}\n\n{{report}}'}});
      case 'UpdateReviewRun': {
        mutations.push({method,body});const run=reviewOf(body.collectionId).runs.find(r=>r.id===body.reviewId);if(!run)return error('not_found','review not found');
        const ended=()=>error('failed_precondition','this review\'s loop has ended');
        switch(body.action){
          case 'pause':if(!active(run))return ended();run.paused=true;loopNote(run,'paused','You','Paused: no re-review or automatic send until you resume.');break;
          case 'resume':if(!active(run))return ended();run.paused=false;loopNote(run,'resumed','You','Resumed.');break;
          case 'stop':if(active(run))stopLoop(body.collectionId,run,'stopped','You stopped the loop.',false);break;
          case 'take_over':if(active(run))stopLoop(body.collectionId,run,'taken_over','You took over: the loop stopped; the conversations are yours.',false);break;
          case 'loop':run.loop=body.loop||'ask';if(body.maxRounds)run.maxRounds=Math.max(run.round,Math.min(10,body.maxRounds));loopNote(run,'mode','You',`Loop: ${run.loop}, up to ${plural(run.maxRounds,'round','rounds')}.`);break;
          default:return error('failed_precondition',`unknown review control ${body.action}`);
        }
        return json(reviewReply(body.collectionId));
      }
      case 'StartReview': {
        mutations.push({method,body});
        const collection=collections.find(c=>c.id===body.collectionId);if(!collection)return error('not_found','Workspace missing');
        const source=sessions.find(s=>s.id===body.sourceSessionId);if(!source||source.collectionId!==collection.id)return error('failed_precondition','that conversation works in another workspace');
        if(reviewOf(collection.id).runs.some(run=>run.reviewerSessionId===source.id))return error('failed_precondition','this conversation is itself a review; review the conversation it reviews instead');
        // Who reviews, as the Runner decides (review_runs.go reviewerProfile): the one named, else the
        // setting: the other agent (default), the same one, or a fixed one; one not installed gives way, said in `fallback`.
        const authorProvider=providerOf(source),setting=runnerSettings.review||{},ready=provider=>(availability[provider]?.installed??true)&&profiles.some(p=>p.provider===provider);
        const mode=setting.reviewerMode||(setting.reviewer?'fixed':'other'),configured=profiles.find(p=>p.id===setting.reviewer);
        let profile,fallback='',model=body.model||'';
        if(body.profileId)profile=profiles.find(p=>p.id===body.profileId);
        else if(body.provider){if(!ready(body.provider))return error('failed_precondition',`${providerName(body.provider)} is not available on this Runner (not installed, or not signed in)`);profile=profiles.find(p=>p.provider===body.provider);}
        else if(mode==='fixed'&&configured&&ready(configured.provider)){profile=configured;model=model||setting.reviewerModel||'';}
        else{const wanted=mode==='same'?authorProvider:authorProvider==='claude'?'codex':'claude',stand=[wanted,authorProvider,'claude','codex'].find(ready);profile=stand&&profiles.find(p=>p.provider===stand);
          const missing=mode==='fixed'?(configured?providerName(configured.provider)+(configured.label?` · ${configured.label}`:''):'The reviewer you chose'):'';
          if(profile&&(missing||stand!==wanted))fallback=`${missing||providerName(wanted)} is not available, so ${stand===authorProvider?`a fresh ${providerName(stand)} conversation reviews`:`${providerName(stand)} reviews`}.`;}
        if(!profile)return error('failed_precondition',`subscription ${body.profileId||body.provider} is not signed in on this Runner`);
        const scope=body.scope||'changes';let scopeLabel;
        if(scope==='turn'){const turn=checkpointState(source.id).turns.find(t=>t.turn===body.turn);if(!turn)return error('failed_precondition',`this conversation has no turn ${body.turn}`);scopeLabel=`Turn ${body.turn}`;}
        else if(scope==='pr'){const pr=shipState(collection.id,body.member).pr;if(!pr)return error('failed_precondition','this branch has no pull request to review');scopeLabel=`PR #${pr.number}`;}
        else scopeLabel=!body.compare?'Changes':body.compare===':uncommitted'?'Uncommitted changes':`Changes vs ${body.compare}`;
        const label=providerName(profile.provider),title=`Review · ${scopeLabel} · ${label}`;
        const reviewer={id:`review-${sessions.length}`,collectionId:collection.id,workId:collection.id,profileId:profile.id,provider:profile.provider,model,permission:'read-only',title,status:'running',busy:true,capabilities:{files:true},pending:[]};sessions.push(reviewer);
        const now=String(Math.floor(Date.now()/1000));
        const loop=runnerSettings.review?.loop||'ask',maxRounds=Math.min(10,Math.max(1,runnerSettings.review?.maxRounds||3));
        const run={loop,maxRounds,round:1,phase:'reviewing',paused:false,stopReason:'',rounds:[{number:1,trigger:'start',startedAtUnix:now}],timeline:[],id:'rv-'+randomUUID().slice(0,8),reviewerSessionId:reviewer.id,reviewerTitle:title,reviewerLabel:label,reviewerProvider:profile.provider,reviewerProfileId:profile.id,sourceSessionId:source.id,sourceTitle:source.title||'',sourceLabel:providerName(authorProvider),sourceProvider:authorProvider,scope,scopeLabel,turn:body.turn||0,compare:body.compare||'',state:'running',detail:`${label} is reviewing ${scopeLabel.toLowerCase()}.${fallback?' '+fallback:''}`,fallback,focus:(body.focus||'').trim(),createdAtUnix:now,updatedAtUnix:now};
        reviewOf(collection.id).runs.push(run);
        loopNote(run,'round_started','Lazybox',`Round 1: ${label} reviews ${scopeLabel.toLowerCase()} (${loop==='off'?'one pass':loop==='auto'?`up to ${maxRounds} rounds; findings go to ${run.sourceLabel} automatically`:`up to ${maxRounds} rounds; you send findings between rounds`}).`);
        reviewBriefs.set(reviewer.id,`You are an independent reviewer (${label}) of work ${run.sourceLabel} did in this workspace, in the conversation “${run.sourceTitle}”.\n\n## What to review: ${scopeLabel}\n\nlazybox review add --file <repository>/<path> --line <N> --severity blocker|major|minor|nit --title "…"${run.focus?`\n\n## What the person asked you to focus on\n\n> ${run.focus}`:''}`);
        // The brief is the reviewer's first message, journaled as the Runner does.
        openEvents(reviewer.id).push({type:'operation_accepted',text:reviewBriefs.get(reviewer.id),operationId:'review-brief-'+run.id,providerDataJson:originData({kind:'review_brief',summary:`Asked ${label} to review ${run.sourceLabel}'s work`,detail:scopeLabel})});
        reviewEvent('review_started',collection.id,run);
        return json({run:wireRun(collection.id,run),session:reviewer});
      }
      case 'SaveRunnerSettings': {
        if(body.revision!==runnerSettings.revision)return error('aborted','Settings changed');
        mutations.push({method,body});const {defaultTemplate:_,...review}=body.review||runnerSettings.review||{};
        // The request is the whole of what it saves, as the Runner reads it: proto3 JSON leaves out a field at its zero
        // value (an empty list, false, ''), which is saved as that zero value, never the previous one kept.
        const zero={naming:'',permission:'',context:'',claudeContext:'',codexContext:'',processBackend:'',rotationExcludedProfileIds:[],claudeAiConnectors:false,plainTitles:false,claudeSubscriptionSignIn:false,notices:[]};
        // A save never adds a notice; it dismisses the ones it leaves out. The switching providers are the Runner's own.
        const {automaticSwitchingProviders:__,...saved}=body;const notices=(runnerSettings.notices||[]).filter(n=>(saved.notices||[]).includes(n));
        // Carried over by the migration only while it stays on, as the Runner keeps it.
        const claudeSignInCarriedOver=!!runnerSettings.claudeSignInCarriedOver&&!!saved.claudeSubscriptionSignIn;
        runnerSettings={...runnerSettings,...zero,...saved,notices,claudeSignInCarriedOver,review,revision:String(Number(runnerSettings.revision)+1)};return json({...runnerSettings,automaticSwitchingProviders:await extensions.switching()});
      }
      case 'SetModelPrice': {
        if(body.revision!==runnerSettings.revision)return error('aborted','Settings changed');
        mutations.push({method,body});runnerSettings.prices=runnerSettings.prices.filter(p=>p.provider!==body.price.provider||p.model!==body.price.model);
        if(!body.remove)runnerSettings.prices.push({...body.price,version:'custom-fixture',custom:true});runnerSettings.revision=String(Number(runnerSettings.revision)+1);return json(runnerSettings);
      }
      case 'ListThemes': return json(themeReply());
      case 'CreateTheme': {
        const name=body.name||'';if(!/^[A-Za-z0-9 ._-]{1,64}$/.test(name)||name.startsWith('.'))return error('invalid_argument','theme name: 1-64 letters, digits, spaces, ".", "_" or "-", not starting with "."');
        const file=name+'.toml';if(themeFolder.files.some(f=>f.name.toLowerCase()===file.toLowerCase()))return error('already_exists',`theme ${file} already exists`);
        mutations.push({method,body});themeFolder.files.push({name:file,format:'toml',json:'',error:'',modifiedUnixMillis:now()});themeFolder.files.sort((x,y)=>x.name<y.name?-1:x.name>y.name?1:0);
        return json({folder:themeReply(),file,path:themeFolder.directory+'/'+file});
      }
      case 'ListRepositoryDiscovery': return json(discovery);
      case 'RemoveRepository': {mutations.push({method,body});const index=library.findIndex(repo=>repo.id===body.repositoryId);if(index>=0)library.splice(index,1);for(const root of discovery.roots)root.repositories=root.repositories.filter(repo=>repo.gitIdentity!==body.gitIdentity);return json(discovery);}
      case 'ScanRepositoryRoot': { mutations.push({ method, body }); const root = { path: body.path, scannedAt: '1', repositories: [...['api', 'web', 'tools'].map(name => ({ name, path: body.path + '/' + name, gitIdentity: 'github:team/' + name })), { name: 'api-other-clone', path: body.path + '/other-clone', gitIdentity: 'github:team/api' }, { name: 'api-feature-worktree', path: body.path + '/api-feature', gitIdentity: 'github:team/api' }] }; discovery.roots = [...discovery.roots.filter(item => item.path !== body.path), root]; return json(discovery); }
      case 'GetGitHubConnection': return json(github);
      case 'ConnectGitHubPersonalToken': { mutations.push({ method, body }); if (body.token !== 'ghp_offline_fixture') // The real Runner reports a rejected GitHub credential as failed_precondition;
        // unauthenticated is reserved for a refused Runner bearer.
        return error('failed_precondition', 'Token rejected by fixture'); Object.assign(github, { state: 'connected', authMethod: 'personal_token', login: 'fixture-user', userId: '42', error: '' }); return json(github); }
      case 'ConfigureGitHubApp': mutations.push({ method, body }); Object.assign(github, { state: 'not_connected', clientId: body.clientId, appSlug: body.slug }); return json(github);
      case 'BeginGitHubSignIn': {
        mutations.push({ method, body });
        if (!githubFlows.has(body.requestKey)) githubFlows.set(body.requestKey, { id: 'github-' + body.requestKey, state: 'awaiting_login', userCode: 'TEST-CODE', signInUrl: 'https://github.com/login/device' });
        github.pendingSignIn = githubFlows.get(body.requestKey); return json(github.pendingSignIn);
      }
      case 'GetGitHubSignIn': return json([...githubFlows.values()].find(flow => flow.id === body.id) || { state: 'expired' });
      case 'CancelGitHubSignIn': { mutations.push({ method, body }); const flow = [...githubFlows.values()].find(flow => flow.id === body.id); Object.assign(flow, { state: 'cancelled', userCode: '', signInUrl: '' }); github.pendingSignIn = undefined; return json(flow); }
      case 'DisconnectGitHub': mutations.push({ method, body }); Object.assign(github, { state: github.clientId ? 'not_connected' : 'setup_required', authMethod: '', login: '', userId: '0', error: '' }); return json(github);
      case 'ListGitHubInstallations': return json({ installations: [{ id: '7', account: 'fixture-team', repositorySelection: 'selected' }], hasMore: false });
      case 'ListGitHubRepositories': return json({ repositories: githubRepositories.map(repo => ({ ...repo, registeredId: library.find(item => item.githubId === repo.id)?.id || '' })), hasMore: false });
      case 'ImportGitHubRepository': { mutations.push({ method, body }); let repo = library.find(item => item.githubId === body.repositoryId); if (!repo) { const remote = githubRepositories.find(item => item.id === body.repositoryId); repo = { id: 'github-' + remote.id, name: remote.name, path: '/fixture/repos/' + remote.id, sourceUrl: remote.htmlUrl, githubId: remote.id,gitIdentity:'github:'+remote.fullName.toLowerCase() }; library.push(repo); } return json({ repository: repo }); }
      case 'ListGitHubItems': return json({ items: [{ id: '501', number: '12', kind: 'issue', title: 'Fix startup', state: 'open', author: 'fixture-user', htmlUrl: 'https://github.com/fixture-team/api/issues/12' }, { id: '502', number: '13', kind: 'pull_request', title: 'Improve startup', state: 'open', author: 'fixture-user', htmlUrl: 'https://github.com/fixture-team/api/pull/13' }], hasMore: false });
      case 'ListSnippets': return json({snippets:[{key:'review',category:'Review',description:'Review this change',body:'Review the current change carefully.',origin:'built-in'},{key:'test',category:'Testing',description:'Run tests',body:'Run the relevant tests.',origin:'global'}]});
      case 'GetTelemetry': return json(telemetry(body));
      case 'RenameCollection': { const workspace=collections.find(c=>c.id===body.collectionId);if(!workspace)return error('not_found','workspace missing');workspace.name=body.name;mutations.push({method,body});return json({collection:workspace}); }
      case 'RestartRunner': {
        mutations.push({ method, body });
        if (!runner.restartAvailable) return error('failed_precondition', 'this Runner was not started by a launcher that can start it again; restart it where it runs');
        if (runner.restartError) return error('internal', runner.restartError);
        const stopping = runner.instanceId; runner.restarts++;
        setTimeout(() => { runner.down = true; setTimeout(() => { runner.instanceId = randomUUID(); runner.down = false; }, runner.restartMs); }, 50);
        return json({ instanceId: stopping });
      }
      case 'GetProviderAvailability': {
        if (device) return error('permission_denied', 'this device is not allowed to do that');
        if (body.refresh) availability.refreshes++;
        return json(availabilityReport());
      }
      case 'Info': return json({ restartAvailable: runner.restartAvailable, instanceId: runner.instanceId, profiles, pendingEnrollments: [...enrollments.values()].filter(flow => flow.state === 'awaiting_login').map(({ id, state, provider, label, profileId }) => ({ id, state, provider, label, profileId })), sessions, attachmentLimits: { maxFiles: 4, maxFileBytes: '20971520', maxMessageBytes: '20971520' } });
      case 'AddSubscription': {
        mutations.push({ method, body });
        const id = `enrollment-${body.requestKey}`;
        // Codex signs in with the browser unless a device code is asked for (signInMethod "device").
        if (!enrollments.has(id)) { const device = body.provider === 'codex' && body.signInMethod === 'device'; enrollments.set(id, { id, state: 'awaiting_login', profileId: `saved-${enrollments.size}`, provider: body.provider, label: body.label, signInMethod: device ? 'device' : 'browser', signInUrl: body.provider === 'claude' ? 'https://claude.com/cai/oauth/authorize?state=fixture' : device ? 'https://auth.openai.com/codex/device' : 'https://auth.openai.com/oauth/authorize?state=fixture', userCode: device ? 'AUTH-FIXTURE-CODE' : '' }); }
        return json(enrollments.get(id));
      }
      case 'GetSubscriptionEnrollment': { const id=body.enrollmentId || `enrollment-${body.requestKey}`; return enrollments.has(id) ? json(enrollments.get(id)) : json({id, state:'expired',error:'The sign-in took too long and was stopped. Try again when you’re ready.'}); }
      case 'CancelSubscriptionEnrollment': { const flow = enrollments.get(body.enrollmentId); if (!flow) return error('not_found', 'Unknown enrollment'); flow.state = 'cancelled'; flow.userCode = ''; flow.signInUrl = ''; return json(flow); }
      case 'RenameConversation': {const session=sessions.find(s=>s.id===body.sessionId);if(!session)return error('not_found','Missing conversation');session.title=body.title;mutations.push({method,body});return json({session});}
      // Pins and archive marks are the conversation's, kept by the Runner (a pin keeps its first time).
      case 'PinConversation': case 'ArchiveConversation': {
        const session=sessions.find(s=>s.id===body.sessionId);if(!session)return error('not_found','Missing conversation');
        const field=method==='PinConversation'?'pinnedAtUnixMillis':'archivedAtUnixMillis', on=method==='PinConversation'?body.pinned:body.archived;
        if(!on)delete session[field];else if(!Number(session[field]))session[field]=String(Date.now());
        pins.push({method,body});return json({session});
      }
      case 'ListHistory': return json({sessions:sessions.filter(s=>(body.collectionId===undefined||s.collectionId===body.collectionId)&&(!body.search||(s.title||`Conversation ${s.id}`).toLowerCase().includes(body.search.toLowerCase()))).map(s=>({...s,title:s.title||`Conversation ${s.id}`,conversationId:s.workId||s.id})),hasMore:false});
      case 'RefreshSubscription': { const profile = profiles.find(p => p.id === body.profileId); if (!profile) return error('not_found', 'Unknown profile'); if (profile.provider === 'claude') profile.availability = {state:'available',plan:'max',windows:[{name:'five_hour',usedPercent:37,durationMinutes:'300',resetAt:String(Math.floor(Date.now()/1000)+3600)}]}; profile.availability.observedAtUnixMillis = String(Date.now()); profile.availability.windows[0].usedPercent += 5; return json(profile); }
      case 'ListCollections': { if (runner.listDelayMs) await new Promise(resolve => setTimeout(resolve, runner.listDelayMs)); return json({ collections: arranged() }); }
      // The person's order, as the Runner keeps it (collections/arrangement.go): new ones first, then the kept order.
      case 'ArrangeWorkspaces': { if (runner.arrangeError) return error('unavailable', runner.arrangeError); mutations.push({method,body}); arrangement.splice(0, arrangement.length, ...new Set((body.collectionIds || []).filter(id => collections.some(c => c.id === id)))); return json({ collections: arranged() }); }
      case 'GetCollection': { const collection = collections.find(c => c.id === body.collectionId); return collection ? json({ collection }) : error('not_found', 'Workspace missing'); }
      case 'AddLocalRepository': {
        mutations.push({ method, body });
        let repository = library.find(repo => repo.path === body.path);
        if (!repository) { repository = { id: 'local-' + (library.length + 1), name: body.name || body.path.split('/').at(-1), path: body.path, gitIdentity: discovery.roots.flatMap(root => root.repositories).find(repo => repo.path === body.path)?.gitIdentity || '' }; library.push(repository); }
        return json({ repository });
      }
      case 'GetRepositoryCatalog': {if(body.refresh&&github.state==='connected'){catalogRemote=structuredClone(githubRepositories);catalogUpdated=String(Math.floor(Date.now()/1000));}return json({entries:catalogEntries(),sources:[{id:'github',state:github.state,detail:github.authMethod==='github_app'?'GitHub App access is limited to its installed repositories.':'Repositories visible to this personal token.',updatedAt:catalogUpdated}],refreshSeconds:300});}
      case 'PrepareWorkspace': {
       mutations.push({method,body});const values=[];const members=[];
       for(const identity of body.repositoryIdentities||[]){const choice=catalogEntries().find(r=>r.identity===identity);if(!choice)return error('not_found','Unknown repository identity');let repo=library.find(r=>r.id===choice.repositoryId);if(!repo){if(preparationFailures.delete(identity)){res.writeHead(200,{'Content-Type':'application/connect+json'});res.end(Buffer.concat([...values.map(v=>frame(v)),frame({error:{code:'failed_precondition',message:'Fixture preparation failure'}},2)]));return;}const remote=githubRepositories.find(r=>'github:'+r.fullName.toLowerCase()===identity);repo={id:remote?'github-'+remote.id:'local-'+(library.length+1),name:choice.name.split('/').at(-1),path:choice.path||'/fixture/repos/'+remote.id,gitIdentity:identity};library.push(repo);prepared.push(identity);}values.push({identity,message:'Preparing '+choice.name});members.push({repositoryId:repo.id,name:repo.name||repo.id,...(body.branch&&!body.collectionId?{branch:body.branch}:{})});}
       let collection=requests.get(body.requestKey);if(!collection){if(body.collectionId){collection=collections.find(c=>c.id===body.collectionId);if(!collection)return error('not_found','Missing workspace');collection.members.push(...members.map(spec=>({spec,path:collection.root+'/'+spec.name,ready:true})));}else{const id='created-'+collections.length;collection={id,name:body.name,...(body.item?{origin:{provider:body.item.provider||'github',key:body.item.key}}:{}),root:'/fixture/collections/'+id,state:'ready',members:members.map(spec=>({spec,path:'/fixture/collections/'+id+'/'+spec.name,ready:true}))};collections.push(collection);}requests.set(body.requestKey,collection);}values.push({message:'Ready',collection});
       // Answering an agent's request for a repository: the request is resolved (the agent told)
       // once the repository is in the workspace, as the Runner does, not when preparation starts.
       const answer=()=>{if(!body.repositoryRequestId)return;
        // The asking agent is told, as the Runner tells it: a message with its origin, shown as an event.
        const asker=sessions.find(s=>(s.pending||[]).some(p=>p.requestId===body.repositoryRequestId)),request=asker?.pending.find(p=>p.requestId===body.repositoryRequestId);
        const asked=request?JSON.parse(Buffer.from(request.providerDataJson,'base64').toString()):{};
        for(const s of sessions)s.pending=(s.pending||[]).filter(p=>p.requestId!==body.repositoryRequestId);repositoryRequests.set(body.repositoryRequestId,{state:'added',collection:collection.id,identities:body.repositoryIdentities});
        if(asker){const added=members.map(spec=>({name:spec.name,path:collection.root+'/'+spec.name,branch:asked.branch||''}));
         eventStreams.get(asker.id)?.push({type:'operation_accepted',operationId:'repository-note-'+body.repositoryRequestId,text:`[Lazybox] You asked to add ${asked.query||added[0]?.name} to this workspace. The person added it:\n${added.map(a=>`- ${a.name} at ${a.path} (branch ${a.branch})`).join('\n')}\nContinue with what you needed it for.`,
          providerDataJson:Buffer.from(JSON.stringify({files:[],origin:{kind:'repository_added',summary:`Added ${added.map(a=>a.name).join(', ')} to this workspace`,detail:asked.branch||'',copy:added.map(a=>a.path).join('\n')}})).toString('base64')});}
       };
       res.writeHead(200,{'Content-Type':'application/connect+json'});
       // preparationDelayMs streams each step apart, as a clone does.
       if(runner.preparationDelayMs){for(const value of values){res.write(frame(value));await new Promise(resolve=>setTimeout(resolve,runner.preparationDelayMs));}answer();res.end(frame({},2));return;}
       answer();res.end(stream(values));return;
      }
      case 'DeclineRepositoryRequest': {
       mutations.push({method,body});const pending=sessions.flatMap(s=>s.pending||[]).find(p=>p.requestId===body.requestId);if(!pending)return error('failed_precondition','This request was already answered');
       for(const s of sessions)s.pending=(s.pending||[]).filter(p=>p.requestId!==body.requestId);repositoryRequests.set(body.requestId,{state:'declined',reason:body.reason||''});
       return json({id:body.requestId,state:'declined',declineReason:body.reason||''});
      }
      // An agent's request to start a conversation (runner/conversation_starts.go): Start opens a
      // peer conversation (in a new workspace when asked), Decline drops the request.
      case 'AnswerConversationStart': {
       mutations.push({method,body});const asker=sessions.find(s=>(s.pending||[]).some(p=>p.requestId===body.requestId));const pending=asker?.pending.find(p=>p.requestId===body.requestId);
       if(!pending)return error('failed_precondition','this request was already answered');
       const asked=JSON.parse(Buffer.from(pending.providerDataJson,'base64').toString());
       for(const s of sessions)s.pending=(s.pending||[]).filter(p=>p.requestId!==body.requestId);
       if(body.decision==='decline'){repositoryRequests.set(body.requestId,{state:'declined',reason:body.reason||''});return json({...asked,state:'declined',declineReason:body.reason||''});}
       const mode=body.workspace||asked.workspace||'same';let collection=collections.find(c=>c.id===asked.collectionId);
       if(mode==='new'){const id='created-'+collections.length;collection={...structuredClone(collection),id,name:body.title||asked.title,root:'/fixture/collections/'+id,ownerExecutionId:''};collections.push(collection);}
       const title=body.title||asked.title;const started={id:`started-${sessions.length}`,collectionId:collection.id,workId:collection.id,profileId:body.profileId||asked.profileId,title,capabilities:{files:true},status:'ready',pending:[]};sessions.push(started);
       if(runner.journalMessages)openEvents(started.id);
       repositoryRequests.set(body.requestId,{state:'started',session:started.id,title,brief:body.brief||asked.brief,workspace:mode,profileId:started.profileId,model:body.model||asked.model});
       return json({...asked,title,workspace:mode,state:'started',startedSessionId:started.id,startedConversationId:started.id,startedCollectionId:collection.id,startedTitle:title});
      }
      case 'ListRepositories': return json({ repositories: library.map(r => homes.has(r.id) ? { ...r, homePath: homes.get(r.id).path, homeKind: 'configured' } : r) });
      case 'ListRepositoryHomes': return json({ root: homeRoot, homes: library.filter(r => !body.repositoryIds?.length || body.repositoryIds.includes(r.id)).map(homeView) });
      case 'SetRepositoryHome': {
        mutations.push({ method, body }); const repo = library.find(r => r.id === body.repositoryId); if (!repo) return error('not_found', 'repository missing');
        if (body.action === 'clear') homes.delete(repo.id);
        else if (body.action === 'create') { const path = body.path || homeView(repo).suggestedPath; if ([...homes.values()].some(h => h.path === path)) return error('failed_precondition', 'that folder already exists: choose “Use an existing folder…” to adopt it, or another path'); homes.set(repo.id, { path }); }
        else if (body.action === 'adopt') { if (!String(body.path || '').startsWith('/') && !String(body.path || '').startsWith('~/')) return error('failed_precondition', 'a home checkout needs an absolute path'); if (/collections/.test(body.path)) return error('failed_precondition', "a home checkout cannot be inside Lazybox's own folders (its workspaces and its Runner state)"); homes.set(repo.id, { path: body.path.replace(/^~\//, '/fixture/home/') }); }
        else return error('failed_precondition', 'action must be create, adopt or clear');
        return json(homeView(repo));
      }
      case 'UpdateRepositoryHome': {
        mutations.push({ method, body }); const repo = library.find(r => r.id === body.repositoryId), h = homes.get(body.repositoryId); if (!repo || !h) return error('failed_precondition', 'this repository has no home checkout yet: create one in Settings → Repositories');
        if (h.dirty) return json({ home: homeView(repo), updated: false, message: 'Fetched origin. Not updated: main has uncommitted changes; commit or stash them yourself first.' });
        if (!h.behind) return json({ home: homeView(repo), updated: false, message: `Already at the latest origin/main (${h.head || 'abc1234'}).` });
        h.behind = 0; h.head = 'def5678';
        return json({ home: homeView(repo), updated: true, message: 'Updated main to the latest origin/main (abc1234 → def5678).' });
      }
      case 'OpenRepositoryHome': { mutations.push({ method, body }); if (!homes.has(body.repositoryId)) return error('failed_precondition', 'this repository has no home checkout yet: create one in Settings → Repositories'); return json({}); }
      case 'ResolveWorkspacePath': {const files=treeFiles(body.collectionId),folders=treeFolders(body.collectionId),path=String(body.path||'').replace(/^\.\//,'');const members=collections.find(c=>c.id===body.collectionId)?.members.map(m=>m.spec.name)||[];
        const exists=p=>files.includes(p)||folders.has(p),match=p=>({path:p,directory:folders.has(p)});
        if(/missing/.test(path))return json({matches:[]});
        if(exists(path))return json({matches:[match(path)],exact:true});
        const under=members.map(m=>m+'/'+path).filter(exists);if(under.length)return json({matches:under.map(match),exact:true});
        const name=path.split('/').pop();const named=files.filter(p=>p.split('/').pop()===name);if(named.length)return json({matches:named.map(match)});
        return json({matches:[{path,directory:false}],exact:true});}
      case 'SearchWorkspaceFiles': {const files=treeFiles(body.collectionId);const found=body.query?files.map(p=>fuzzy(p,body.query)).filter(Boolean).sort((a,b)=>b.score-a.score||a.path.localeCompare(b.path)).slice(0,body.limit||50):[];return json({matches:found.map(({path,positions})=>({path,positions})),indexed:files.length});}
      case 'ReadWorkspaceFile': {const key=body.collectionId+'/'+body.path; if(treeFolders(body.collectionId).has(body.path))return json({path:body.path,directory:true,entries:listFolder(body.collectionId,body.path)}); if(!workspaceFiles.has(key))workspaceFiles.set(key,{path:body.path,text:'# File preview\nA retained file.\n',version:'v1',binary:false,truncated:false});return json(workspaceFiles.get(key));}
      case 'SaveWorkspaceFile': {const key=body.collectionId+'/'+body.path,file=workspaceFiles.get(key);if(file.version!==body.expectedVersion)return error('aborted','File changed on the Runner');Object.assign(file,{text:body.text,version:crypto.randomUUID()});mutations.push({method,body});return json(file);}
      case 'DeleteConversation': {const session=sessions.find(s=>s.id===body.sessionId);if(session&&!session.closed&&!body.stopSessions)return error('failed_precondition','Agent running');if(session)sessions.splice(sessions.indexOf(session),1);mutations.push({method,body});return json({});}
      case 'ResumeSession': {
        const previous=sessions.find(s=>s.id===body.sessionId);if(!previous)return error('not_found','Missing conversation');
        // Like the Runner (Bench.reconnect): a live conversation that only lost its stream re-attaches its agent in place.
        if(!previous.closed&&/^(provider stream ended|event capture failed)/.test(previous.fault||'')){Object.assign(previous,{fault:'',status:previous.busy?'running':'ready'});mutations.push({method,body});return json({session:previous});}
        let session=sessions.find(s=>s.previousExecutionId===previous.id);
        if(!session){session={...previous,id:previous.id+'-continued',previousExecutionId:previous.id,conversationId:previous.conversationId||previous.id,closed:false,historical:false,status:'ready'};sessions.push(session);}
        previous.successorExecutionId=session.id;mutations.push({method,body});return json({session});
      }
      // `frozenSessions`: a poll that lags the stream, answering with the session as it was when frozen.
      case 'GetSession': { const session = sessions.find(value => value.id === body.sessionId); if (session && frozenSessions.has(session.id)) return json({ session: frozenSessions.get(session.id) }); return session ? json({ session }) : error('not_found', 'Fixture session not found'); }
      case 'ListCommands': { const workspace = sessions.find(value => value.id === body.sessionId)?.collectionId; return json({ commands: commands.filter(value => value.sessionId === body.sessionId || (!value.sessionId && value.collectionId && value.collectionId === workspace)).map(listedCommand) }); }
      case 'ClaimCommandShare': { const command = commands.find(value => value.id === body.commandId); if (!command) return error('not_found', 'Command missing'); const claimed = !!command.shareWith && command.done && !claimedShares.has(command.id); if (claimed) claimedShares.add(command.id); return json({ claimed, status: wireCommand(command) }); }
      case 'PreviewWorkspaceDeletion': return json({activeSessionIds:sessions.filter(s=>s.collectionId===body.collectionId&&!s.closed).map(s=>s.id),dirtyRepositories:['api'],filesOutsideRepositories:false});
      case 'DeleteWorkspace': {const index=collections.findIndex(c=>c.id===body.collectionId);if(index<0||body.confirmName!==collections[index].name)return error('failed_precondition','Confirm name');if(!body.stopSessions||!body.discardChanges)return error('failed_precondition','Explicit cleanup consent required');mutations.push({method,body});const collection=collections.splice(index,1)[0];for(const s of sessions.filter(s=>s.collectionId===collection.id)){s.closed=true;s.status='stopped';}return json({collection:{...collection,state:'deleted'}});}
      case 'WatchEvents': {
        const channel=eventStreams.get(body.sessionId);if(channel){res.writeHead(200,{'Content-Type':'application/connect+json'});res.flushHeaders();
          // Like the Runner, each replayed record says where the retained history ends.
          const head=String(channel.events.length);
          const backlog=channel.events.filter(event=>BigInt(event.cursor)>BigInt(body.after||0)).map(event=>({...event,retained:head}));
          // `pace` paces a replay like the Runner's one-message-per-record stream: a
          // flush every `pace` records instead of one write of the whole journal.
          // Records pushed meanwhile follow the replay; then the stream is live.
          // A reader that asks for heartbeats hears one once it has every record there was, as
          // from the Runner; `channel.noHeartbeats` is a Runner from before them.
          const beat=sent=>{if(body.heartbeats&&!channel.noHeartbeats&&!res.destroyed){channel.heartbeats=(channel.heartbeats||0)+1;res.write(frame({cursor:String(sent),heartbeat:true,retained:String(Math.max(Number(head),sent))}));}};
          if(channel.pace){void (async()=>{let sent=Number(body.after||0);while(!res.destroyed&&sent<channel.events.length){const chunk=channel.events.slice(sent,sent+channel.pace);res.write(Buffer.concat(chunk.map(event=>frame({...event,retained:String(Math.max(Number(head),Number(event.cursor)))}))));sent+=chunk.length;await new Promise(resolve=>setTimeout(resolve,channel.paceMs??1));}
            if(res.destroyed)return;if(channel.done){res.end(frame({},2));return;}beat(sent);channel.clients.add(res);res.on('close',()=>channel.clients.delete(res));})();return;}
          channel.clients.add(res);res.on('close',()=>channel.clients.delete(res));for(const event of backlog)res.write(frame(event));if(channel.done){channel.clients.delete(res);res.end(frame({},2));return;}beat(Math.max(Number(body.after||0),channel.events.length));return;}
        const events=lineageEvents(body.sessionId);
        res.writeHead(200, { 'Content-Type': 'application/connect+json' }); res.end(stream(events.filter(value => BigInt(value.cursor) > BigInt(body.after || 0)))); return;
      }
      // The latest turns, or an earlier page, of what WatchEvents replays (the Runner's internal/runner/timeline.go).
      case 'ReadSessionTail': {
        // `noTail`: a Runner from before ReadSessionTail, for a conversation the window must replay.
        if(eventStreams.get(body.sessionId)?.noTail)return error('unimplemented','Unsupported fixture RPC ReadSessionTail');
        const events=eventStreams.get(body.sessionId)?.events||lineageEvents(body.sessionId);
        const page=readTail(events,body);
        if(!page)return error('out_of_range','cursor outside retained range');
        if(tailReadDelayMs)await new Promise(resolve=>setTimeout(resolve,tailReadDelayMs));
        return json(page);
      }
      case 'WatchOutput': {
        const command = commands.find(value => value.id === body.commandId) || runtimeCommands.find(value => value.id === body.commandId);
        // A streaming command (streamCommand): its chunks so far, then each new one as it is written, until it finishes.
        if (command?.live) {
          res.writeHead(200, { 'Content-Type': 'application/connect+json' }); res.flushHeaders();
          for (const event of command.live.events.filter(value => BigInt(value.cursor) > BigInt(body.after || 0))) res.write(frame(event));
          if (command.done) { res.end(frame({}, 2)); return; }
          command.live.clients.add(res); res.on('close', () => command.live.clients.delete(res)); return;
        }
        const events = [{ cursor: '1', offset: '0', channel: 'stdout', data: Buffer.from(command?.output || 'fixture retained output\n').toString('base64'), status: command && wireCommand(command) }];
        res.writeHead(200, { 'Content-Type': 'application/connect+json' }); res.end(stream(events.filter(value => BigInt(value.cursor) > BigInt(body.after || 0)))); return;
      }
      case 'OpenTerminal': {
        mutations.push({method,body});const collection=collections.find(c=>c.id===body.collectionId);if(!collection)return error('not_found','Workspace missing');
        if(body.directory&&!collection.members.some(m=>m.spec.name===body.directory))return error('failed_precondition','terminal directory must be inside the workspace');
        const existing=[...terminals.values()].find(t=>body.requestKey&&t.requestKey===body.requestKey);if(existing)return json(terminalStatus(existing));
        // A shell outside the workspace: a repository's home checkout, or the home folder.
        let place='',cwd='',placeTitle='';
        if(body.repositoryHome){const home=homes.get(body.repositoryHome);if(!home)return error('failed_precondition','this repository has no home checkout yet: create one in Settings → Repositories');const member=collection.members.find(m=>m.spec.repositoryId===body.repositoryHome);place='repository-home';cwd=home.path;placeTitle=(member?.spec.name||body.repositoryHome)+' home';}
        else if(body.userHome){place='user-home';cwd='/fixture/home';placeTitle='~';}
        const t={id:'terminal-'+(terminals.size+1),requestKey:body.requestKey,collectionId:collection.id,place,repositoryId:body.repositoryHome||'',cwd,directory:place?'':body.directory||'',title:place?placeTitle:collection.name+(body.directory?' / '+body.directory:''),cols:body.cols,rows:body.rows,createdAt:Date.now(),output:Buffer.alloc(0),line:'',typeahead:'',ready:false,running:false,input:[],resizes:[],done:false,clients:new Set()};
        terminals.set(t.id,t);terminalOutput(t,`fixture shell in ${place?cwd:t.title}\r\n`);
        if(terminalStartDelayMs)setTimeout(()=>shellReady(t),terminalStartDelayMs);else shellReady(t);
        return json(terminalStatus(t));
      }
      case 'ListTerminals': return json({terminals:[...terminals.values()].filter(t=>!body.collectionId||t.collectionId===body.collectionId).map(terminalStatus)});
      case 'WatchTerminal': {
        const t=terminals.get(body.terminalId);if(!t)return error('not_found','Terminal missing');
        res.writeHead(200,{'Content-Type':'application/connect+json'});res.flushHeaders();
        const after=Number(body.after||0);if(t.output.length>after)res.write(frame({offset:String(after),data:t.output.subarray(after).toString('base64'),status:terminalStatus(t)}));
        if(t.done){res.end(Buffer.concat([frame({offset:String(t.output.length),status:terminalStatus(t)}),frame({},2)]));return;}
        t.clients.add(res);res.on('close',()=>t.clients.delete(res));return;
      }
      case 'WriteTerminal': {
        const t=terminals.get(body.terminalId);if(!t||t.done)return error(t?'failed_precondition':'not_found','Terminal unavailable');
        const text=Buffer.from(body.data||'','base64').toString();t.input.push(text);terminalKeys(t,text);
        return json(terminalStatus(t));
      }
      case 'ResizeTerminal': {const t=terminals.get(body.terminalId);if(!t)return error('not_found','Terminal missing');Object.assign(t,{cols:body.cols,rows:body.rows});t.resizes.push([body.cols,body.rows]);return json(terminalStatus(t));}
      case 'CloseTerminal': {
        const t=terminals.get(body.terminalId);if(!t)return error('not_found','Terminal missing');mutations.push({method,body});
        t.done=true;for(const client of t.clients)client.end(Buffer.concat([frame({offset:String(t.output.length),status:terminalStatus(t)}),frame({},2)]));terminals.delete(t.id);return json(terminalStatus(t));
      }
      case 'CreateCollection': {
        mutations.push({ method, body });
        if (!requests.has(body.requestKey)) {
          const id = `created-${collections.length}`;
          const collection = { id, name: body.spec.name, workId: id, root: `/fixture/collections/${id}`, state: 'ready', members: (body.spec.members || []).map(member => ({ spec: { ...member, branch: `lazybox/${id}/${member.name}` }, path: `/fixture/collections/${id}/${member.name}`, baseCommit: 'c'.repeat(40), ready: true })) };
          requests.set(body.requestKey, collection); collections.push(collection);
        }
        return json({ collection: requests.get(body.requestKey) });
      }
      case 'AddCollectionRepositories': {
        mutations.push({method,body});const collection=collections.find(c=>c.id===body.collectionId);
        if(!collection)return error('not_found','Workspace missing');
        if(!requests.has(body.requestKey)){for(const member of body.members){const repo=library.find(r=>r.id===member.repositoryId);const name=member.name||repo?.name||member.repositoryId;collection.members.push({spec:{...member,name,branch:`feature/${collection.name}`},path:`${collection.root}/${name}`,baseCommit:'c'.repeat(40),ready:true});}requests.set(body.requestKey,collection);}
        return json({collection});
      }
      case 'OpenAssistant': {
        mutations.push({ method, body });
        const session = { id: `assistant-${sessions.length}`, collectionId:body.collectionId||'', profileId: body.profileId, purpose: body.purpose, status: 'ready', capabilities: { images: true }, pending: [] }; sessions.push(session); return json({ session });
      }
      // Like the Runner: the agent ends between turns and the conversation continues in a new one with that model.
      case 'RestartWithModel': case 'RestartWithPermission': {
        mutations.push({method,body});const old=sessions.find(s=>s.id===body.sessionId);if(!old)return error('not_found','Fixture session not found');
        if(old.busy)return error('failed_precondition','wait for the agent to finish its turn, or interrupt it, before restarting it');
        const next={...old,id:`${old.id}-restarted-${sessions.length}`,conversationId:old.conversationId||old.id,...(body.model?{model:body.model,modelReported:false}:{permission:body.mode})};sessions.push(next);old.closed=true;
        const channel=openEvents(next.id);channel.push(body.model?{type:'model_changed',text:body.model,status:'restart'}:{type:'permission_changed',text:body.mode,status:'restart'});
        return json({session:next});
      }
      case 'OpenSession': {
        mutations.push({ method, body });
        if (body.emptyWorkspace) { const session = {id:`empty-${sessions.length}`,profileId:body.profileId,workingDirectory:`/fixture/workspaces/${sessions.length}`,status:'ready',capabilities:{files:true},pending:[]};sessions.push(session);return json({session}); }
        const collection = collections.find(value => value.id === body.collectionId); if (!collection) return error('failed_precondition', 'Fixture Collection required');
        const session = { id: `started-${sessions.length}`, collectionId: collection.id, workId: collection.id, profileId: body.profileId, capabilities: { files: true }, pending: [] }; sessions.push(session); collection.ownerExecutionId = session.id;
        if (runner.journalMessages) openEvents(session.id);
        return json({ session });
      }
      case 'ListWorkspaceChanges': {
        const collection=collections.find(c=>c.id===body.collectionId);if(!collection)return error('not_found','Workspace missing');
        return json({repositories:collection.members.map(m=>{const s=shipState(collection.id,m.spec.name);return {member:m.spec.name,branch:s.branch,head:s.head,base:s.base||'c'.repeat(40),files:s.files,committed:s.committed,commits:s.commits,branches:[s.branch,'main','origin/main','release/2'],defaultBranch:'origin/main',upstream:s.upstream,ahead:s.ahead,githubRepository:s.githubRepository};})});
      }
      // The sidebar's git line, from the same ship state (behind, behindDefault and dirty are the test's to set).
      case 'ListWorkspaceGitSummaries': return json({workspaces:collections.filter(c=>!body.collectionIds?.length||body.collectionIds.includes(c.id)).map(c=>({collectionId:c.id,repositories:c.members.map(m=>{const s=shipState(c.id,m.spec.name);if(s.summaryError)return {member:m.spec.name,error:s.summaryError};const pr=s.pr;return {member:m.spec.name,branch:s.branch,head:s.head,upstream:s.upstream,ahead:s.ahead,behind:s.behind||0,defaultBranch:'origin/main',aheadOfDefault:s.commits.length,behindDefault:s.behindDefault||0,dirty:s.dirty??s.files.length,githubRepository:s.githubRepository,...(body.changes?{changes:s.lines||{base:'origin/main',files:s.files.length+s.committed.length,additions:0,deletions:0,truncated:false}}:{}),...(pr?{pullRequest:{number:pr.number,title:pr.title||'',url:pr.url||'',state:pr.state||'open',draft:!!pr.draft,checksState:pr.checksState||'none',reviewDecision:pr.reviewDecision||''}}:{})};})}))});
      case 'ReadWorkspaceDiff': {const text=diffs.get(`${body.collectionId}/${body.member}/${body.path}`);return json({text:text??`@@ fixture ${body.committed?'branch':'working tree'} diff @@\n+${body.path}`});}
      case 'ListReviewComments': if(!collections.some(c=>c.id===body.collectionId))return error('not_found','workspace missing');return json(reviewReply(body.collectionId));
      case 'AddReviewComment': {
        mutations.push({method,body});const r=reviewOf(body.collectionId);const snippet=body.snippet||[];
        if(!(body.body||'').trim())return error('failed_precondition','write a comment first');
        const now=String(Math.floor(Date.now()/1000)),c={id:'rc-'+randomUUID().slice(0,8),member:body.member,path:body.path,side:body.side,startLine:body.startLine,endLine:body.startLine+snippet.length-1,snippet,hash:'fixture',body:body.body.trimEnd(),head:shipState(body.collectionId,body.member).head,view:body.view,base:body.base||'',createdAtUnix:now,updatedAtUnix:now,disposition:'open'};
        r.comments.push(c);return json(reviewReply(body.collectionId,c.id));
      }
      case 'EditReviewComment': {mutations.push({method,body});const c=reviewOf(body.collectionId).comments.find(c=>c.id===body.id);if(!c)return error('not_found','review comment not found');c.body=body.body.trimEnd();return json(reviewReply(body.collectionId));}
      case 'DeleteReviewComment': {mutations.push({method,body});const r=reviewOf(body.collectionId);if(!r.comments.some(c=>c.id===body.id))return error('not_found','review comment not found');r.comments=r.comments.filter(c=>c.id!==body.id);return json(reviewReply(body.collectionId));}
      case 'SetReviewDisposition': {
        mutations.push({method,body});const r=reviewOf(body.collectionId);const chosen=r.comments.filter(c=>(body.ids||[]).includes(c.id));
        if(chosen.length!==(body.ids||[]).length)return error('not_found','review comment not found');
        if(body.disposition==='sent')queueMicrotask(()=>findingsSent(body.collectionId,body.ids||[],'You',body.sessionTitle));
        for(const c of chosen){Object.assign(c,{disposition:body.disposition,resolvedBy:'',note:body.disposition==='dismissed'?(body.note||'').trim():''});if(body.disposition==='sent')Object.assign(c,{sentTo:body.sessionId,sentToTitle:body.sessionTitle||'',sentHead:shipState(body.collectionId,c.member).head,sentAtUnix:String(Math.floor(Date.now()/1000))});if(body.disposition==='open')Object.assign(c,{sentTo:'',sentToTitle:'',sentHead:'',sentAtUnix:'0'});}
        return json(reviewReply(body.collectionId));
      }
      case 'SetReviewAutoResolve': mutations.push({method,body});reviewOf(body.collectionId).autoResolve=!!body.enabled;return json(reviewReply(body.collectionId));
      case 'CommitWorkspaceChanges': {
        mutations.push({method,body});const s=shipState(body.collectionId,body.member);
        const paths=body.paths||[];const chosen=s.files.filter(f=>!paths.length||paths.includes(f.path));
        if(!chosen.length)return error('failed_precondition','nothing to commit: the selected files have no changes');
        const subject=(body.message||'').trim().split('\n')[0];if(!subject)return error('failed_precondition','write a commit message');
        s.head=hex();s.files=s.files.filter(f=>!chosen.includes(f));s.committed.push(...chosen.map(f=>({path:f.path,indexStatus:f.indexStatus==='?'?'A':f.indexStatus.trim()||'M'})));s.commits.unshift({id:s.head,subject,committedAtUnix:String(Math.floor(Date.now()/1000))});s.ahead++;
        return json({commit:s.head,subject,files:chosen.length,branch:s.branch});
      }
      case 'PushWorkspaceBranch': {
        mutations.push({method,body});const s=shipState(body.collectionId,body.member);
        s.upstream='origin/'+s.branch;s.ahead=0;if(s.pr&&s.pr.state==='open')s.pr.headSha=s.head;return json({branch:s.branch,upstream:s.upstream,head:s.head,githubRepository:s.githubRepository});
      }
      case 'GetPullRequest': return json(pullRequest(shipState(body.collectionId,body.member)));
      case 'CreatePullRequest': {
        mutations.push({method,body});const s=shipState(body.collectionId,body.member);
        if(!s.upstream)return error('failed_precondition','push the branch before opening a pull request');
        if(s.ahead)return error('failed_precondition',`${s.ahead} local commit(s) are not pushed yet; push before opening a pull request`);
        s.pr={number:'7',title:body.title,body:body.body,url:`https://github.com/${s.githubRepository}/pull/7`,state:'open',draft:!!body.draft,base:body.base||'main',headSha:s.head,mergeableState:'unstable',reviewDecision:'',checksState:'failure',author:'fixture-user',
          checks:[{name:'test',status:'completed',conclusion:'failure',url:`https://github.com/${s.githubRepository}/actions/runs/41/job/1`,summary:'2 tests failed'},{name:'lint',status:'in_progress',conclusion:'',url:''},{name:'build',status:'completed',conclusion:'success',url:`https://github.com/${s.githubRepository}/actions/runs/40`}]};
        return json(pullRequest(s));
      }
      case 'ListLinks': {
        // Like the Runner (runner/items.go): the store's links, never GitHub on the request path.
        tracked.requests.push(body);
        const links=tracked.links.filter(l=>body.all||l.collectionId===body.collectionId);
        return json({links,events:tracked.events,trackingError:tracked.trackingError,pollSeconds:body.focused?60:300});
      }
      case 'SendPullRequestFailures': case 'ImportPullRequestReview': {
        mutations.push({method,body});
        const l=tracked.links.find(x=>x.collectionId===body.collectionId&&x.item?.ref?.key===body.ref?.key&&x.role!=='mentions');
        if(!l)return error('not_found','this workspace does not deliver that pull request');
        if(tracked.refusal)return error('failed_precondition',tracked.refusal);
        const sessionId=body.sessionId||l.sessionId,key=l.item.ref.key,session=sessions.find(x=>x.id===sessionId),queued=!!session?.busy;
        const failing=(l.item.pullRequest?.failingChecks||[]).map(c=>c.name);
        const kind=method==='SendPullRequestFailures'?'pr_checks':'pr_review';
        const text=kind==='pr_checks'?`[Lazybox] Checks failed on ${key}: ${failing.join(', ')}.\n\n### ${failing[0]} — \`go test ./...\` → exit 1\n\n\`\`\`text\n--- FAIL: TestRefundRounding\n\`\`\`\n\nFull log (12 KB): /fixture/output/${sessionId}/pr-${failing[0]}.log`:`[Lazybox] The review of ${key} on GitHub asks for changes.`;
        tracked.sent.push({method,sessionId,text,kind});
        eventStreams.get(sessionId)?.push({type:'operation_accepted',operationId:`${kind}-${tracked.sent.length}`,text,providerDataJson:originData({kind,summary:kind==='pr_checks'?`Checks failed on ${key}`:`Review of ${key} on GitHub`,detail:kind==='pr_checks'?failing.join(', '):'1 comment',copy:l.item.url})});
        return json({sessionId,queued,summary:kind==='pr_checks'?`Sent ${failing.length} failing ${failing.length===1?'check':'checks'} of #${key.split('#')[1]} to the agent`:`Sent 1 review comment of #${key.split('#')[1]} to the agent`,files:kind==='pr_checks'?[`/fixture/output/${sessionId}/pr-${failing[0]}.log`]:[],notes:[],comments:kind==='pr_review'?1:0});
      }
      case 'MergePullRequest': {
        if(body.item?.key){
          mutations.push({method,body});
          const l=tracked.links.find(x=>x.collectionId===body.collectionId&&x.item?.ref?.key===body.item.key&&x.role!=='mentions');
          if(!l)return error('not_found','this workspace does not deliver that pull request');
          if(body.expectedHead!==l.item.pullRequest?.headSha)return error('failed_precondition','GitHub: Head branch was modified. Review and try the merge again.');
          l.item.state={category:'merged',label:'Merged'};l.item.pullRequest.closedAtUnixMillis=String(Date.now());
          const [repo,number]=l.item.ref.key.split('#');
          return json({githubRepository:repo,branch:l.item.pullRequest.head,found:true,number,title:l.item.title,url:l.item.url,state:'merged',headSha:l.item.pullRequest.headSha});
        }
        mutations.push({method,body});const s=shipState(body.collectionId,body.member);
        if(!s.pr||String(s.pr.number)!==String(body.number))return error('failed_precondition',`pull request #${body.number} is not this branch's pull request`);
        if(s.mergeRefusal)return error('failed_precondition','GitHub: '+s.mergeRefusal);
        if(body.expectedHead!==s.pr.headSha)return error('failed_precondition','GitHub: Head branch was modified. Review and try the merge again.');
        Object.assign(s.pr,{state:'merged',mergeCommit:hex(),mergeMethod:body.method});return json(pullRequest(s));
      }
      case 'ListTurnCheckpoints': {const state=checkpointState(body.sessionId);return json({turns:state.turns.map(wireTurn),reverts:state.reverts,notePending:state.notePending});}
      case 'ListTurnChanges': {const t=checkpointState(body.sessionId).turns.find(t=>t.turn===body.turn);if(!t)return error('not_found','no checkpoint for that turn');return json({repositories:turnRepositories(t.files),inProgress:!t.ended});}
      case 'ReadTurnDiff': return json({text:`@@ turn ${body.turn} @@\n+${body.member}/${body.path}`});
      case 'PreviewRevertCode': {
        const state=checkpointState(body.sessionId);const index=state.turns.findIndex(t=>t.turn===body.turn);if(index<0)return error('not_found','no checkpoint for that turn');
        // Reverting to before a turn undoes it and every later turn.
        const files=state.reverted?.turn<=body.turn?[]:state.turns.slice(index).flatMap(t=>t.files).map(f=>({...f,status:flip[f.status]||f.status}));
        const session=sessions.find(s=>s.id===body.sessionId);const busy=sessions.find(s=>s.collectionId===session?.collectionId&&s.busy&&!s.closed);
        return json({repositories:turnRepositories(files).map(r=>({...r,commitsSince:state.commitsSince?.[r.member]||0,branchThen:'lazybox/'+r.member,branchNow:'lazybox/'+r.member})),blocked:busy?`an agent is working in this workspace; wait for its turn to end, or interrupt it, before reverting files (busy: “${busy.title||busy.id}”)`:''});
      }
      case 'RevertCode': {
        mutations.push({method,body});const state=checkpointState(body.sessionId);const index=state.turns.findIndex(t=>t.turn===body.turn);if(index<0)return error('not_found','no checkpoint for that turn');
        if(sessions.some(s=>s.collectionId===sessions.find(x=>x.id===body.sessionId)?.collectionId&&s.busy&&!s.closed))return error('failed_precondition','an agent is working in this workspace');
        const files=state.turns.slice(index).flatMap(t=>t.files).length;const revert={id:'r'+(state.reverts.length+1),kind:'revert',turn:body.turn,files,atUnixMillis:String(Date.now()),problems:[]};
        state.reverts.push(revert);state.reverted=revert;state.notePending=true;eventStreams.get(body.sessionId)?.push({type:'code_reverted',text:`Files reverted to before turn ${body.turn}.`,status:'revert',requestId:revert.id});return json(revert);
      }
      case 'UndoRevertCode': {
        mutations.push({method,body});const state=checkpointState(body.sessionId);const last=state.reverts.at(-1);if(!last||last.id!==body.revertId)return error('failed_precondition','only the latest revert can be undone');if(last.undoneBy)return error('failed_precondition','that revert was already undone');
        const undo={id:'r'+(state.reverts.length+1),kind:'undo',turn:last.turn,undoOf:last.id,files:last.files,atUnixMillis:String(Date.now()),problems:[]};last.undoneBy=undo.id;state.reverts.push(undo);state.reverted=undefined;state.notePending=true;
        eventStreams.get(body.sessionId)?.push({type:'code_reverted',text:`Revert to before turn ${last.turn} undone; files are back as they were.`,status:'undo',requestId:undo.id});return json(undo);
      }
      case 'ListProcesses': return json(processInventory());
      case 'StopProcess': {
        mutations.push({ method, body });
        const terminal = [...terminals.values()].find(t => 'host-' + t.id === body.id && !t.done);
        if (terminal) { terminal.done = true; for (const client of terminal.clients) client.end(frame({}, 2)); return json(processInventory()); }
        const run = String(body.id || '').startsWith('run:') && commands.find(c => 'run:' + c.id === body.id && !c.done);
        if (run) { Object.assign(run, { done: true, signal: 'SIGTERM', ports: [] }); return json(processInventory()); }
        const index = processes.findIndex(p => p.id === body.id);
        if (index < 0) return error('not_found', 'hosted process not found');
        processes.splice(index, 1); return json(processInventory());
      }
      case 'CleanUpOrphanProcesses': {
        mutations.push({ method, body });
        const orphans = processes.filter(p => !p.attached);
        for (const p of orphans) processes.splice(processes.indexOf(p), 1);
        return json({ stopped: orphans.length, inventory: processInventory() });
      }
      case 'GetRemoteAccess': return json(remote.access());
      case 'SetRemoteAccess': { mutations.push({ method, body }); remote.tailscale.enabled = !!body.tailscale; remote.tailscale.state = body.tailscale ? 'serving' : 'available'; return json(remote.access()); }
      case 'BeginDevicePairing': {
        if (!remote.access().phoneUrl) return error('failed_precondition', 'no route reaches this Runner from a phone yet; turn on Tailscale access in Settings → Devices first');
        const grants = ['read', ...['respond', 'start'].filter(g => (body.grants || []).includes(g))];
        const offer = { id: randomUUID().replaceAll('-', ''), secret: randomUUID().replaceAll('-', '') + randomUUID().replaceAll('-', ''), state: 'pending', grants, expiresAtUnixMillis: String(Date.now() + remote.offerLifetimeMs) };
        remote.offers.set(offer.id, offer);
        return json({ ...remote.wireOffer(offer), url: `${remote.phoneUrl}/?view=remote#${new URLSearchParams({ pair: `${offer.id}.${offer.secret}`, grants: grants.join(',') })}` });
      }
      case 'GetDevicePairing': { const offer = remote.offers.get(body.pairingId); return offer ? json(remote.wireOffer(offer)) : error('not_found', 'this pairing code is not valid'); }
      case 'CancelDevicePairing': { const offer = remote.offers.get(body.pairingId); if (!offer) return error('not_found', 'unknown'); if (remote.state(offer) === 'pending') offer.state = 'cancelled'; return json(remote.wireOffer(offer)); }
      case 'ListDevices': return json({ devices: [...remote.devices.values()].map(remote.wireDevice) });
      // Like the Runner: a phone reads and sets only its own push; the owner may only test a named device.
      case 'GetPushSettings': { if (!device) return error('failed_precondition', 'only a paired phone has push settings; open Lazybox on the phone'); return json(remote.pushView(device, true)); }
      case 'SetPushSettings': {
        if (!device) return error('failed_precondition', 'only a paired phone has push settings; open Lazybox on the phone');
        mutations.push({ method, body, device: device.id });
        const push = device.push ||= { preferences: { ...pushDefaults } };
        if (body.subscription) { if (!/^https:\/\/([a-z0-9-]+\.)*(push\.apple\.com|fcm\.googleapis\.com|push\.services\.mozilla\.com|notify\.windows\.com)\//.test(body.subscription.endpoint || '')) return error('invalid_argument', 'this push service is not accepted'); push.subscription = body.subscription; push.lastError = ''; }
        if (body.unsubscribe) delete push.subscription;
        if (body.preferences) push.preferences = { needsInput: false, failed: false, review: false, quietStart: '', quietEnd: '', timeZone: '', ...body.preferences }; // ProtoJSON omits false and ''
        return json(remote.pushView(device, true));
      }
      case 'SendTestPush': {
        const target = device ? (body.deviceId && body.deviceId !== device.id ? null : device) : remote.devices.get(body.deviceId);
        if (!target) return device ? error('permission_denied', 'a phone may only test itself') : error('not_found', 'this device is not paired');
        mutations.push({ method, body, ...(device ? { device: device.id } : {}) });
        if (!target.push?.subscription) return error('failed_precondition', 'Notifications are off on this phone; turn them on there.');
        if (remote.pushFailure) { target.push.lastError = remote.pushFailure; return error('failed_precondition', remote.pushFailure); }
        const payload = { title: 'Notifications work', kind: 'test', url: '/?view=remote', tag: 'test' };
        remote.pushes.push({ device: target.id, endpoint: target.push.subscription.endpoint, payload }); target.push.lastSentAt = Date.now();
        return json({ push: remote.pushView(target, false) });
      }
      case 'RevokeDevice': { if (!remote.devices.delete(body.deviceId)) return error('not_found', 'this device is not paired'); mutations.push({ method, body }); return json({ devices: [...remote.devices.values()].map(remote.wireDevice) }); }
      case 'CancelCommand': {
        mutations.push({ method, body });
        const command = commands.find(value => value.id === body.commandId);
        if (!command) return error('not_found', 'Command missing');
        Object.assign(command, { done: true, signal: 'SIGTERM', canBackground: false });
        return json({ status: wireCommand(command) });
      }
      // Like the Runner: only a `lazybox run` still in the foreground whose CLI follows it moves.
      case 'BackgroundCommand': {
        mutations.push({ method, body });
        const command = commands.find(value => value.id === body.commandId);
        if (!command) return error('not_found', 'Command missing');
        if (!command.canBackground || command.done || command.background) return error('failed_precondition', 'only a lazybox run still in the foreground can move to the background');
        Object.assign(command, { background: true, canBackground: false });
        return json(wireCommand(command));
      }
      case 'Apply': {
        mutations.push({ method, body, ...(device ? { device: device.id } : {}) });
        if (remote.resolveOnAnswer && (body.approval || body.answer)) { const answered = sessions.find(s => s.id === body.sessionId); const id = (body.approval || body.answer).requestId; if (answered?.pending?.some(r => r.requestId === id)) { answered.pending = answered.pending.filter(r => r.requestId !== id); if (!answered.pending.length) { answered.status = 'running'; answered.busy = true; } } }
        // Like the Runner: at a subscription limit a message is accepted and
        // waits in the queue (journaled once), delivered after the switch.
        const session=sessions.find(s=>s.id===body.sessionId);
        // `applyDelayMs`: a message the Runner takes a while to confirm (a send in flight).
        if(session?.applyDelayMs&&body.message)await new Promise(resolve=>setTimeout(resolve,session.applyDelayMs));
        // `turnOver`: the turn ended in the Runner before the client's view of it, as the engine says it.
        if(session?.turnOver&&body.interrupt)return error('failed_precondition','no active turn');
        // Like the Runner: a stop names a sub-agent it knows running, by the provider's id; else nothing is running.
        // `notRunning`: the Runner's own state moved on after the snapshot the client shows.
        if(session&&body.stopAgent){const agent=(session.agents||[]).find(a=>a.id===body.stopAgent.agentId);if(!agent||!agent.nativeId||agent.notRunning||!['running','starting','waiting_input','waiting_approval'].includes(agent.status))return error('failed_precondition','sub-agent is not running');}
        // Like the Runner: a live model switch is a setting, recorded as one line in the conversation.
        if(session&&body.model){if(!session.capabilities?.liveModel)return error('unimplemented','operation unsupported by adapter');session.model=body.model.model;session.modelReported=true;eventStreams.get(session.id)?.push({type:'model_changed',text:body.model.model,status:'live'});return json({operationId:body.operationId,delivery:'acknowledged'});}
        // Like the Runner: an "always allow" taken back leaves the conversation's list.
        if(session&&body.revokeGrant){const grants=session.permissionGrants||[];if(!grants.some(g=>g.id===body.revokeGrant.grantId))return error('failed_precondition','this conversation has no such grant');session.permissionGrants=grants.filter(g=>g.id!==body.revokeGrant.grantId);return json({operationId:body.operationId,delivery:'acknowledged'});}
        if(session&&body.permission){session.permission=body.permission.mode;eventStreams.get(session.id)?.push({type:'permission_changed',text:body.permission.mode,status:'live'});return json({operationId:body.operationId,delivery:'acknowledged'});}
        // Like the Runner: a queued message delivered ahead of its turn, "now" (the turn is
        // interrupted, it goes next) or "steer" (into the running turn, where the agent can).
        if(session&&body.deliverQueued){
          const {operationId,how}=body.deliverQueued,queue=session.queuedMessages||[],index=queue.findIndex(m=>m.operationId===operationId);
          if(index<0)return error('failed_precondition','that message is no longer queued');
          if(how==='steer'&&!session.capabilities?.steer)return error('unimplemented','operation unsupported by adapter');
          if(!session.busy)return error('failed_precondition','no active turn');
          queue.splice(index,1);session.queued=Math.max(0,(session.queued||1)-1);
          const ch=eventStreams.get(session.id);
          if(how==='now'){
            session.interrupted=(session.interrupted||0)+1;
            // The journal of a Claude turn stopped for it, as a Claude worker
            // that still reports the stop writes it: the interrupt, Claude's
            // "error during execution" failure, the turn ended interrupted.
            // Then the message is delivered (its receipt) and is the next turn
            // (session.sentNowReply answers it).
            if(ch&&session.provider==='claude'){
              ch.push({type:'operation_accepted',operationId:`${operationId}:now`,text:''});
              ch.push({type:'receipt',receipt:{operationId:`${operationId}:now`,delivery:'acknowledged'}});
              ch.push({type:'failure',status:'provider_error',text:'[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use',account:{state:'unknown',reason:'error_during_execution'}});
              ch.push({type:'turn_ended',status:'interrupted',providerDataJson:Buffer.from(JSON.stringify({subtype:'error_during_execution'})).toString('base64')});
              ch.push({type:'receipt',receipt:{operationId,delivery:'submitted'}});
              ch.push({type:'turn_started'});
              if(session.sentNowReply)ch.push({type:'message_delta',text:session.sentNowReply});
            }
          }
          // Steered into the running turn: the receipt says so (engine.DeliverQueued).
          else ch?.push({type:'receipt',status:'steered',receipt:{operationId,delivery:'acknowledged'}});
          return json({operationId:body.operationId,delivery:'accepted'});
        }
        // A message sent during a turn waits in the queue, which a test opts into with session.queuedMessages.
        // Like the Runner, it is journaled when accepted; its receipt, when it is delivered.
        if(session&&body.message&&session.busy&&Array.isArray(session.queuedMessages)){
          session.queuedMessages.push({operationId:body.operationId,text:String(body.message.text||'').slice(0,280),queuedAtUnixMillis:String(Date.now())});session.queued=session.queuedMessages.length;
          eventStreams.get(session.id)?.push({type:'operation_accepted',operationId:body.operationId,text:String(body.message.text||'')});
          return json({operationId:body.operationId,delivery:'accepted'});
        }
        if(session&&['exhausted','throttled','auth_required','billing_error'].includes(session.fault)&&body.message){
          const queued=waitingMessages.get(session)||new Map();waitingMessages.set(session,queued);
          if(!queued.has(body.operationId)){queued.set(body.operationId,body.message.text);session.queued=(session.queued||0)+1;eventStreams.get(session.id)?.push({type:'operation_accepted',text:body.message.text});}
        }
        // A message with files is retained like the Runner's attachment store
        // (by execution and content digest) and journaled with the file list a
        // conversation shows its thumbnails from.
        else if(session&&body.message?.attachments?.length){
          // With how each reached the agent, by the Runner's rule (agent.Delivery).
          let images=0;
          const files=body.message.attachments.map(file=>{const data=Buffer.from(file.data||'','base64');const path=`/fixture/attachments/${session.id}/${createHash('sha256').update(data).digest('hex')}/${file.name}`;attachments.set(path,{name:file.name,mediaType:file.mediaType||'application/octet-stream',data});
            const image=!!session.capabilities?.images&&images<4&&data.length<=6<<20&&/^image\/(png|jpeg|gif|webp)$/.test(file.mediaType||'');if(image)images++;
            return {name:file.name,mediaType:file.mediaType||'application/octet-stream',path,size:data.length,delivery:image?'image':data.length<=256<<10&&isText(data)?'text':'file'};});
          eventStreams.get(session.id)?.push({type:'operation_accepted',text:body.message.text||'',providerDataJson:Buffer.from(JSON.stringify({files})).toString('base64')});
        }
        // Like the Runner, which journals every message it accepts; opt-in (runner.journalMessages) so tests that count events keep theirs.
        else if(session&&body.message&&runner.journalMessages)eventStreams.get(session.id)?.push({type:'operation_accepted',operationId:body.operationId,text:body.message.text||''});
        return json({ operationId: body.operationId, delivery: 'accepted' });
      }
      // Like the Runner: only a retained attachment, only an image a browser displays or text (served as plain text).
      case 'ReadAttachment': {
        const file=attachments.get(body.path);
        if(!file)return error(String(body.path||'').startsWith('/fixture/attachments/')?'not_found':'invalid_argument','not a retained message attachment');
        const image=/^image\/(png|jpeg|gif|webp)$/.test(file.mediaType);
        if(!image&&!isText(file.data))return error('failed_precondition','only PNG, JPEG, GIF and WebP images and text attachments can be previewed');
        if(attachmentReadDelayMs)await new Promise(resolve=>setTimeout(resolve,attachmentReadDelayMs));
        return json({name:file.name,mediaType:image?file.mediaType:'text/plain; charset=utf-8',data:file.data.toString('base64')});
      }
      // A switch continues the same conversation: the fixture journals what the
      // Runner does (switching, switched with its origin) and clears the limit.
      case 'SwitchSubscription': {
        mutations.push({method,body});const session=sessions.find(s=>s.id===body.sessionId);if(!session)return error('not_found','Missing conversation');
        if(session.closed)return error('failed_precondition','the conversation has ended; resume it to continue');
        const target=profiles.find(p=>p.id===body.profileId);if(!target)return error('failed_precondition','subscription not found');
        const from=session.profileId,channel=eventStreams.get(session.id),data=value=>Buffer.from(JSON.stringify(value)).toString('base64');
        const resetAt=Number(session.account?.resetAt||0),fromState=session.account?.state||'available';
        if(from===target.id){channel?.push({type:'profile_changed',text:from,status:'retry_same_profile',providerDataJson:data({from,fromState,resetAt,requested:true})});}
        else{channel?.push({type:'profile_switching',status:'requested'});channel?.push({type:'profile_changed',text:target.id,status:'checkpoint_handoff',providerDataJson:data({from,fromState,resetAt,requested:true})});}
        if(body.enableRotation)session.rotationProfileIds=profiles.filter(p=>p.provider===target.provider&&p.isolation==='environment'&&p.id!==target.id).map(p=>p.id);
        Object.assign(session,{profileId:target.id,fault:'',status:'running',busy:true,queued:0,account:{state:'unknown'}});waitingMessages.delete(session);
        return json({session});
      }
      case 'SetSessionRotation': {
        mutations.push({method,body});const session=sessions.find(s=>s.id===body.sessionId);if(!session)return error('not_found','Missing conversation');
        const provider=profiles.find(p=>p.id===session.profileId)?.provider||session.provider;
        const pool=id=>body.enabled?profiles.filter(p=>p.provider===provider&&p.isolation==='environment'&&p.id!==id).map(p=>p.id):[];
        for(const s of body.allSessions?sessions.filter(s=>!s.closed&&(s.provider||profiles.find(p=>p.id===s.profileId)?.provider)===provider):[session])s.rotationProfileIds=pool(s.profileId);
        return json({session});
      }
      case 'CloseSession': mutations.push({ method, body }); return error('failed_precondition', 'Closing a surface must not close a session');
      // The real reviewed-team preset as the Runner sends it (`lazybox process show --json --preset reviewed-team`).
      case 'GetOrchestrationProcess': return json({
        preset: 'reviewed-team', valid: true, digest: 'sha256:fixture', json: JSON.stringify(orchestrationProcess), userFile: '/fixture/state/process.yaml',
        layers: [{ kind: 'preset', source: 'reviewed-team', digest: 'sha256:preset' }, ...(body.collectionId ? [{ kind: 'repository', source: 'api/.lazybox/process.yaml', digest: 'sha256:repo' }] : [])],
        issues: body.collectionId ? [{ severity: 'warning', path: 'roles.worker.instructions.file', message: '.lazybox/roles/worker.md does not exist in this repository; the role starts without these instructions', layer: 'preset:reviewed-team', source: 'preset reviewed-team', line: 39, column: 26 }] : [],
        candidates: body.collectionId ? [{ repositoryId: 'api', name: 'api', path: '/fixture/api/.lazybox/process.yaml', digest: 'sha256:repo' }] : [],
      });
      case 'GetWorkspaceRuntime': return collections.some(c => c.id === body.collectionId) ? json(runtimeView(body.collectionId)) : error('not_found', 'Workspace missing');
      case 'StartWorkspaceAction': {
        mutations.push({ method, body });
        const runtime = runtimeFor(body.collectionId), action = runtime.actions.find(a => a.key === body.action);
        if (!action) return error('not_found', `No action ${body.action} in this workspace`);
        if (action.blocked) return error('failed_precondition', action.blocked);
        const stoppedElsewhere = [];
        if (isLiveAction(action) && !body.restart) return json({ runtime: runtimeView(body.collectionId), stoppedElsewhere });
        if (isLiveAction(action)) { const old = runtimeCommands.find(c => c.id === action.runState.commandId); finished(old, 'stopped', { signal: 'SIGTERM' }); }
        if (!action.concurrent) for (const other of collections.filter(c => c.id !== body.collectionId)) {
          const elsewhere = runtimes[other.id]?.actions.find(a => a.key === action.key && isLiveAction(a));
          if (elsewhere) { stopAction(other.id, elsewhere); stoppedElsewhere.push({ collectionId: other.id, workspaceName: other.name, action: action.key }); }
        }
        const command = runCommand(body.collectionId, 'action', action.run, action.url ? `> ${action.name}\nready on ${action.url}\n` : `> ${action.name}\n✓ 42 tests passed\n`);
        action.runState = runState(command);
        return json({ runtime: runtimeView(body.collectionId), stoppedElsewhere });
      }
      case 'StopWorkspaceAction': {
        mutations.push({ method, body });
        const action = runtimeFor(body.collectionId).actions.find(a => a.key === body.action);
        if (!action) return error('not_found', `No action ${body.action} in this workspace`);
        stopAction(body.collectionId, action);
        return json({ runtime: runtimeView(body.collectionId), stoppedElsewhere: [] });
      }
      case 'ApproveWorkspaceScripts': {
        mutations.push({ method, body });
        const runtime = runtimeFor(body.collectionId), source = runtime.sources.find(s => s.member === body.member);
        if (!source) return error('not_found', 'No scripts for that repository');
        if (source.hash !== body.hash) return error('failed_precondition', 'The scripts changed since you reviewed them; review the new version');
        Object.assign(source, { approval: 'approved', changed: [] });
        for (const action of runtime.actions.filter(a => a.member === body.member)) action.blocked = '';
        const setup = runtime.setups.find(s => s.member === body.member);
        if (setup) { setup.pendingApproval = false; if (body.runSetup && setup.runState?.state !== 'succeeded') startSetup(body.collectionId, setup); }
        return json(runtimeView(body.collectionId));
      }
      case 'RunWorkspaceSetup': {
        mutations.push({ method, body });
        const setup = runtimeFor(body.collectionId).setups.find(s => s.member === body.member);
        if (!setup) return error('not_found', 'No setup for that repository');
        if (setup.pendingApproval) return error('failed_precondition', `Approve ${body.member}’s scripts first`);
        startSetup(body.collectionId, setup);
        return json(runtimeView(body.collectionId));
      }
      case 'GetWorkspaceRuntimeSettings': return json(runtimeSettingsView());
      case 'SaveWorkspaceRuntimeSettings': {
        mutations.push({ method, body });
        if (body.revision !== runtimeSettings.revision) return error('aborted', 'Workspace settings changed elsewhere; reload them');
        const repositories = (body.repositories || []).map(r => ({ repositoryId: r.repositoryId, config: r.config || '', defaultAction: r.defaultAction || '', skipSetup: !!r.skipSetup, error: /^\s*bogus\s*:/m.test(r.config || '') ? 'line 1: field bogus not found in type workspace.Config' : '' }));
        runtimeSettings = { revision: String(Number(runtimeSettings.revision) + 1), portRangeStart: body.portRangeStart, portRangeEnd: body.portRangeEnd, repositories };
        return json(runtimeSettingsView());
      }
      default: { const answer = missions.handle(method, body) || items.handle(method, body) || acp.handle(method, body, availabilityReport) || browser.handle(method, body, res) || config.handle(method, body) || await extensions.handle(method, body) || await power.handle(method, body, res); if (answer === true) return; if (answer) return answer.error ? error(answer.error.code, answer.error.message) : json(answer.value); return error('unimplemented', `Unsupported fixture RPC ${method}`); }
    }
  };
  const listen = async () => { const next = createServer(handle); await new Promise(resolve => next.listen(0, '127.0.0.1', resolve)); return next; };
  const stop = async value => { value.closeAllConnections(); await new Promise(resolve => value.close(resolve)); };
  let server = await listen();
  url = `http://127.0.0.1:${server.address().port}`;
  const directory = await mkdtemp(join(tmpdir(), 'lazybox-workspace-ui-'));
  const endpoint = join(directory, 'endpoint.json');
  const publish = () => writeFile(endpoint, JSON.stringify({ url, token, runnerId: 'fixturerunneridentity' }), { mode: 0o600 });
  await publish();
  return { claudeAuth, arrangement, get runnerSettings() { return runnerSettings; }, freezeSession: id => frozenSessions.set(id, structuredClone(sessions.find(value => value.id === id))), thawSession: id => frozenSessions.delete(id), items, versionInfo, reviewBriefs, fileFinding, finishReview, authorFixed, resolveFinding, reopenFinding, exchange, power, config, extensions, browser, artifacts: artifactStore, themeFolder, remote, checkpoints, setCheckpoints, attachments, set telemetry(fn) { telemetry = fn; }, set attachmentReadDelayMs(value) { attachmentReadDelayMs = value; }, set terminalStartDelayMs(value) { terminalStartDelayMs = value; }, terminalOutput, set tailReadDelayMs(value) { tailReadDelayMs = value; }, runner, missions, processes, processHosting, terminals, commands, runtimeCommands, runtimes, runtimeFor, finishSetup, runtimeTiming, previewPort, startRun, startTerminalRun, streamCommand, claimedShares, githubRepositories, preparationFailures, prepared, repositoryRequests, workspaceFiles, openEvents, reviews, reviewOf, diffs, workingFiles,
    availability, acp, tracked,
    /** A conversation's open journal, without replacing it (openEvents starts a new one). */
    eventsOf: id => eventStreams.get(id),
    get url() { return url; }, get token() { return token; }, get runnerSettings() { return runnerSettings; }, endpoint, calls, mutations, pins, collections, sessions, homes, profiles, enrollments, completeEnrollment, failEnrollment, github, completeGitHub, library, ship, shipState,
    /** A Codefly hot reload: the process is replaced on the same port with a new bearer; its streams end. */
    async hotReload() { token = randomUUID(); runner.instanceId = randomUUID(); server.closeAllConnections(); await publish(); },
    /** A launcher restart: the old port stops answering and the new Runner serves elsewhere, with a new bearer. */
    async moveToNewPort() { const previous = server; server = await listen(); url = `http://127.0.0.1:${server.address().port}`; token = randomUUID(); runner.instanceId = randomUUID(); await stop(previous); await publish(); },
    async close() { await stop(server); await stop(previewServer); await extensions.close(); await rm(directory, { recursive: true, force: true }); } };
}
