import { lazy, Suspense, useEffect, useRef, useState } from 'react';
import {ConnectError} from '@lazybox/client-core';
import type { WorkspaceFile } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { RichContent } from './RichContent';
import type { FileLink } from './file-reference';

const CodeEditor = lazy(() => import('./CodeEditor'));

export function FilePreview({workspaceId, reference, onClose}:{workspaceId:string;reference:FileLink;onClose():void}) {
 const {api}=useRunner();
 const [path,setPath]=useState(reference.path);
 const dialog=useRef<HTMLDialogElement>(null),lifetime=useRef<AbortController>(null);
 const [file,setFile]=useState<WorkspaceFile>(),[draft,setDraft]=useState(''),[error,setError]=useState('');
 const [loading,setLoading]=useState(true),[saving,setSaving]=useState(false),[editing,setEditing]=useState(false);
 const [source,setSource]=useState(!!reference.line),[maximized,setMaximized]=useState(false);
 const [discard,setDiscard]=useState<'close'|'reload'|'cancel'>();
 const dirty=!!file&&draft!==file.text;
 const load=async(signal:AbortSignal)=>{
  setLoading(true);setError('');setFile(undefined);setEditing(false);
  try{const value=await api.readWorkspaceFile({collectionId:workspaceId,path},{signal,timeoutMs:10000});if(!signal.aborted){setFile(value);setDraft(value.text);}}
  catch(error){if(!signal.aborted)setError(ConnectError.from(error).rawMessage);}
  finally{if(!signal.aborted)setLoading(false);}
 };
 useEffect(()=>{dialog.current?.showModal();dialog.current?.querySelector<HTMLButtonElement>('button[aria-label="Close file preview"]')?.focus();},[]);
 useEffect(()=>{const control=new AbortController();lifetime.current=control;void load(control.signal);return()=>control.abort();},[api,workspaceId,path]);
 const act=(action:'close'|'reload'|'cancel',confirmed=false)=>{
  if(saving)return;
  if(dirty&&!confirmed){setDiscard(action);return;}
  setDiscard(undefined);
  if(action==='close')onClose();
  else if(action==='reload')void load(lifetime.current!.signal);
  else {setDraft(file?.text||'');setEditing(false);setError('');}
 };
 const save=async()=>{
  if(!file||!editing||saving||!dirty)return;
  const signal=lifetime.current!.signal;setSaving(true);setError('');
  try{const value=await api.saveWorkspaceFile({collectionId:workspaceId,path:file.path,text:draft,expectedVersion:file.version},{signal,timeoutMs:15000});if(!signal.aborted){setFile(value);setDraft(value.text);setDiscard(undefined);}}
  catch(error){if(!signal.aborted)setError(ConnectError.from(error).rawMessage);}
  finally{if(!signal.aborted)setSaving(false);}
 };
 const markdown=/\.(md|markdown|mdown)$/i.test(file?.path||path);
 const showSource=source||editing||!markdown;
 return <DraggableDialog ref={dialog} className={'file-preview-dialog'+(maximized?' maximized':'')} aria-label="File preview" onCancel={event=>{event.preventDefault();act('close');}}>
  <DialogHeader><h2 title={file?.path||path}>{file?.path||path}{dirty?' •':''}</h2><div className="file-window-actions"><button aria-label={maximized?'Restore file window':'Maximize file window'} onClick={()=>setMaximized(value=>!value)}>{maximized?'Restore':'Maximize'}</button><button aria-label="Close file preview" disabled={saving} onClick={()=>act('close')}>×</button></div></DialogHeader>
  <div className="file-preview-toolbar"><span>{editing?(dirty?'Unsaved changes':'Saved'):file?.directory?'Workspace folder':'Workspace file'}{reference.line?` · Line ${reference.line}`:''}</span><div className="file-window-actions">
   {!file?.directory&&markdown&&!editing&&<button disabled={loading} onClick={()=>setSource(value=>!value)}>{source?'Preview Markdown':'View source'}</button>}
   {editing?<><button disabled={saving} onClick={()=>act('cancel')}>Done editing</button><button className="primary" disabled={saving||!dirty} onClick={()=>void save()}>{saving?'Saving…':'Save'}</button></>:file&&!file.binary&&!file.truncated&&file.version&&<button className="primary" disabled={loading} onClick={()=>{setSource(true);setEditing(true);}}>Edit file</button>}
   {file&&file.path!=='.'&&<button disabled={saving||loading||dirty} title={dirty?'Save or discard edits before navigating':undefined} onClick={()=>setPath(file.path.includes('/')?file.path.slice(0,file.path.lastIndexOf('/')):'.')}>Parent folder</button>}
   <button disabled={saving||loading} onClick={()=>act('reload')}>Refresh file</button>
  </div></div>
  {discard&&<div className="file-unsaved" role="alert"><span>Discard your unsaved changes?</span><button onClick={()=>setDiscard(undefined)}>Keep editing</button><button onClick={()=>act(discard,true)}>Discard changes</button></div>}
  {error&&<div className="error" role="alert">{error}{dirty&&<p>Your edits are still here. Refresh to load the current file before editing again.</p>}</div>}
  <div className={'file-preview-content'+(showSource?' source-view':'')}>
   {loading&&!file?<p role="status">Loading…</p>:file?.directory?<nav className="workspace-directory" aria-label="Folder contents">{file.truncated&&<p role="status">Showing the first 500 entries.</p>}{file.entries.map(entry=><button key={entry.path} onClick={()=>setPath(entry.path)}><span aria-hidden="true">{entry.directory?'▸':'·'}</span>{entry.name}<small>{entry.directory?'Folder':'File'}</small></button>)}{!file.entries.length&&<p>This folder is empty.</p>}</nav>:file?.binary?<p>Binary file · text preview unavailable.</p>:file&&<>
    {file.truncated&&<p role="status">Preview limited to 1 MiB. Editing is unavailable.</p>}
    {showSource?<Suspense fallback={<p role="status">Loading editor…</p>}><CodeEditor path={file.path} value={draft} readOnly={!editing||saving||loading} line={reference.line} onChange={setDraft} onSave={()=>void save()}/></Suspense>:<RichContent text={file.text}/>}
   </>}
  </div>
 </DraggableDialog>;
}
