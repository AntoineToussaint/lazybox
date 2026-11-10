import { useEffect, useId, useRef, useState } from 'react';
import type { ContextSnapshot, TelemetryReport } from '@lazybox/protocol';
import {useRunner} from './RunnerContext';
import './telemetry.css';

const number=(n?:bigint)=>n===undefined?'—':n.toLocaleString();
const short=(n?:bigint)=>n===undefined?'—':new Intl.NumberFormat('en',{notation:'compact',maximumFractionDigits:1}).format(n);
const percentage=(n?:bigint,capacity?:bigint)=>n===undefined||!capacity||capacity<0n?undefined:Number(n)*100/Number(capacity);
const percentLabel=(n?:number)=>n===undefined?'—':`${n>0&&n<0.1?'<0.1':n.toFixed(1).replace(/\.0$/,'')}%`;
const labels:Record<string,string>={messages:'Messages',system_tools:'System tools',mcp_tools:'MCP tools',system_prompt:'System prompt',skills:'Skills',memory_files:'Memory files',mcp_instructions:'MCP server instructions',mcp_server_instructions:'MCP server instructions',autocompact_buffer:'Compaction reserve',free_space:'Free space',mcp_tools_deferred:'MCP tools',system_tools_deferred:'System tools'};
const colors:Record<string,string>={messages:'#579cf4',system_tools:'#e99557',mcp_tools:'#46bba5',system_prompt:'#d3ae4b',skills:'#ab8cde',memory_files:'#b4a6c5',mcp_server_instructions:'#799fad'};
function ContextView({context:c,showAgent}:{context:ContextSnapshot;showAgent:boolean}){
 const percent=percentage(c.used,c.capacity);
 const used=c.categories.filter(row=>row.kind==='used'||row.included);
 const deferred=c.categories.filter(row=>row.kind==='deferred');
 const reserve=c.reserve??c.categories.find(row=>row.kind==='buffer')?.tokens;
 const free=c.used!==undefined&&c.capacity!==undefined?c.capacity-c.used-(reserve??0n):undefined;
 const rows=[...used.map(row=>({name:labels[row.name]||row.name.replaceAll('_',' '),tokens:row.tokens,details:row.details,color:colors[row.name]||'#9295ad'})),...(reserve!==undefined?[{name:'Compaction reserve',details:[],tokens:reserve,color:'#777984'}]:[]),...(free!==undefined&&free>=0n?[{name:'Free space',details:[],tokens:free,color:'#34353f'}]:[])];
 // Provider categories are estimates. Scale only the visualization to the reported
 // used total; category counts remain unchanged and deferred tools stay outside it.
 const categoryTotal=used.reduce((sum,row)=>sum+Number(row.tokens??0n),0);
 const usedPercent=Math.max(0,Math.min(100,percent??0));
 const reservePercent=Math.max(0,Math.min(100-usedPercent,percentage(reserve,c.capacity)??0));
 return <section className="context-card" aria-label={showAgent?(c.agent==='root'?'Main agent context':`${c.agent} context`):'Context breakdown'}>
  {showAgent&&<h4>{c.agent==='root'?'Main agent':c.agent}</h4>}
  <div className="context-total"><strong>{percent===undefined?'Context usage':`${percentLabel(percent)} used`}</strong><span title={`${number(c.used)} / ${number(c.capacity)} tokens`}>{short(c.used)} <span>/ {short(c.capacity)}</span></span></div>
  <div className="context-meter" role="meter" aria-label="Context used" aria-valuemin={0} aria-valuemax={c.capacity===undefined?undefined:Number(c.capacity)} aria-valuenow={c.used===undefined?undefined:Number(c.used)} aria-valuetext={`${number(c.used)} of ${number(c.capacity)} tokens`}>
   {used.length&&categoryTotal>0?used.map(row=><span key={row.name} title={`${labels[row.name]||row.name}: ${number(row.tokens)} tokens`} style={{width:`${usedPercent*Number(row.tokens??0n)/categoryTotal}%`,background:colors[row.name]||'#9295ad'}}/>):<span style={{width:`${usedPercent}%`,background:'#579cf4'}}/>}
   <span title="Compaction reserve" style={{width:`${reservePercent}%`,background:'#777984'}}/>
  </div>
  <div className="context-rows">{rows.map(row=><details className="context-category" key={row.name}><summary className="context-row"><span className="context-swatch" style={{background:row.color}}/><span>{row.name} <span className="context-chevron">›</span></span><span title={`${number(row.tokens)} tokens`}>{short(row.tokens)}</span><span>{percentLabel(percentage(row.tokens,c.capacity))}</span></summary><div className="context-details">{row.details?.length ? row.details.map((d,i)=><div key={i}><span>{d.name}</span><span title={`${number(d.tokens)} tokens`}>{short(d.tokens)}</span></div>) : <p>{row.name==='Free space'?'Window capacity minus used context and compaction reserve.':row.name==='Compaction reserve'?'Space reserved by the provider for compaction; it is not used context.':'The provider has not reported item-level detail for this category.'}</p>}</div></details>)}</div>
  {!used.length&&<p className="telemetry-note">Category breakdown not reported.</p>}
  {!!deferred.length&&<div className="context-deferred"><div className="context-section-label">Deferred tools <span>Outside context</span></div>{deferred.map(row=><details className="context-category" key={row.name}><summary className="context-row"><span className="context-swatch"/><span>{labels[row.name]||row.name.replaceAll('_',' ')} <span className="context-chevron">›</span></span><span title={`${number(row.tokens)} tokens`}>{short(row.tokens)}</span><span>—</span></summary><div className="context-details">{row.details?.length?row.details.map((d,i)=><div key={i}><span>{d.name}</span><span>{short(d.tokens)}</span></div>):<p>Individual tool sizes not reported by the provider.</p>}</div></details>)}</div>}
  <footer className="context-source">{c.model&&<span>{c.model}</span>}<span title={c.observedAt?new Date(c.observedAt).toLocaleString():undefined}>{c.source==='sdk_summary_estimate'?'Provider estimate':'Provider report'}{c.observedAt?` · ${new Date(c.observedAt).toLocaleTimeString([],{hour:'2-digit',minute:'2-digit'})}`:''}</span></footer>
 </section>;
}

/** Presentation only: closing a panel never stops Runner accounting. */
export function TelemetryPanel({workspaceId,executionId,conversationId,compact=false}:{workspaceId?:string;executionId?:string;conversationId?:string;compact?:boolean}){
 const {api,info,error:connectionError}=useRunner();
 const [report,setReport]=useState<TelemetryReport>(),[error,setError]=useState(''),[open,setOpen]=useState(false);
 const id=useId(),popover=useRef<HTMLDivElement>(null);
 useEffect(()=>{
  setReport(undefined);setError('');if(!workspaceId&&!executionId&&!conversationId)return;
  const controller=new AbortController();let reading=false;
  const update=async()=>{if(reading)return;reading=true;try{const r=await api.getTelemetry(conversationId?{conversation:conversationId}:executionId?{execution:executionId}:{workspace:workspaceId},{signal:controller.signal,timeoutMs:5000});if(!controller.signal.aborted){setReport(r);setError('');}}catch(e){if(!controller.signal.aborted)setError(String(e));}finally{reading=false;}};
  void update();const timer=setInterval(()=>void update(),3000);return()=>{controller.abort();clearInterval(timer);};
 },[api,workspaceId,executionId,conversationId]);
 useEffect(()=>{popover.current?.hidePopover();},[workspaceId,executionId,conversationId]);
 useEffect(()=>{const close=()=>popover.current?.hidePopover();window.addEventListener('resize',close);return()=>window.removeEventListener('resize',close);},[]);
 if(!workspaceId&&!executionId&&!conversationId)return <p className="module-empty">Select a workspace to view its usage.</p>;
 const groups=report?.groups||[],contexts=(report?.contexts||[]).filter((context,index,all)=>!conversationId||all.findIndex(value=>value.agent===context.agent)===index);
 const known=groups.filter(g=>g.nanos!==undefined),cost=known.reduce((sum,g)=>sum+Number(g.nanos)/1e9,0);
 const unpriced=groups.reduce((sum,g)=>sum+Number(g.unpriced),0),incomplete=groups.reduce((sum,g)=>sum+Number(g.incomplete),0);
 const root=contexts.find(c=>c.agent==='root'),percent=percentage(root?.used,root?.capacity);
 const costLabel=known.length?`$${cost.toFixed(4)}`:'Not priced';
 const content=<>
  {error&&!connectionError&&<p role="status" className="telemetry-note">Usage unavailable; retrying automatically.</p>}
  {contexts.map(c=><ContextView key={`${c.scope?.execution}:${c.scope?.attempt}:${c.agent}`} context={c} showAgent={contexts.length>1}/>)}
  {!contexts.length&&<p className="telemetry-note">{report?'Context usage not reported yet.':'Loading context…'}</p>}
  <details className="token-cost"><summary><span>Token cost</span><strong>{costLabel}{unpriced>0&&known.length?' + unpriced':''}</strong></summary><div className="token-cost-body">
   <p className="telemetry-note">API-equivalent estimate, separate from subscription allowance and billed spend.</p>
   {(unpriced>0||incomplete>0)&&<p className="telemetry-note">{unpriced>0?`${unpriced} unpriced observations. `:''}{incomplete>0?`${incomplete} incomplete requests.`:''}</p>}
   {unpriced>0&&<p className="telemetry-note">Set model rates in Settings → Pricing. {Array.from(new Set((report?.recent||[]).filter(r=>r.nanos===undefined).map(r=>r.reason).filter(Boolean))).join(' · ')}</p>}
   {!groups.length&&<p className="telemetry-note">No token usage recorded yet.</p>}
   {groups.map((g,i)=><section className="token-cost-group" key={i}><div><strong>{g.model||'Model unavailable'}</strong><strong>{g.nanos===undefined?'Unpriced':`$${(Number(g.nanos)/1e9).toFixed(4)}`}</strong></div><p>{info?.profiles.find(p=>p.id===g.scope?.profile)?.label||g.scope?.profile} · {g.source==='gateway'?'Metered':'Provider'}{g.agent&&g.agent!=='root'?` · ${g.agent}`:''}</p><dl>{[['Input',g.tokens?.input],['Cache read',g.tokens?.cacheRead],['Cache write',g.tokens?.cacheWrite5m===undefined||g.tokens?.cacheWrite1h===undefined?undefined:g.tokens.cacheWrite5m+g.tokens.cacheWrite1h],['Output',g.tokens?.output]].map(([label,value])=><div key={String(label)}><dt>{label as string}</dt><dd>{short(value as bigint|undefined)}</dd></div>)}</dl></section>)}
  </div></details>
 </>;
 if(!compact)return <section className="telemetry-panel" aria-label="Workspace usage">{content}</section>;
 return <div className="telemetry-panel compact"><button className="context-trigger" popoverTarget={id} aria-expanded={open} aria-label="Context and token cost" onClick={event=>{event.currentTarget.focus();const bounds=event.currentTarget.getBoundingClientRect();if(popover.current){popover.current.style.left=`${Math.max(12,Math.min(bounds.left,window.innerWidth-392))}px`;popover.current.style.top=`${Math.max(12,Math.min(bounds.bottom+8,window.innerHeight-Math.min(window.innerHeight*.7,610)-12))}px`;}}}><span>Context</span><span className="context-mini-bar" aria-hidden="true"><i style={{width:`${Math.max(0,Math.min(100,percent??0))}%`}}/></span><span>{percentLabel(percent)}</span><span className="context-trigger-cost">{known.length?costLabel:'Usage'}</span><span aria-hidden="true">⌄</span></button>
  <div id={id} ref={popover} popover="auto" className="context-popover" onToggle={event=>setOpen(event.newState==='open')}><header><strong>Context</strong><button aria-label="Close context" popoverTarget={id} popoverTargetAction="hide">×</button></header>{content}</div>
 </div>;
}
