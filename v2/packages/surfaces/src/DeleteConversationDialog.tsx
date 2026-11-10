import {useEffect,useRef,useState} from 'react';
import type {Session} from '@lazybox/protocol';
import {DraggableDialog,DialogHeader} from './DraggableDialog';
import {useRunner} from './RunnerContext';
import {forgetConversation} from './transcript-cache';
export function DeleteConversationDialog({session,onClose,onDeleted}:{session:Session;onClose():void;onDeleted():void}){
 const {api,refresh}=useRunner(),dialog=useRef<HTMLDialogElement>(null);
 const cancel=useRef<HTMLButtonElement>(null);
 const [busy,setBusy]=useState(false),[error,setError]=useState(''),[checked,setChecked]=useState(false),[active,setActive]=useState(!session.closed),[blocked,setBlocked]=useState(''),[stop,setStop]=useState(false);
 useEffect(()=>{
  dialog.current?.showModal();cancel.current?.focus();const control=new AbortController();
  void Promise.all([api.getSession({sessionId:session.id},{signal:control.signal}),api.listCommands({sessionId:session.id},{signal:control.signal})]).then(([result,commands])=>{
   if(control.signal.aborted)return;
   setActive(!result.session?.closed||commands.commands.some(command=>!command.done));
   setBlocked(result.session?.recoveryRequired?'Previous process ownership is unconfirmed. Reconcile this session before deleting it.':'');setChecked(true);
  },error=>{if(!control.signal.aborted)setError(String(error));});return()=>control.abort();
 },[api,session.id]);
 return <DraggableDialog ref={dialog} className="delete-conversation-dialog" aria-label="Delete conversation" onCancel={event=>{if(busy)event.preventDefault();else onClose();}}>
 <DialogHeader><h2>Delete conversation?</h2><button disabled={busy} aria-label="Cancel conversation deletion" onClick={onClose}>×</button></DialogHeader>
 <p className="deletion-subject">{session.title||'Untitled conversation'}</p><p>Its messages and uploads will be removed from Lazybox.</p><p className="deletion-note">Workspace files, accounts, usage records and provider history are kept.</p>
 {!checked&&!error&&<p role="status">Checking running resources…</p>}
 {checked&&active&&<><p>This conversation has an active session or command.</p><label className="cleanup-choice"><input type="checkbox" checked={stop} disabled={busy} onChange={e=>setStop(e.target.checked)}/>Stop its agent and running commands before deleting</label></>}
 {blocked&&<p role="alert">{blocked}</p>}{error&&<p role="alert">{error}</p>}
 <footer><button ref={cancel} autoFocus disabled={busy} onClick={onClose}>Cancel</button><button className="danger" disabled={busy||!checked||!!blocked||(active&&!stop)} onClick={async()=>{setBusy(true);setError('');try{await api.deleteConversation({sessionId:session.id,stopSessions:stop},{timeoutMs:120000});forgetConversation(api,session.id);refresh();onDeleted();}catch(error){setError(String(error));}finally{setBusy(false);}}}>{busy?'Deleting…':active?'Stop and delete':'Delete conversation'}</button></footer>
 </DraggableDialog>;
}
