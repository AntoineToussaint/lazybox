import { conversationName } from './conversation-model';
import {useEffect,useState} from 'react';
import type {Session} from '@lazybox/protocol';
import {ConfirmDialog} from './ConfirmDialog';
import {useRunner} from './RunnerContext';
import {forgetConversation} from './transcript-cache';
// Its tabs leave every page of this window (conversation-requests.ts).
import {conversationOpeners} from './conversation-requests';
export function DeleteConversationDialog({session,onClose,onDeleted}:{session:Session;onClose():void;onDeleted():void}){
 const {api,refresh}=useRunner();
 const [error,setError]=useState(''),[checked,setChecked]=useState(false),[active,setActive]=useState(!session.closed),[blocked,setBlocked]=useState(''),[stop,setStop]=useState(false);
 useEffect(()=>{
  const control=new AbortController();
  void Promise.all([api.getSession({sessionId:session.id},{signal:control.signal}),api.listCommands({sessionId:session.id},{signal:control.signal})]).then(([result,commands])=>{
   if(control.signal.aborted)return;
   setActive(!result.session?.closed||commands.commands.some(command=>!command.done));
   setBlocked(result.session?.recoveryRequired?'Previous process ownership is unconfirmed. Reconcile this session before deleting it.':'');setChecked(true);
  },error=>{if(!control.signal.aborted)setError(String(error));});return()=>control.abort();
 },[api,session.id]);
 const live=checked&&active;
 return <ConfirmDialog title="Delete conversation?" label="Delete conversation" closeLabel="Cancel conversation deletion" subject={conversationName(session,'Untitled conversation')}
  kept="Workspace files, accounts, usage records and provider history are kept."
  status={!checked&&!error?'Checking running resources…':undefined} error={blocked||error}
  warning={live?'This conversation has an active session or command.':undefined}
  check={live?{label:'Stop its agent and running commands before deleting',checked:stop,onChange:setStop,required:true}:undefined}
  canConfirm={checked&&!blocked} confirm={live?'Stop and delete':'Delete conversation'} working="Deleting…" onClose={onClose}
  onConfirm={async()=>{await api.deleteConversation({sessionId:session.id,stopSessions:stop},{timeoutMs:120000});forgetConversation(api,session.id);conversationOpeners()?.forget(session.id);refresh();onDeleted();}}>
  Its messages and uploads will be removed from Lazybox.
 </ConfirmDialog>;
}
