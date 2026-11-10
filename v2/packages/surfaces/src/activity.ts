import type { JournalEvent, Session } from '@lazybox/protocol';
/** Phase is evidence from the current turn; transport state is separate. */
export function activityLabel(events:JournalEvent[],session?:Session):string {
 if(session?.closed||session?.fault||session?.pending.length)return '';
 const root=session?.agents.find(a=>a.id==='root');
 if(root?.status==='done'&&session?.agents.some(a=>a.id!=='root'&&['running','starting'].includes(a.status)))return 'Background agents working…';
 if(session&&!session.busy&&!['running','starting'].includes(session.status))return '';
 for(let i=events.length-1;i>=0;i--){const e=events[i].event;if(!e||(e.agentId&&e.agentId!=='root'))continue;
  if(['turn_ended','session_closed','failure'].includes(e.type))return '';
  if(e.type==='message_delta')return 'Writing response…';
  if(e.type==='activity')return ({requesting:'Waiting for provider…',thinking:'Thinking…',responding:'Writing response…',compacting:'Compacting context…',working:'Working…',tool:e.name?`Running ${e.name}…`:'Running tool…'} as Record<string,string>)[e.status]||'Working…';
  if(e.type==='request_resolved')return 'Continuing after your response…';
  if(e.type==='turn_started'||e.type==='operation_accepted')return 'Working…';
 }
 return session?.busy?'Working…':'';
}
