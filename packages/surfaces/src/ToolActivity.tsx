import {memo,useEffect,useMemo,useRef,useState} from 'react';
import type {JournalEvent} from '@lazybox/protocol';
import {DraggableDialog,DialogHeader} from './DraggableDialog';
import {Icon} from './Icons';
import {CodeBlock} from './RichContent';
import {toolActivity,toolTitle,toolDuration,durationLabel,type ToolCall} from './tool-activity';
import './tool-activity.css';
import {TOOL_DETAILS_EVENT} from './ToolLine';
import {titleWhenTruncated} from './truncated-title';

function Payload({value,label}:{value:unknown;label:string}){
 const text=typeof value==='string'?value:Array.isArray(value)&&value.every(v=>v?.type==='text'&&typeof v.text==='string')?value.map(v=>v.text).join('\n'):JSON.stringify(value,null,2);
 if(text===undefined)return null;
 return <><CodeBlock text={text.slice(0,24000)} language={typeof value==='object'&&!Array.isArray(value)?'json':''} label={label}/>{text.length>24000&&<small>Preview limited to 24,000 characters.</small>}</>;
}
/** One tool call, collapsed; `nested` when it is shown inside its sub-agent's timeline. */
export function CallRow({call,now,nested=false,focused=false}:{call:ToolCall;now:number;nested?:boolean;focused?:boolean}){
 const [expanded,setExpanded]=useState(focused);
 const row=useRef<HTMLDetailsElement>(null);
 useEffect(()=>{if(focused){setExpanded(true);row.current?.scrollIntoView({block:'nearest'});row.current?.querySelector('summary')?.focus();}},[focused]);
 const title=toolTitle(call),duration=toolDuration(call,now),observed=call.startedAt??call.endedAt;
 const state=call.status==='Result not reported'?'unknown':call.status.toLowerCase();
 const input=call.input&&typeof call.input==='object'?call.input as Record<string,unknown>:undefined;
 const command=typeof input?.command==='string'?input.command:undefined;
 return <details ref={row} className="tool-call" open={expanded||undefined} onToggle={event=>setExpanded(event.currentTarget.open)}>
  <summary onClick={event => { if (event.detail > 1) event.preventDefault(); }}>
   <span className={`tool-state ${state}`} role="img" aria-label={call.status==='Result not reported'?'Completion unknown':call.status} title={call.status==='Result not reported'?'No saved completion event':call.status}>{state==='completed'?'✓':state==='failed'?'!':state==='running'?'◌':'○'}</span>
   <span className="tool-call-heading"><strong onPointerEnter={titleWhenTruncated(title)}>{title}</strong><small>{call.name}{call.agent!=='root'&&!nested?' · Sub-agent':''}{state==='running'?' · Running':''}{state==='failed'?' · Failed':''}</small></span>
   <time className="tool-time" dateTime={observed===undefined?undefined:new Date(observed).toISOString()} title={observed===undefined?'Timestamp unavailable':`${call.startedAt===undefined?'Result':'Call'} observed ${new Date(observed).toLocaleString()}`}>{observed===undefined?'—':new Date(observed).toLocaleTimeString([],{hour:'2-digit',minute:'2-digit',second:'2-digit',hour12:false})}</time>
   <span className="tool-duration" title={duration===undefined?'Duration unavailable: no recorded start/result pair':'Observed elapsed time, including waiting for approval and provider responses'}>{durationLabel(duration)}</span>
  </summary>
  {expanded&&<div className="tool-call-details">
   {call.agent!=='root'&&!nested&&<small className="tool-agent-detail">{call.agent}</small>}
   {command?<><CodeBlock text={command.slice(0,24000)} language="bash" label="Command"/><details className="tool-raw-input"><summary>Full input</summary><Payload value={call.input} label="Input JSON"/></details></>:<Payload value={call.input} label="Input"/>}
   {call.output===undefined?<p className="tool-empty">{state==='running'?'Waiting for the result…':'No output was saved for this call.'}</p>:<Payload value={call.output} label="Output"/>}
  </div>}
 </details>;
}

export const ToolActivity=memo(function ToolActivity({events,truncated=false,sessionId}:{events:JournalEvent[];truncated?:boolean;sessionId?:string}){
 const {calls,available}=useMemo(()=>toolActivity(events),[events]);
 const [open,setOpen]=useState(false),[query,setQuery]=useState(''),[limit,setLimit]=useState(100),[filter,setFilter]=useState('all'),[now,setNow]=useState(Date.now);
 const dialog=useRef<HTMLDialogElement>(null);
 // "Open full details" on a conversation's tool line opens this dialog on that call.
 const [focus,setFocus]=useState('');
 const callsRef=useRef(calls);callsRef.current=calls;
 useEffect(()=>{const show=(event:Event)=>{const request=(event as CustomEvent<{id:string;sessionId?:string}>).detail;if(request?.sessionId&&request.sessionId!==sessionId)return;const id=request?.id;const call=id&&callsRef.current.find(c=>c.id===`root:${id}`||c.id.endsWith(`:${id}`));if(!call)return;setQuery('');setFilter('all');setLimit(Number.MAX_SAFE_INTEGER);setFocus(call.id);setOpen(true);};window.addEventListener(TOOL_DETAILS_EVENT,show);return()=>window.removeEventListener(TOOL_DETAILS_EVENT,show);},[sessionId]);
 const running=calls.filter(c=>c.status==='Running').length,failed=calls.filter(c=>c.status==='Failed').length,unknown=calls.filter(c=>c.status==='Result not reported').length;
 useEffect(()=>{if(open){setNow(Date.now());dialog.current?.showModal();dialog.current?.querySelector<HTMLInputElement>('input')?.focus();}},[open]);
 useEffect(()=>{if(!open||!running)return;const timer=setInterval(()=>setNow(Date.now()),1000);return()=>clearInterval(timer);},[open,running]);
 const filtered=open?calls.filter(c=>(filter==='all'||(filter==='running'?c.status==='Running':c.status==='Failed'))&&`${toolTitle(c)} ${c.name} ${c.agent} ${c.status}`.toLowerCase().includes(query.toLowerCase())).reverse():[];
 return <div className="tool-activity-toolbar"><button type="button" onClick={()=>setOpen(true)} aria-label="View tools and activity" title={calls.length?`Activity · ${calls.length} tool ${calls.length===1?"call":"calls"}${running?`, ${running} running`:""}${failed?`, ${failed} failed`:""}`:"Activity · no tool calls yet"} data-empty={calls.length?undefined:true} data-running={running?true:undefined} data-failed={failed?true:undefined}><Icon name="activity" size={14}/>{calls.length>0&&<span className="tool-activity-count">{calls.length.toLocaleString()}<span className="tool-activity-noun"> tool {calls.length===1?'call':'calls'}</span></span>}{failed>0&&<span className="tool-activity-failed">{failed.toLocaleString()} failed</span>}</button>{open&&<DraggableDialog ref={dialog} className="tool-activity-dialog" aria-label="Tools and activity" onCancel={()=>{setOpen(false);setFocus('');}}>
  <DialogHeader><div><h2>Activity</h2><p>Tool calls · newest first</p></div><button aria-label="Close tools" onClick={()=>{setOpen(false);setFocus('');}}>×</button></DialogHeader>
  <div className="activity-controls"><input type="search" aria-label="Filter tool activity" placeholder="Search actions or tools…" value={query} onChange={e=>{setQuery(e.target.value);setLimit(100);}}/><div className="activity-filters" role="group" aria-label="Activity status">{[['all',`All ${calls.length}`],['running',`Running ${running}`],['failed',`Failed ${failed}`]].map(([value,label])=><button key={value} aria-pressed={filter===value} onClick={()=>{setFilter(value);setLimit(100);}}>{label}</button>)}</div></div>
  {unknown>0&&<p className="activity-note">{unknown} {unknown===1?'call has':'calls have'} no saved result. Completion and duration are unknown.</p>}
  <div className="activity-column-head" aria-hidden="true"><span>Action</span><span>Observed</span><span>Elapsed</span></div>
  <div className="tool-call-list">{truncated&&<p className="tool-empty">Showing the latest 1,024 retained activity events.</p>}{!filtered.length&&<p className="tool-empty">{calls.length?'No matching actions.':'No tool calls reported yet.'}</p>}{filtered.slice(0,limit).map(call=><CallRow key={call.id} call={call} now={now} focused={open&&call.id===focus}/>)}{filtered.length>limit&&<button className="activity-more" onClick={()=>setLimit(n=>n+100)}>Show earlier calls ({filtered.length-limit})</button>}</div>
  <footer><span>Timing uses Runner observations.</span><details className="available-tools"><summary>Tool inventory{available.length?` · ${available.length}`:''}</summary>{available.length?<ul>{available.map(t=><li key={t} title={t}>{t}</li>)}</ul>:<p>No inventory reported for this execution.</p>}</details></footer>
 </DraggableDialog>}</div>;
});
