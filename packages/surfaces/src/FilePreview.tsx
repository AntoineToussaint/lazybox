import { DirectoryListing, PathCrumbs, parentPath } from './DirectoryListing';
import { Suspense, useEffect, useRef, useState } from 'react';
import {ConnectError} from '@lazybox/client-core';
import type { WorkspaceFile } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { RichContent } from './RichContent';
import type { FileLink } from './file-reference';
import type { ViewStateStore } from '@lazybox/platform';
import { useVimPreferences } from './keyboard';
import { CodeEditor, EditorSkeleton, preloadEditor } from './editor-loader';
import { useWorkbench } from './workbench';

export function FilePreview({workspaceId, reference, viewState, onClose}:{workspaceId:string;reference:FileLink;viewState?:ViewStateStore;onClose():void}) {
 const {api}=useRunner();
 const vim=useVimPreferences(viewState).editor;
 const [path,setPath]=useState(reference.path);
 const dialog=useRef<HTMLDialogElement>(null),lifetime=useRef<AbortController>(null);
 const [file,setFile]=useState<WorkspaceFile>(),[draft,setDraft]=useState(''),[error,setError]=useState('');
 const [loading,setLoading]=useState(true),[saving,setSaving]=useState(false),[editing,setEditing]=useState(false);
 const [source,setSource]=useState(!!reference.line),[maximized,setMaximized]=useState(false);
 const [discard,setDiscard]=useState<'close'|'reload'|'cancel'>();
 const dirty=!!file&&draft!==file.text;
 const workbench=useWorkbench(),editorHere=!!workbench&&workbench.workspaceId===workspaceId;
 useEffect(()=>{void preloadEditor();},[]);
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
 /** Resolves to false when the file could not be saved. */
 const save=async()=>{
  if(!file||!editing||saving)return false;
  if(!dirty)return true;
  const signal=lifetime.current!.signal;setSaving(true);setError('');
  try{const value=await api.saveWorkspaceFile({collectionId:workspaceId,path:file.path,text:draft,expectedVersion:file.version},{signal,timeoutMs:15000});if(!signal.aborted){setFile(value);setDraft(value.text);setDiscard(undefined);}return !signal.aborted;}
  catch(error){if(!signal.aborted)setError(ConnectError.from(error).rawMessage);return false;}
  finally{if(!signal.aborted)setSaving(false);}
 };
 const markdown=/\.(md|markdown|mdown)$/i.test(file?.path||path);
 const showSource=source||editing||!markdown;
 return <DraggableDialog ref={dialog} className={'file-preview-dialog'+(maximized?' maximized':'')} aria-label="File preview" onCancel={event=>{event.preventDefault();act('close');}}>
  <DialogHeader><h2 title={file?.path||path}><PathCrumbs path={file?.path||path} disabled={saving||loading||dirty} onOpen={setPath}/>{dirty?' •':''}</h2><div className="file-window-actions"><button aria-label={maximized?'Restore file window':'Maximize file window'} onClick={()=>setMaximized(value=>!value)}>{maximized?'Restore':'Maximize'}</button><button aria-label="Close file preview" disabled={saving} onClick={()=>act('close')}>×</button></div></DialogHeader>
  <div className="file-preview-toolbar"><span>{editing?(dirty?'Unsaved changes':'Saved'):file?.directory?'Workspace folder':'Workspace file'}{reference.line?` · Line ${reference.line}`:''}</span><div className="file-window-actions">
   {!file?.directory&&markdown&&!editing&&<button disabled={loading} onClick={()=>setSource(value=>!value)}>{source?'Preview Markdown':'View source'}</button>}
   {editing?<><button disabled={saving} onClick={()=>act('cancel')}>Done editing</button><button className="primary" disabled={saving||!dirty} onClick={()=>void save()}>{saving?'Saving…':'Save'}</button></>:file&&!file.binary&&!file.truncated&&file.version&&<button className="primary" disabled={loading} onClick={()=>{setSource(true);setEditing(true);}}>Edit file</button>}
   {editorHere&&file&&!file.directory&&<button disabled={saving||dirty} title={dirty?'Save or discard edits first':'Open in the Editor panel, with tabs and splits'} onClick={()=>{onClose();workbench.open(file.path,{line:reference.line,focus:true});}}>Open in editor</button>}
   {editorHere&&file?.directory&&<button disabled={saving} onClick={()=>{onClose();workbench.reveal(file.path);}}>Show in Files</button>}
   {file&&file.path!=='.'&&<button disabled={saving||loading||dirty} title={dirty?'Save or discard edits before navigating':undefined} onClick={()=>setPath(parentPath(file.path))}>Parent folder</button>}
   <button disabled={saving||loading} onClick={()=>act('reload')}>{file?.directory?'Refresh folder':'Refresh file'}</button>
  </div></div>
  {discard&&<div className="file-unsaved" role="alert"><span>Discard your unsaved changes?</span><button onClick={()=>setDiscard(undefined)}>Keep editing</button><button onClick={()=>act(discard,true)}>Discard changes</button></div>}
  {error&&<div className="error" role="alert">{error}{dirty&&<p>Your edits are still here. Refresh to load the current file before editing again.</p>}</div>}
  <div className={'file-preview-content'+(showSource?' source-view':'')}>
   {loading&&!file?<EditorSkeleton label="Opening file"/>:file?.directory?<DirectoryListing path={file.path} entries={file.entries} truncated={file.truncated} onOpen={setPath}/>:file?.binary?<p>Binary file · text preview unavailable.</p>:file&&<>
    {file.truncated&&<p role="status">Preview limited to 1 MiB. Editing is unavailable.</p>}
    {showSource?<Suspense fallback={<EditorSkeleton/>}><CodeEditor path={file.path} value={draft} readOnly={!editing||saving||loading} line={reference.line} vim={vim} onChange={setDraft} onSave={save} onQuit={force=>act('close',force)}/></Suspense>:<RichContent text={file.text}/>}
   </>}
  </div>
 </DraggableDialog>;
}
