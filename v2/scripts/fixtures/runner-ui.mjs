// An explicitly fake RPC authority for isolated browser/native surface checks.
// It neither loads credentials nor runs Git, commands, or model inference.
import { createServer } from 'node:http';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';

const frame = (value, flag = 0) => { const data = Buffer.from(JSON.stringify(value)); const head = Buffer.alloc(5); head[0] = flag; head.writeUInt32BE(data.length, 1); return Buffer.concat([head, data]); };
const stream = values => Buffer.concat([...values.map(value => frame(value)), frame({}, 2)]);
export async function startRunnerFixture() {
  const token = randomUUID();
  const calls = [], mutations = [];
  const workspaceFiles=new Map();
  const eventStreams=new Map();
  const openEvents=id=>{const channel={events:[],clients:new Set(),push(event){const record={cursor:String(this.events.length+1),event};this.events.push(record);for(const res of this.clients)res.write(frame(record));}};eventStreams.set(id,channel);return channel;};
  const collections = ['alpha', 'beta'].map((id, index) => ({ id, name: index ? 'Beta release' : 'Alpha feature', workId: id, root: `/fixture/collections/${id}`, state: 'ready', ownerExecutionId: `${id}-session`, members: ['api', 'web'].map((name, i) => ({ spec: { name, repositoryId: i ? 'frontend' : 'backend', baseRef: 'main', branch: `lazybox/${id}/${name}` }, path: `/fixture/collections/${id}/${name}`, baseCommit: (index ? 'b' : 'a').repeat(40), ready: true })) }));
  const sessions = collections.map(collection => ({ id: `${collection.id}-session`, collectionId: collection.id, workId: collection.id, profileId: 'claude-default', gatewayId: 'direct', workingDirectory: collection.root, capabilities: { files: true, images: true, resume: true }, pending: [] }));
  sessions[0].status = 'running'; sessions[0].agents = [{ id: 'root', name: 'Main agent', status: 'done' }, { id: 'child-review', parentId: 'root', name: 'Review', status: 'running' }];
  const commands = sessions.map(session => ({ id: `${session.collectionId}-command`, sessionId: session.id, title: `${session.collectionId} fixture build`, mode: 'managed', done: true, exitCode: 0, offset: '24' }));
  const requests = new Map();
  const library = [{ id: 'backend', path: '/fixture/backend' }, { id: 'frontend', path: '/fixture/frontend' }];
  const github = { state: 'setup_required', clientId: '', appSlug: '', login: '', userId: '0', pendingSignIn: undefined };
  const githubFlows = new Map();
  const discovery = { roots: [] };
  const githubRepositories = [
    { id: '101', name: 'api', fullName: 'fixture-team/api', private: true, defaultBranch: 'main', htmlUrl: 'https://github.com/fixture-team/api' },
    { id: '102', name: 'web', fullName: 'fixture-team/web', private: false, defaultBranch: 'main', htmlUrl: 'https://github.com/fixture-team/web' },
  ];
  let catalogRemote=[], catalogUpdated='0';const preparationFailures=new Set();const prepared=[];
  const catalogEntries=()=>{const rows=new Map();for(const r of catalogRemote)rows.set('github:'+r.fullName.toLowerCase(),{identity:'github:'+r.fullName.toLowerCase(),name:r.fullName});for(const r of discovery.roots.flatMap(root=>root.repositories||[]))rows.set(r.gitIdentity||r.path,{identity:r.gitIdentity||r.path,name:r.gitIdentity?.startsWith('github:')?r.gitIdentity.slice(7):r.name,path:r.path});for(const r of library){const identity=r.gitIdentity||'local:'+r.id;rows.set(identity,{...rows.get(identity),identity,name:r.gitIdentity?.startsWith('github:')?r.gitIdentity.slice(7):r.name||r.id,repositoryId:r.id});}return [...rows.values()];};
  const completeGitHub = () => { github.state = 'connected'; github.authMethod = 'github_app'; github.login = 'fixture-user'; github.userId = '42'; if (github.pendingSignIn) { github.pendingSignIn.state = 'connected'; github.pendingSignIn.userCode = ''; github.pendingSignIn.signInUrl = ''; } github.pendingSignIn = undefined; };

  const profiles = [{ id: 'claude-default', provider: 'claude', isolation: 'inherited' }, { id: 'codex-default', provider: 'codex', isolation: 'inherited', availability: { state: 'available', plan: 'plus', observedAtUnixMillis: String(Date.now()), windows: [{ name: 'codex:primary', durationMinutes: '300', usedPercent: 45, resetAt: String(Math.floor(Date.now()/1000)+7200) }, { name: 'codex:secondary', durationMinutes: '10080', usedPercent: 60, resetAt: String(Math.floor(Date.now()/1000)+172800) }] } }];
  profiles.push({ id:'codex-saved', provider:'codex', label:'Saved Codex', managed:true, isolation:'environment', availability:{...structuredClone(profiles[1].availability),email:'codex-fixture@example.invalid'} });
  const enrollments = new Map();
  const completeEnrollment = id => { const flow = enrollments.get(id); if (!flow) throw new Error('Unknown fixture enrollment'); if (flow.state === 'added') return; flow.state = 'added'; flow.signInUrl = ''; flow.userCode = ''; profiles.push({ id: flow.profileId, provider: flow.provider, label: flow.label, managed:true, isolation: 'environment', availability: flow.provider === 'codex' ? { state: 'available', plan: 'pro', observedAtUnixMillis: String(Date.now()), windows: [{ name: 'codex:primary', durationMinutes: '300', usedPercent: 73, resetAt: String(Math.floor(Date.now()/1000)+3600) }] } : { state: 'unknown' } }); };

  let runnerSettings={revision:'1',naming:'repositories',permission:'',context:'You are running inside Lazybox.',claudeContext:'',codexContext:'',suggestedName:'gentle-copper-otter',prices:[]};
  const server = createServer(async (req, res) => {
    if (req.headers.authorization !== `Bearer ${token}`) { res.writeHead(401, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ code: 'unauthenticated', message: 'Fixture bearer required' })); return; }
    const method = req.url.split('/').at(-1); let body;
    try {
      const chunks = []; for await (const chunk of req) chunks.push(chunk);
      let data = Buffer.concat(chunks);
      if (req.headers['content-type']?.startsWith('application/connect+json')) data = data.subarray(5, 5 + data.readUInt32BE(1));
      body = JSON.parse(data.toString() || '{}');
    }
    catch { res.writeHead(400); res.end(); return; }
    calls.push({ method, body });
    const json = value => { res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(value)); };
    const error = (code, message) => { res.writeHead(400, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ code, message })); };
    switch (method) {
      case 'GetRunnerSettings': return json(runnerSettings);
      case 'SaveRunnerSettings': {
        if(body.revision!==runnerSettings.revision)return error('aborted','Settings changed');
        mutations.push({method,body});runnerSettings={...runnerSettings,...body,revision:String(Number(runnerSettings.revision)+1)};return json(runnerSettings);
      }
      case 'SetModelPrice': {
        if(body.revision!==runnerSettings.revision)return error('aborted','Settings changed');
        mutations.push({method,body});runnerSettings.prices=runnerSettings.prices.filter(p=>p.provider!==body.price.provider||p.model!==body.price.model);
        if(!body.remove)runnerSettings.prices.push({...body.price,version:'custom-fixture',custom:true});runnerSettings.revision=String(Number(runnerSettings.revision)+1);return json(runnerSettings);
      }
      case 'ListRepositoryDiscovery': return json(discovery);
      case 'RemoveRepository': {mutations.push({method,body});const index=library.findIndex(repo=>repo.id===body.repositoryId);if(index>=0)library.splice(index,1);for(const root of discovery.roots)root.repositories=root.repositories.filter(repo=>repo.gitIdentity!==body.gitIdentity);return json(discovery);}
      case 'ScanRepositoryRoot': { mutations.push({ method, body }); const root = { path: body.path, scannedAt: '1', repositories: [...['api', 'web', 'tools'].map(name => ({ name, path: body.path + '/' + name, gitIdentity: 'github:team/' + name })), { name: 'api-other-clone', path: body.path + '/other-clone', gitIdentity: 'github:team/api' }, { name: 'api-feature-worktree', path: body.path + '/api-feature', gitIdentity: 'github:team/api' }] }; discovery.roots = [...discovery.roots.filter(item => item.path !== body.path), root]; return json(discovery); }
      case 'GetGitHubConnection': return json(github);
      case 'ConnectGitHubPersonalToken': { mutations.push({ method, body }); if (body.token !== 'ghp_offline_fixture') return error('unauthenticated', 'Token rejected by fixture'); Object.assign(github, { state: 'connected', authMethod: 'personal_token', login: 'fixture-user', userId: '42', error: '' }); return json(github); }
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
      case 'GetTelemetry': return json({groups:[],contexts:[],recent:[]});
      case 'RenameCollection': { const workspace=collections.find(c=>c.id===body.collectionId);if(!workspace)return error('not_found','workspace missing');workspace.name=body.name;mutations.push({method,body});return json({collection:workspace}); }
      case 'Info': return json({ profiles, pendingEnrollments: [...enrollments.values()].filter(flow => flow.state === 'awaiting_login').map(({ id, state, provider, label, profileId }) => ({ id, state, provider, label, profileId })), sessions, attachmentLimits: { maxFiles: 4, maxFileBytes: '20971520', maxMessageBytes: '20971520' } });
      case 'AddSubscription': {
        mutations.push({ method, body });
        const id = `enrollment-${body.requestKey}`;
        if (!enrollments.has(id)) { enrollments.set(id, { id, state: 'awaiting_login', profileId: `saved-${enrollments.size}`, provider: body.provider, label: body.label, signInUrl: body.provider === 'claude' ? 'https://claude.com/cai/oauth/authorize?state=fixture' : 'https://auth.openai.com/codex/device', userCode: body.provider === 'claude' ? '' : 'AUTH-FIXTURE-CODE' }); }
        return json(enrollments.get(id));
      }
      case 'GetSubscriptionEnrollment': { const id=body.enrollmentId || `enrollment-${body.requestKey}`; return enrollments.has(id) ? json(enrollments.get(id)) : json({id, state:'expired',error:'Fixture enrollment no longer tracked'}); }
      case 'CancelSubscriptionEnrollment': { const flow = enrollments.get(body.enrollmentId); if (!flow) return error('not_found', 'Unknown enrollment'); flow.state = 'cancelled'; flow.userCode = ''; flow.signInUrl = ''; return json(flow); }
      case 'RenameConversation': {const session=sessions.find(s=>s.id===body.sessionId);if(!session)return error('not_found','Missing conversation');session.title=body.title;mutations.push({method,body});return json({session});}
      case 'ListHistory': return json({sessions:sessions.filter(s=>(body.collectionId===undefined||s.collectionId===body.collectionId)&&(!body.search||(s.title||`Conversation ${s.id}`).toLowerCase().includes(body.search.toLowerCase()))).map(s=>({...s,title:s.title||`Conversation ${s.id}`,conversationId:s.workId||s.id})),hasMore:false});
      case 'RefreshSubscription': { const profile = profiles.find(p => p.id === body.profileId); if (!profile) return error('not_found', 'Unknown profile'); if (profile.provider === 'claude') profile.availability = {state:'available',plan:'max',windows:[{name:'five_hour',usedPercent:37,durationMinutes:'300',resetAt:String(Math.floor(Date.now()/1000)+3600)}]}; profile.availability.observedAtUnixMillis = String(Date.now()); profile.availability.windows[0].usedPercent += 5; return json(profile); }
      case 'ListCollections': return json({ collections });
      case 'AddLocalRepository': {
        mutations.push({ method, body });
        let repository = library.find(repo => repo.path === body.path);
        if (!repository) { repository = { id: 'local-' + (library.length + 1), name: body.name || body.path.split('/').at(-1), path: body.path, gitIdentity: discovery.roots.flatMap(root => root.repositories).find(repo => repo.path === body.path)?.gitIdentity || '' }; library.push(repository); }
        return json({ repository });
      }
      case 'GetRepositoryCatalog': {if(body.refresh&&github.state==='connected'){catalogRemote=structuredClone(githubRepositories);catalogUpdated=String(Math.floor(Date.now()/1000));}return json({entries:catalogEntries(),sources:[{id:'github',state:github.state,detail:github.authMethod==='github_app'?'GitHub App access is limited to its installed repositories.':'Repositories visible to this personal token.',updatedAt:catalogUpdated}],refreshSeconds:300});}
      case 'PrepareWorkspace': {
       mutations.push({method,body});const values=[];const members=[];
       for(const identity of body.repositoryIdentities||[]){const choice=catalogEntries().find(r=>r.identity===identity);if(!choice)return error('not_found','Unknown repository identity');let repo=library.find(r=>r.id===choice.repositoryId);if(!repo){if(preparationFailures.delete(identity)){res.writeHead(200,{'Content-Type':'application/connect+json'});res.end(Buffer.concat([...values.map(v=>frame(v)),frame({error:{code:'failed_precondition',message:'Fixture preparation failure'}},2)]));return;}const remote=githubRepositories.find(r=>'github:'+r.fullName.toLowerCase()===identity);repo={id:remote?'github-'+remote.id:'local-'+(library.length+1),name:choice.name.split('/').at(-1),path:choice.path||'/fixture/repos/'+remote.id,gitIdentity:identity};library.push(repo);prepared.push(identity);}values.push({identity,message:'Preparing '+choice.name});members.push({repositoryId:repo.id,name:repo.name||repo.id});}
       let collection=requests.get(body.requestKey);if(!collection){if(body.collectionId){collection=collections.find(c=>c.id===body.collectionId);if(!collection)return error('not_found','Missing workspace');collection.members.push(...members.map(spec=>({spec,path:collection.root+'/'+spec.name,ready:true})));}else{const id='created-'+collections.length;collection={id,name:body.name,root:'/fixture/collections/'+id,state:'ready',members:members.map(spec=>({spec,path:'/fixture/collections/'+id+'/'+spec.name,ready:true}))};collections.push(collection);}requests.set(body.requestKey,collection);}values.push({message:'Ready',collection});res.writeHead(200,{'Content-Type':'application/connect+json'});res.end(stream(values));return;
      }
      case 'ListRepositories': return json({ repositories: library });
      case 'ReadWorkspaceFile': {const key=body.collectionId+'/'+body.path; if(!workspaceFiles.has(key))workspaceFiles.set(key,{path:body.path,text:'# File preview\nA retained file.\n',version:'v1',binary:false,truncated:false});return json(workspaceFiles.get(key));}
      case 'SaveWorkspaceFile': {const key=body.collectionId+'/'+body.path,file=workspaceFiles.get(key);if(file.version!==body.expectedVersion)return error('aborted','File changed on the Runner');Object.assign(file,{text:body.text,version:crypto.randomUUID()});mutations.push({method,body});return json(file);}
      case 'DeleteConversation': {const session=sessions.find(s=>s.id===body.sessionId);if(session&&!session.closed&&!body.stopSessions)return error('failed_precondition','Agent running');if(session)sessions.splice(sessions.indexOf(session),1);mutations.push({method,body});return json({});}
      case 'ResumeSession': {
        const previous=sessions.find(s=>s.id===body.sessionId);if(!previous)return error('not_found','Missing conversation');
        let session=sessions.find(s=>s.previousExecutionId===previous.id);
        if(!session){session={...previous,id:previous.id+'-continued',previousExecutionId:previous.id,conversationId:previous.conversationId||previous.id,closed:false,historical:false,status:'ready'};sessions.push(session);}
        previous.successorExecutionId=session.id;mutations.push({method,body});return json({session});
      }
      case 'GetSession': { const session = sessions.find(value => value.id === body.sessionId); return session ? json({ session }) : error('not_found', 'Fixture session not found'); }
      case 'ListCommands': return json({ commands: commands.filter(value => value.sessionId === body.sessionId) });
      case 'PreviewWorkspaceDeletion': return json({activeSessionIds:sessions.filter(s=>s.collectionId===body.collectionId&&!s.closed).map(s=>s.id),dirtyRepositories:['api'],filesOutsideRepositories:false});
      case 'DeleteWorkspace': {const index=collections.findIndex(c=>c.id===body.collectionId);if(index<0||body.confirmName!==collections[index].name)return error('failed_precondition','Confirm name');if(!body.stopSessions||!body.discardChanges)return error('failed_precondition','Explicit cleanup consent required');mutations.push({method,body});const collection=collections.splice(index,1)[0];for(const s of sessions.filter(s=>s.collectionId===collection.id)){s.closed=true;s.status='stopped';}return json({collection:{...collection,state:'deleted'}});}
      case 'WatchEvents': {
        const channel=eventStreams.get(body.sessionId);if(channel){res.writeHead(200,{'Content-Type':'application/connect+json'});res.flushHeaders();channel.clients.add(res);res.on('close',()=>channel.clients.delete(res));for(const event of channel.events)if(BigInt(event.cursor)>BigInt(body.after||0))res.write(frame(event));return;}
        const lineage=[];let id=body.sessionId;while(id&&!lineage.includes(id)){lineage.unshift(id);id=sessions.find(s=>s.id===id)?.previousExecutionId;}const events=lineage.flatMap((id,index)=>[{cursor:String(index*2+1),event:{type:'message_delta',text:`Retained conversation for ${id}`,sessionReference:id}},{cursor:String(index*2+2),event:{type:'message_delta',text:'CHILD_PRIVATE_TRANSCRIPT',agentId:'child-review',parentAgentId:'root'}}]);
        res.writeHead(200, { 'Content-Type': 'application/connect+json' }); res.end(stream(events.filter(value => BigInt(value.cursor) > BigInt(body.after || 0)))); return;
      }
      case 'WatchOutput': {
        const command = commands.find(value => value.id === body.commandId);
        const events = [{ cursor: '1', offset: '0', channel: 'stdout', data: Buffer.from('fixture retained output\n').toString('base64'), status: command }];
        res.writeHead(200, { 'Content-Type': 'application/connect+json' }); res.end(stream(events.filter(value => BigInt(value.cursor) > BigInt(body.after || 0)))); return;
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
      case 'OpenSession': {
        mutations.push({ method, body });
        if (body.emptyWorkspace) { const session = {id:`empty-${sessions.length}`,profileId:body.profileId,workingDirectory:`/fixture/workspaces/${sessions.length}`,status:'ready',capabilities:{files:true},pending:[]};sessions.push(session);return json({session}); }
        const collection = collections.find(value => value.id === body.collectionId); if (!collection) return error('failed_precondition', 'Fixture Collection required');
        const session = { id: `started-${sessions.length}`, collectionId: collection.id, workId: collection.id, profileId: body.profileId, capabilities: { files: true }, pending: [] }; sessions.push(session); collection.ownerExecutionId = session.id;
        return json({ session });
      }
      case 'Apply': mutations.push({ method, body }); return json({ operationId: body.operationId, delivery: 'accepted' });
      case 'CloseSession': mutations.push({ method, body }); return error('failed_precondition', 'Closing a surface must not close a session');
      default: return error('unimplemented', `Unsupported fixture RPC ${method}`);
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const url = `http://127.0.0.1:${server.address().port}`;
  const directory = await mkdtemp(join(tmpdir(), 'lazybox-workspace-ui-'));
  const endpoint = join(directory, 'endpoint.json');
  await writeFile(endpoint, JSON.stringify({ url, token }), { mode: 0o600 });
  return { githubRepositories, preparationFailures, prepared, workspaceFiles, openEvents, url, token, endpoint, calls, mutations, collections, sessions, profiles, enrollments, completeEnrollment, github, completeGitHub, library, async close() { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); await rm(directory, { recursive: true, force: true }); } };
}
