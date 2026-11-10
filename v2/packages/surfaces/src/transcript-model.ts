import type { JournalEvent } from '@lazybox/protocol';
import { providerData } from '@lazybox/client-core';

export type TranscriptMessage = { id: string; type: string; text: string; turnId: string; files?: { name: string; size: number }[] };
export type Transcript = { messages: TranscriptMessage[]; recent: JournalEvent[]; activity: JournalEvent[]; activityTruncated: boolean; cursor: bigint; appendToLast: boolean; reportedModel:string };
export const emptyTranscript = (): Transcript => ({ messages: [], recent: [], activity: [], activityTruncated: false, cursor: 0n, appendToLast: false, reportedModel:'' });

/** Fold deltas into durable message-shaped state; diagnostic events are bounded separately. */
export function appendTranscript(state: Transcript, record: JournalEvent): Transcript {
  if (record.cursor <= state.cursor) return state;
  const next = { ...state, cursor: record.cursor, recent: [...state.recent, record].slice(-80) };
  const event = record.event;
  if(event&&['tool_catalog','tool_activity','turn_ended','failure','session_closed'].includes(event.type)){
    next.activity=[...state.activity,boundedActivity(record)];
    if(next.activity.length>1024){next.activity=next.activity.slice(-1024);next.activityTruncated=true;}
  }
  if (!event || (event.agentId && event.agentId !== 'root')) return next;
  if(['session_ready','context','usage'].includes(event.type)) {
    const model=providerData(event.providerDataJson).model;
    if(typeof model==='string'&&model.trim())next.reportedModel=model;
  }
  const notice = runnerNotice(record);
  if (notice) {
    next.messages = [...state.messages, { id: record.cursor.toString(), type: 'notice', text: notice, turnId: event.turnId }];
    next.appendToLast = false;
    return next;
  }
  const files = event.type === 'operation_accepted' ? providerData(event.providerDataJson).files as TranscriptMessage['files'] : undefined;
  const last = state.messages.at(-1);
  if (event.type === 'message_delta' && event.text) {
    if (state.appendToLast && last?.type === 'message_delta' && last.turnId === event.turnId) {
      next.messages = [...state.messages.slice(0, -1), { ...last, text: last.text + event.text }];
    } else next.messages = [...state.messages, { id: record.cursor.toString(), type: event.type, text: event.text, turnId: event.turnId }];
    next.appendToLast = true;
  } else if (['operation_accepted', 'failure'].includes(event.type) && (event.text || files?.length)) {
    next.messages = [...state.messages, { id: record.cursor.toString(), type: event.type, text: event.text, turnId: event.turnId, files }];
    next.appendToLast = false;
  } else if (['turn_started', 'turn_ended', 'session_closed', 'approval', 'question', 'request_resolved'].includes(event.type) || (event.type === 'activity' && event.status === 'tool')) {
    next.appendToLast = false;
  }
  return next;
}

/**
 * Runner restarts are not interruptions: agents and commands keep running and
 * the next Runner re-attaches. What a restart can leave uncertain is said
 * plainly, in the conversation, where the user will look for it.
 */
export function runnerNotice(record: JournalEvent): string {
  const event = record.event;
  if (record.receipt?.delivery === 'unknown_after_runner_restart')
    return 'The Runner restarted before it could confirm this message reached the agent. It was not sent again; resend it if it is still needed.';
  if (event?.type === 'runner_reattached')
    return 'The Runner restarted and re-attached to this agent. Nothing was interrupted.';
  if (event?.type === 'request_resolved' && event.status === 'outcome_unknown_after_restart')
    return 'The Runner restarted while a command the agent started was in flight. Its outcome is unknown, so it was not run again; the agent was told to check before retrying.';
  return '';
}

// Tool results can contain images or megabytes of logs. Keep a bounded display
// projection while the Runner journal owns the original evidence.
function boundedActivity(record:JournalEvent):JournalEvent {
 const event=record.event;if(!event||event.providerDataJson.length<=32768)return record;
 let raw:any;try{raw=JSON.parse(new TextDecoder().decode(event.providerDataJson));}catch{return {...record,event:{...event,providerDataJson:new Uint8Array()}};}
 const limit=(v:any,depth=0):any=>typeof v==='string'?v.length>4096?v.slice(0,4096)+'\n[Preview truncated]':v:depth>5?'[Nested content omitted]':Array.isArray(v)?v.slice(0,20).map(x=>limit(x,depth+1)):v&&typeof v==='object'?Object.fromEntries(Object.entries(v).slice(0,30).map(([k,x])=>[k,limit(x,depth+1)])):v;
 let data=new TextEncoder().encode(JSON.stringify(limit(raw)));
 if(data.length>32768){const item=raw.item;raw=item?{item:{id:item.id,type:item.type,tool:item.tool,status:item.status,exitCode:item.exitCode,result:'[Large payload omitted]'}}:{type:raw.type,id:raw.id,tool_use_id:raw.tool_use_id,name:raw.name,status:raw.status,is_error:raw.is_error,output:raw.output===undefined?undefined:'[Large payload omitted]',input:'[Large payload omitted]',content:'[Large payload omitted]'};data=new TextEncoder().encode(JSON.stringify(raw));}
 return {...record,event:{...event,providerDataJson:data}};
}
