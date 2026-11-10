import type {JournalEvent} from '@lazybox/protocol';
export type ToolCall={id:string;agent:string;name:string;status:string;input?:unknown;output?:unknown;startedAt?:number;endedAt?:number};
const object=(value:unknown):Record<string,any>=>value&&typeof value==='object'&&!Array.isArray(value)?value as Record<string,any>:{};
/** Replays durable events. A missing result is never inferred to be success. */
export function toolActivity(events:JournalEvent[]) {
 const calls=new Map<string,ToolCall>(); let available:string[]=[];
 for(const {event:e,capturedAt} of events){
  const parsed=Date.parse(capturedAt),at=Number.isFinite(parsed)?parsed:undefined;
  if(!e)continue;
  if(['turn_ended','failure','session_closed'].includes(e.type)){
   for(const call of calls.values())if(call.status==='Running')call.status='Result not reported';
  }
  if(!['tool_activity','tool_catalog'].includes(e.type))continue;
  let d:Record<string,any>;try{d=object(JSON.parse(new TextDecoder().decode(e.providerDataJson)));}catch{continue;}
  if(e.type==='tool_catalog'){available=Array.isArray(d.tools)?d.tools.filter((t:unknown)=>typeof t==='string'):[];continue;}
  const agent=e.agentId||'root';
  const nativeId=d.type==='tool_result'?d.tool_use_id:d.item?.id??d.id;
  const key=`${agent}:${nativeId}`;
  const previous=calls.get(key);
  if(d.type==='tool_call'){
   const id=`${agent}:${d.id}`,old=calls.get(id);
   calls.set(id,{...old,id,agent,name:d.name||old?.name||'Tool',status:({running:'Running',completed:'Completed',failed:'Failed'} as Record<string,string>)[d.status]||'Result not reported',input:d.input??old?.input,output:d.output??old?.output});
  // Read-only compatibility for journals written before normalized tool calls.
  }else if(d.type==='tool_use'){
   const id=`${agent}:${d.id}`;const old=calls.get(id);
   calls.set(id,{...old,id,agent,name:d.name||'Tool',status:old?.status||'Running',input:d.input});
  }else if(d.type==='tool_result'){
   const id=`${agent}:${d.tool_use_id}`;const old=calls.get(id);
   calls.set(id,{...old,id,agent,name:old?.name||'Tool',status:d.is_error?'Failed':'Completed',output:d.content});
  }else if(d.item){
   const item=object(d.item);
   // Agent prose/reasoning are not tools; never render private reasoning payloads.
   if(!['commandExecution','fileChange','mcpToolCall','dynamicToolCall','webSearch','imageView','collabAgentToolCall'].includes(item.type)||!item.id)continue;
   const id=`${agent}:${item.id}`,old=calls.get(id);
   const failed=['failed','declined','error'].includes(item.status)||item.error!=null||(typeof item.exitCode==='number'&&item.exitCode!==0);
   calls.set(id,{...old,id,agent,name:item.tool||({commandExecution:'Shell command',fileChange:'File changes',webSearch:'Web search',imageView:'View image',collabAgentToolCall:'Sub-agent'} as Record<string,string>)[item.type]||item.type,
    status:failed?'Failed':e.status==='completed'||item.status==='completed'?'Completed':'Running',
    input:item.arguments??item.command??item.query??item.changes??old?.input,
    output:item.result??item.aggregatedOutput??item.error??old?.output});
  }
  const call=calls.get(key);
  if(call){
   // Journal timestamps are observations, not provider CPU execution time.
   if(call.startedAt===undefined&&call.status==='Running')call.startedAt=at;
   if(call.endedAt===undefined&&['Completed','Failed'].includes(call.status))call.endedAt=at;
   if(previous&&['Completed','Failed'].includes(previous.status)&&call.status==='Running')call.status=previous.status;
  }
 }
 return {calls:[...calls.values()],available};
}

/** Prefer the provider's action description; raw tool names are secondary. */
export function toolTitle(call:ToolCall):string {
 const input=object(call.input);
 const value=[input.description,input.title,input.command,input.file_path,input.path,input.pattern,input.query,typeof call.input==='string'?call.input:undefined].find(v=>typeof v==='string'&&v.trim());
 return typeof value==='string'?value.replace(/\s+/g,' ').trim().slice(0,160):call.name;
}
/** End-to-end observed latency, including any approval or provider waiting. */
export function toolDuration(call:ToolCall,now:number):number|undefined {
 if(call.startedAt===undefined)return;
 const end=call.endedAt??(call.status==='Running'?now:undefined);
 if(end===undefined||end<call.startedAt)return;
 return end-call.startedAt;
}
export function durationLabel(milliseconds:number|undefined):string {
 if(milliseconds===undefined||!Number.isFinite(milliseconds)||milliseconds<0)return '—';
 if(milliseconds<1000)return '<1s';
 const seconds=Math.floor(milliseconds/1000);
 if(seconds<10)return `${(milliseconds/1000).toFixed(1).replace(/\.0$/,'')}s`;
 if(seconds<60)return `${seconds}s`;
 const minutes=Math.floor(seconds/60);
 return minutes<60?`${minutes}m ${seconds%60}s`:`${Math.floor(minutes/60)}h ${minutes%60}m`;
}
