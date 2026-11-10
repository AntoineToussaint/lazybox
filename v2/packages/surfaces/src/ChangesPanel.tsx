import { FilePreview } from './FilePreview';
import { CodeBlock } from './RichContent';
import { useEffect, useState } from 'react';
import type { RepositoryChanges, WorkspaceDiff } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';

/** A workspace-scoped read-only projection. All Git operations belong to Runner. */
export function ChangesPanel({workspaceId}:{workspaceId:string}) {
 const {api}=useRunner();
 const [preview,setPreview]=useState<string>();
 const [repos,setRepos]=useState<RepositoryChanges[]>([]),[error,setError]=useState(''),[loading,setLoading]=useState(true),[revision,setRevision]=useState(0);
 const [selection,setSelection]=useState<{member:string;path:string}>(),[staged,setStaged]=useState(false),[savedDiff,setDiff]=useState<{key:string;value:WorkspaceDiff}>(),[diffError,setDiffError]=useState('');
 useEffect(()=>{
  if(!workspaceId){setLoading(false);return;}
  const control=new AbortController();let busy=false;
  const load=async()=>{if(busy)return;busy=true;try{const result=await api.listWorkspaceChanges({collectionId:workspaceId},{signal:control.signal,timeoutMs:20000});if(!control.signal.aborted){setRepos(result.repositories);setError('');}}catch(e){if(!control.signal.aborted)setError(String(e));}finally{busy=false;if(!control.signal.aborted)setLoading(false);}};
  void load();const timer=setInterval(()=>void load(),10000);return()=>{control.abort();clearInterval(timer);};
 },[api,workspaceId,revision]);
 const diffKey=JSON.stringify([workspaceId,selection,staged,revision]);
 const diff=savedDiff?.key===diffKey?savedDiff.value:undefined;
 useEffect(()=>{
  setDiff(undefined);setDiffError('');if(!selection)return;
  const control=new AbortController();void api.readWorkspaceDiff({collectionId:workspaceId,...selection,staged},{signal:control.signal,timeoutMs:20000}).then(value=>{if(!control.signal.aborted)setDiff({key:diffKey,value});},error=>{if(!control.signal.aborted)setDiffError(String(error));});return()=>control.abort();
 },[api,workspaceId,selection,staged,revision,diffKey]);
 if(!workspaceId)return <p className="module-empty">Choose a workspace to review its changes.</p>;
 const count=repos.reduce((sum,repo)=>sum+repo.files.length,0);
 return <section className="changes-panel" aria-label="Workspace changes">
  <header><div><strong>{loading?'Reading changes…':`${count} changed ${count===1?'file':'files'}`}</strong><small>Uncommitted changes · staged and unstaged files</small></div><button onClick={()=>setRevision(value=>value+1)}>Refresh changes</button></header>
  {error&&<p className="error" role="alert">{error}</p>}
  <div className="changes-columns"><nav aria-label="Changed files">{repos.map(repo=><section key={repo.member}><h3>{repo.member}<small>{repo.branch==='HEAD'?'Detached HEAD':repo.branch} · {repo.head.slice(0,8)}</small></h3>{repo.error?<p role="alert" className="error">{repo.error}</p>:repo.files.length===0?<p className="module-empty">Working tree clean</p>:repo.files.map(file=><button key={file.path} className={selection?.member===repo.member&&selection.path===file.path?'selected':''} onClick={()=>{setSelection({member:repo.member,path:file.path});setStaged(file.worktreeStatus===' '&&file.indexStatus!=='?');}}><code title="Index / working tree status">{file.indexStatus}{file.worktreeStatus}</code><span>{file.path}{file.originalPath&&<small>from {file.originalPath}</small>}</span></button>)}{repo.truncated&&<p role="status">File list limited. Some changes are not shown.</p>}</section>)}</nav>
   <div className="diff-view">{selection?<><header><strong>{selection.member}/{selection.path}</strong><button onClick={()=>setPreview(`${selection.member}/${selection.path}`)}>Open file</button><label><input type="checkbox" checked={staged} onChange={event=>setStaged(event.target.checked)}/> Staged</label></header><div className="diff-comparison">{staged?'HEAD → index (staged changes)':'Index → working tree (unstaged changes)'}</div>{diffError?<p role="alert" className="error">{diffError}</p>:!diff?<p role="status">Loading diff…</p>:<>{diff.truncated&&<p role="status">Preview limited to 1 MiB.</p>}{diff.binary?<p>Binary file · text preview unavailable</p>:diff.text?<CodeBlock text={diff.text} language="diff" label="File diff"/>:<p className="module-empty">No {staged?'staged':'unstaged'} diff for this file. Refresh if the agent has changed it.</p>}</>}</>:<div className="diff-empty"><h3>{count?'Select a file to review':'No uncommitted changes'}</h3><p>{count?'Choose a changed file on the left. Switch “Staged” to inspect the index against HEAD.':'The workspace has no staged, unstaged or untracked files reported by Git.'}</p><p>Committed branch changes are not included in this view.</p></div>}</div>
  </div>
 {preview&&<FilePreview workspaceId={workspaceId} reference={{path:preview}} onClose={()=>setPreview(undefined)}/>}
 </section>;
}
