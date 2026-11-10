import {useEffect,useMemo,useRef,useState} from 'react';
import type {JournalEvent} from '@lazybox/protocol';
import {DraggableDialog,DialogHeader} from './DraggableDialog';
import {CodeBlock} from './RichContent';
import {toolActivity,toolTitle,toolDuration,durationLabel,type ToolCall} from './tool-activity';
import './tool-activity.css';

function Payload({value,label}:{value:unknown;label:string}){
 const text=typeof value==='string'?value:Array.isArray(value)&&value.every(v=>v?.type==='text'&&typeof v.text==='string')?value.map(v=>v.text).join('\n'):JSON.stringify(value,null,2);
 if(text===undefined)return null;
 return <><CodeBlock text={text.slice(0,24000)} language={typeof value==='object'&&!Array.isArray(value)?'json':''} label={label}/>{text.length>24000&&<small>Preview limited to 24,000 characters.</small>}</>;
}
function CallRow({call,now}:{call:ToolCall;now:number}){
 const [expanded,setExpanded]=useState(false);
 const title=toolTitle(call),duration=toolDuration(call,now),observed=call.startedAt??call.endedAt;
 const state=call.status==='Result not reported'?'unknown':call.status.toLowerCase();
 const input=call.input&&typeof call.input==='object'?call.input as Record<string,unknown>:undefined;
 const command=typeof input?.command==='string'?input.command:undefined;
 return <details className="tool-call" onToggle={event=>setExpanded(event.currentTarget.open)}>
  <summary>
   <span className={`tool-state ${state}`} role="img" aria-label={call.status==='Result not reported'?'Completion unknown':call.status} title={call.status==='Result not reported'?'No saved completion event':call.status}>{state==='completed'?'✓':state==='failed'?'!':state==='running'?'◌':'○'}</span>
   <span className="tool-call-heading"><strong title={title}>{title}</strong><small>{call.name}{call.agent!=='root'?' · Sub-agent':''}{state==='running'?' · Running':''}{state==='failed'?' · Failed':''}</small></span>
   <time className="tool-time" dateTime={observed===undefined?undefined:new Date(observed).toISOString()} title={observed===undefined?'Timestamp unavailable':`${call.startedAt===undefined?'Result':'Call'} observed ${new Date(observed).toLocaleString()}`}>{observed===undefined?'—':new Date(observed).toLocaleTimeString([],{hour:'2-digit',minute:'2-digit',second:'2-digit',hour12:false})}</time>
   <span className="tool-duration" title={duration===undefined?'Duration unavailable: no recorded start/result pair':'Observed elapsed time, including waiting for approval and provider responses'}>{durationLabel(duration)}</span>
  </summary>
  {expanded&&<div className="tool-call-details">
   {call.agent!=='root'&&<small className="tool-agent-detail">{call.agent}</small>}
   {command?<><CodeBlock text={command.slice(0,24000)} language="bash" label="Command"/><details className="tool-raw-input"><summary>Full input</summary><Payload value={call.input} label="Input JSON"/></details></>:<Payload value={call.input} label="Input"/>}
   {call.output===undefined?<p className="tool-empty">{state==='running'?'Waiting for the result…':'No output was saved for this call.'}</p>:<Payload value={call.output} label="Output"/>}
  </div>}
 </details>;
}

export function ToolActivity({events,truncated=false}:{events:JournalEvent[];truncated?:boolean}){
 const {calls,available}=useMemo(()=>toolActivity(events),[events]);
 const [open,setOpen]=useState(false),[query,setQuery]=useState(''),[limit,setLimit]=useState(100),[filter,setFilter]=useState('all'),[now,setNow]=useState(Date.now);
 const dialog=useRef<HTMLDialogElement>(null);
 const running=calls.filter(c=>c.status==='Running').length,failed=calls.filter(c=>c.status==='Failed').length,unknown=calls.filter(c=>c.status==='Result not reported').length;
 useEffect(()=>{if(open){setNow(Date.now());dialog.current?.showModal();dialog.current?.querySelector<HTMLInputElement>('input')?.focus();}},[open]);
 useEffect(()=>{if(!open||!running)return;const timer=setInterval(()=>setNow(Date.now()),1000);return()=>clearInterval(timer);},[open,running]);
 const filtered=calls.filter(c=>(filter==='all'||(filter==='running'?c.status==='Running':c.status==='Failed'))&&`${toolTitle(c)} ${c.name} ${c.agent} ${c.status}`.toLowerCase().includes(query.toLowerCase())).reverse();
 return <div className="tool-activity-toolbar"><button type="button" onClick={()=>setOpen(true)} aria-label="View tools and activity">Activity <span>{calls.length}</span>{running>0&&<small> · {running} running</small>}</button>{open&&<DraggableDialog ref={dialog} className="tool-activity-dialog" aria-label="Tools and activity" onCancel={()=>setOpen(false)}>
  <DialogHeader><div><h2>Activity</h2><p>Tool calls · newest first</p></div><button aria-label="Close tools" onClick={()=>setOpen(false)}>×</button></DialogHeader>
  <div className="activity-controls"><input type="search" aria-label="Filter tool activity" placeholder="Search actions or tools…" value={query} onChange={e=>{setQuery(e.target.value);setLimit(100);}}/><div className="activity-filters" role="group" aria-label="Activity status">{[['all',`All ${calls.length}`],['running',`Running ${running}`],['failed',`Failed ${failed}`]].map(([value,label])=><button key={value} aria-pressed={filter===value} onClick={()=>{setFilter(value);setLimit(100);}}>{label}</button>)}</div></div>
  {unknown>0&&<p className="activity-note">{unknown} {unknown===1?'call has':'calls have'} no saved result. Completion and duration are unknown.</p>}
  <div className="activity-column-head" aria-hidden="true"><span>Action</span><span>Observed</span><span>Elapsed</span></div>
  <div className="tool-call-list">{truncated&&<p className="tool-empty">Showing the latest 1,024 retained activity events.</p>}{!filtered.length&&<p className="tool-empty">{calls.length?'No matching actions.':'No tool calls reported yet.'}</p>}{filtered.slice(0,limit).map(call=><CallRow key={call.id} call={call} now={now}/>)}{filtered.length>limit&&<button className="activity-more" onClick={()=>setLimit(n=>n+100)}>Show earlier calls ({filtered.length-limit})</button>}</div>
  <footer><span>Timing uses Runner observations.</span><details className="available-tools"><summary>Tool inventory{available.length?` · ${available.length}`:''}</summary>{available.length?<ul>{available.map(t=><li key={t}>{t}</li>)}</ul>:<p>No inventory reported for this execution.</p>}</details></footer>
 </DraggableDialog>}</div>;
}
