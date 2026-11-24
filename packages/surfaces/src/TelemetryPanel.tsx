import { useEffect, useMemo, useId, useRef, useState, useSyncExternalStore } from 'react';
import { createPortal } from 'react-dom';
import type { ContextSnapshot, TelemetryReport } from '@lazybox/protocol';
import {useRunner} from './RunnerContext';
import type { TranscriptMessage } from './transcript-model';
import { activeSubagent, mergedStatus } from './subagent-model';
import { useSubagentView, useSubagentIndex, usePresentation } from './subagent-view';
import { describeToolBrief } from './tool-lines';
import './telemetry.css';
import { contextLevel, dollars, percentLabel, unpricedHelp, unpricedLine, usageBreakdown, type UsageBreakdown } from './usage-model';

const number=(n?:bigint)=>n===undefined?'—':n.toLocaleString();
const short=(n?:bigint)=>n===undefined?'—':new Intl.NumberFormat('en',{notation:'compact',maximumFractionDigits:1}).format(n);
const percentage=(n?:bigint,capacity?:bigint)=>n===undefined||!capacity||capacity<0n?undefined:Number(n)*100/Number(capacity);
const labels:Record<string,string>={uncached_input:'Uncached input',cached_input:'Cached input',reasoning_output:'Reasoning output',other_output:'Other output',messages:'Messages',system_tools:'System tools',mcp_tools:'MCP tools',system_prompt:'System prompt',skills:'Skills',memory_files:'Memory files',mcp_instructions:'MCP server instructions',mcp_server_instructions:'MCP server instructions',autocompact_buffer:'Compaction reserve',free_space:'Free space',mcp_tools_deferred:'MCP tools',system_tools_deferred:'System tools'};
// Chart series colours are theme tokens (chart-1 … chart-7), readable in light and dark.
const colors:Record<string,string>={uncached_input:'var(--chart-1)',cached_input:'var(--chart-2)',reasoning_output:'var(--chart-3)',other_output:'var(--chart-4)',messages:'var(--chart-1)',system_tools:'var(--chart-2)',mcp_tools:'var(--chart-3)',system_prompt:'var(--chart-4)',skills:'var(--chart-5)',memory_files:'var(--chart-6)',mcp_server_instructions:'var(--chart-7)'};
const textSize = (bytes: number) => bytes < 1000 ? `${bytes} B` : bytes < 1_000_000
 ? `${Number((bytes / 1000).toFixed(1))} kB` : `${Number((bytes / 1_000_000).toFixed(1))} MB`;

function ObservedContributors({messages}:{messages:readonly TranscriptMessage[]}) {
 const groups = useMemo(() => {
  type Item = { key: string; label: string; bytes: number; preview: string };
  const grouped = new Map<string, {name:string; bytes:number; largest:Item[]}>();
  const encoder = new TextEncoder();
  function add(name: string, item: Item) {
   const group = grouped.get(name) ?? {name, bytes:0, largest:[]};
   group.bytes += item.bytes;
   group.largest.push(item);
   group.largest.sort((a,b) => b.bytes - a.bytes);
   if (group.largest.length > 5) group.largest.pop();
   grouped.set(name, group);
  }
  for (let i = messages.length - 1; i >= 0; i--) {
   const message = messages[i];
   if (message.compaction) break;
   if (message.text && (message.type === 'operation_accepted' || message.type === 'message_delta')) {
    const name = message.type === 'message_delta' ? 'Assistant replies' : 'Your messages';
    add(name, {key:message.id, label:message.text.trim().split('\n',1)[0].slice(0,100),
     bytes:encoder.encode(message.text).length, preview:message.text.slice(0,500)});
   }
   for (const tool of message.tools || []) {
    if (!tool.output) continue;
    const title = describeToolBrief(tool);
    add('Tool outputs', {key:message.id+'/'+tool.id, label:[title.verb,title.target].filter(Boolean).join(' '),
     bytes:tool.outputBytes ?? encoder.encode(tool.output).length, preview:tool.output.slice(0,500)});
   }
  }
  return [...grouped.values()].sort((a,b)=>b.bytes-a.bytes);
 },[messages]);
 if (!groups.length) return null;
 return <section className="context-contributors" aria-label="Transcript size"><strong>Transcript size</strong>
  {groups.map(group=><details key={group.name}><summary><span>{group.name}</span><small>{textSize(group.bytes)}</small></summary>
   <div className="context-items">{group.largest.map(item=><details key={item.key}><summary><span>{item.label}</span><small>{textSize(item.bytes)}</small></summary><pre>{item.preview}</pre></details>)}</div>
  </details>)}
  <p className="telemetry-note">Loaded text since the last observed compaction. Token counts by category are unavailable.</p>
 </section>;
}

function ContextView({context:c,showAgent,messages}:{context:ContextSnapshot;showAgent:boolean;messages?:readonly TranscriptMessage[]}){
 const percent=percentage(c.used,c.capacity);
 const used=c.categories.filter(row=>row.kind==='used'||row.included);
 const deferred=c.categories.filter(row=>row.kind==='deferred');
 const reserve=c.reserve??c.categories.find(row=>row.kind==='buffer')?.tokens;
 const free=c.used!==undefined&&c.capacity!==undefined?c.capacity-c.used-(reserve??0n):undefined;
 const rows=[...used.map(row=>({name:labels[row.name]||row.name.replaceAll('_',' '),tokens:row.tokens,details:row.details,color:colors[row.name]||'var(--text-muted)'})),...(reserve!==undefined?[{name:'Compaction reserve',details:[],tokens:reserve,color:'var(--chart-muted)'}]:[]),...(free!==undefined&&free>=0n?[{name:'Free space',details:[],tokens:free,color:'var(--surface-strong)'}]:[])];
 // Provider categories are estimates. Scale only the visualization to the reported
 // used total; category counts remain unchanged and deferred tools stay outside it.
 const categoryTotal=used.reduce((sum,row)=>sum+Number(row.tokens??0n),0);
 const usedPercent=Math.max(0,Math.min(100,percent??0));
 const reservePercent=Math.max(0,Math.min(100-usedPercent,percentage(reserve,c.capacity)??0));
 const categoryRows = <div className="context-rows">{rows.map(row=><details className="context-category" key={row.name}><summary className="context-row"><span className="context-swatch" style={{background:row.color}}/><span>{row.name} <span className="context-chevron">›</span></span><span title={`${number(row.tokens)} tokens`}>{short(row.tokens)}</span><span>{percentLabel(percentage(row.tokens,c.capacity))}</span></summary><div className="context-details">{row.details?.length ? row.details.map((d,i)=><div key={i}><span>{d.name}</span><span title={`${number(d.tokens)} tokens`}>{short(d.tokens)}</span></div>) : <p>{row.name==='Free space'?'Window capacity minus used context and compaction reserve.':row.name==='Compaction reserve'?'Space reserved by the provider for compaction; it is not used context.':'The provider has not reported item-level detail for this category.'}</p>}</div></details>)}</div>;
 return <section className="context-card" aria-label={showAgent?(c.agent==='root'?'Main agent context':`${c.agent} context`):'Context breakdown'}>
  {showAgent&&<h4>{c.agent==='root'?'Main agent':c.agent}</h4>}
  <div className="context-total"><strong>{percent===undefined?'Context usage':`${percentLabel(percent)} used`}</strong><span title={`${number(c.used)} / ${number(c.capacity)} tokens`}>{short(c.used)} <span>/ {short(c.capacity)}</span></span></div>
  <div className="context-meter" role="meter" aria-label="Context used" aria-valuemin={0} aria-valuemax={c.capacity===undefined?undefined:Number(c.capacity)} aria-valuenow={c.used===undefined?undefined:Number(c.used)} aria-valuetext={`${number(c.used)} of ${number(c.capacity)} tokens`}>
   {c.source!=='sdk_token_breakdown'&&used.length&&categoryTotal>0?used.map(row=><span key={row.name} title={`${labels[row.name]||row.name}: ${number(row.tokens)} tokens`} style={{width:`${usedPercent*Number(row.tokens??0n)/categoryTotal}%`,background:colors[row.name]||'var(--text-muted)'}}/>):<span style={{width:`${usedPercent}%`,background:'var(--chart-1)'}}/>}
   <span title="Compaction reserve" style={{width:`${reservePercent}%`,background:'var(--chart-muted)'}}/>
  </div>
  {messages && <ObservedContributors messages={messages}/>}
  {c.source==='sdk_token_breakdown' ? <details className="context-request"><summary>Latest request details</summary><p className="telemetry-note">Token types from the latest Codex request.</p>{categoryRows}</details> : categoryRows}
  {!used.length&&<p className="telemetry-note">Category breakdown not reported.</p>}
  {!!deferred.length&&<div className="context-deferred"><div className="context-section-label">Deferred tools <span>Outside context</span></div>{deferred.map(row=><details className="context-category" key={row.name}><summary className="context-row"><span className="context-swatch"/><span>{labels[row.name]||row.name.replaceAll('_',' ')} <span className="context-chevron">›</span></span><span title={`${number(row.tokens)} tokens`}>{short(row.tokens)}</span><span>—</span></summary><div className="context-details">{row.details?.length?row.details.map((d,i)=><div key={i}><span>{d.name}</span><span>{short(d.tokens)}</span></div>):<p>Individual tool sizes not reported by the provider.</p>}</div></details>)}</div>}
  <footer className="context-source">{c.model&&<span>{c.model}</span>}<span title={c.observedAt?new Date(c.observedAt).toLocaleString():undefined}>{c.source==='sdk_summary_estimate'?'Provider estimate':'Provider report'}{c.observedAt?` · ${new Date(c.observedAt).toLocaleTimeString([],{hour:'2-digit',minute:'2-digit'})}`:''}</span></footer>
 </section>;
}

/** Tokens by part (input, cache read, cache write, output) and what each cost, then the total. */
function UsageTable({usage,label,compact=false}:{usage:UsageBreakdown;label?:string;compact?:boolean}){
 const priced=usage.nanos!==undefined,partial=usage.unpriced.length>0&&priced;
 const cost=(nanos?:bigint)=>nanos!==undefined?dollars(nanos):priced?'—':'';
 return <div className={'usage-table'+(compact?' compact':'')}>
  <table aria-label={label?`${label}: tokens and cost by part`:'Tokens and cost by part'}>
   {!compact&&<thead><tr><th scope="col">{label||'Usage'}</th><th scope="col">Tokens</th><th scope="col">Cost</th></tr></thead>}
   <tbody>{usage.parts.map(p=><tr key={p.key} data-part={p.key}><th scope="row" title={p.hint}>{p.label}</th><td title={p.tokens===undefined?'Not reported':`${number(p.tokens)} tokens`}>{short(p.tokens)}</td><td title={p.nanos===undefined&&priced?'Cost by part unknown: priced before Lazybox recorded parts':undefined}>{cost(p.nanos)}</td></tr>)}</tbody>
   <tfoot><tr><th scope="row">Total</th><td title={`${number(usage.tokens)} tokens`}>{short(usage.tokens)}</td><td>{priced?<>{dollars(usage.nanos)}{partial&&<span className="usage-partial" title="Covers the priced usage only">*</span>}</>:<span className="usage-unpriced-label">Not priced</span>}</td></tr></tfoot>
  </table>
  {!compact&&usage.cached!==undefined&&<p className="usage-cache"><span className="usage-cache-bar" aria-hidden="true"><i style={{width:`${Math.round(usage.cached*100)}%`}}/></span>{percentLabel(usage.cached*100)} of input came from the cache</p>}
  {!compact&&partial&&<p className="telemetry-note">* Covers the priced usage only; see what is not priced below.</p>}
 </div>;
}

/**
 * A conversation's context as its header chip last read it, and the popover
 * that chip opens: the message box shows the same number and opens the same
 * details without asking the Runner again.
 */
export type ConversationContext={percent?:number;tokensLabel?:string;popover:string};
const contextReads=new Map<string,ConversationContext>(),contextListeners=new Set<()=>void>();
function publishContext(executionId:string,read:ConversationContext|undefined){
 const previous=contextReads.get(executionId);
 if(previous?.percent===read?.percent&&previous?.tokensLabel===read?.tokensLabel&&previous?.popover===read?.popover)return;
 if(read)contextReads.set(executionId,read);else contextReads.delete(executionId);
 for(const listener of contextListeners)listener();
}
const subscribeContext=(listener:()=>void)=>{contextListeners.add(listener);return()=>{contextListeners.delete(listener);};};
export function useConversationContext(executionId:string):ConversationContext|undefined{
 return useSyncExternalStore(subscribeContext,()=>contextReads.get(executionId));
}
/**
 * Places the context popover by what opened it: under the header's chip, its
 * right edges aligned; over the message box's meter (`from: 'below'`), its left
 * edges aligned, never taller than the room above it.
 */
export function placeUsagePopover(popover:HTMLElement,bounds:DOMRect,from:'above'|'below'='above'){
 const width=Math.min(380,window.innerWidth-24);
 if(from==='below'){
  popover.style.left=`${Math.max(12,Math.min(bounds.left,window.innerWidth-width-12))}px`;
  popover.style.top='auto';popover.style.bottom=`${Math.max(12,window.innerHeight-bounds.top+8)}px`;
  popover.style.maxHeight=`${Math.max(160,Math.min(610,window.innerHeight*.7,bounds.top-20))}px`;
  return;
 }
 popover.style.left=`${Math.max(12,Math.min(bounds.right-380,window.innerWidth-392))}px`;
 popover.style.bottom='auto';popover.style.maxHeight='';
 popover.style.top=`${Math.max(12,Math.min(bounds.bottom+8,window.innerHeight-Math.min(window.innerHeight*.7,610)-12))}px`;
}

const USAGE_EVENT='lazybox-open-usage';
type UsageRequest={executionId:string;part:'context'|'cost'};
/** Opens a conversation's context and cost popover; false when no header of it is on screen. */
export function openUsage(executionId:string,part:'context'|'cost'):boolean{return !window.dispatchEvent(new CustomEvent<UsageRequest>(USAGE_EVENT,{detail:{executionId,part},cancelable:true}));}

/**
 * Presentation only: closing a panel never stops Runner accounting. Compact
 * (the chat header's chip), it shows nothing until the conversation has usage:
 * an empty grey meter says nothing (`data-empty` hides it; `/context` still opens it).
 */
export function TelemetryPanel({workspaceId,executionId,conversationId,messages,compact=false}:{messages?:readonly TranscriptMessage[];workspaceId?:string;executionId?:string;conversationId?:string;compact?:boolean}){
 const {api,info,error:connectionError}=useRunner();
 const [report,setReport]=useState<TelemetryReport>(),[error,setError]=useState(''),[open,setOpen]=useState(false),[asked,setAsked]=useState(false);
 const id=useId(),popover=useRef<HTMLDivElement>(null);
 useEffect(()=>{
  setReport(undefined);setError('');if(!workspaceId&&!executionId&&!conversationId)return;
  const controller=new AbortController();let reading=false;
  const update=async()=>{if(reading)return;reading=true;try{const r=await api.getTelemetry(conversationId?{conversation:conversationId}:executionId?{execution:executionId}:{workspace:workspaceId},{signal:controller.signal,timeoutMs:5000});if(!controller.signal.aborted){setReport(r);setError('');}}catch(e){if(!controller.signal.aborted)setError(String(e));}finally{reading=false;}};
  void update();const timer=setInterval(()=>void update(),3000);return()=>{controller.abort();clearInterval(timer);};
 },[api,workspaceId,executionId,conversationId]);
 // Another workspace or conversation closes the popover. Not conversationId: it is the execution's
 // conversation, known once the session is read (often after the popover was opened), never a new one.
 useEffect(()=>{popover.current?.hidePopover();},[workspaceId,executionId]);
 useEffect(()=>{const close=()=>popover.current?.hidePopover();window.addEventListener('resize',close);return()=>window.removeEventListener('resize',close);},[]);
 // `/cost` and `/context` (and the palette) open this conversation's popover, at the part asked for.
 const trigger=useRef<HTMLButtonElement>(null),costDetails=useRef<HTMLDetailsElement>(null);
 useEffect(()=>{
  if(!compact||!executionId)return;
  const listener=(event:Event)=>{const request=(event as CustomEvent<UsageRequest>).detail;if(request.executionId!==executionId)return;event.preventDefault();
   // Before any usage the chip is hidden: show it first, so the popover has a place to open from.
   setAsked(true);
   requestAnimationFrame(()=>{
    if(!popover.current?.matches(':popover-open'))trigger.current?.click();
    if(costDetails.current)costDetails.current.open=request.part==="cost";
    requestAnimationFrame(()=>(request.part==='cost'?costDetails.current:popover.current?.querySelector<HTMLElement>('.context-card'))?.scrollIntoView({block:'nearest'}));
   });};
  window.addEventListener(USAGE_EVENT,listener);return()=>window.removeEventListener(USAGE_EVENT,listener);
 },[compact,executionId]);
 const view=useSubagentView(), index=useSubagentIndex(view);
 usePresentation(view);
 const liveAgents=new Set<string>();
 if(view && compact) for(const id of index.order){
  const agent=index.byId[id];
  if(agent.kind==='agent' && activeSubagent(mergedStatus(agent,view.runner))){
   // Metering sees the native id before rotation adds its attempt prefix.
   liveAgents.add(agent.runnerId.replace(/^attempt-\d+:/,''));
  }
 }
 const groups=report?.groups||[];
 const contexts=(report?.contexts||[]).filter(c=>!compact || c.agent==='root' ||
  (c.scope?.execution===executionId && liveAgents.has(c.agent)))
  .filter((context,index,all)=>!conversationId||all.findIndex(value=>value.agent===context.agent)===index);
 if(compact) contexts.sort((a,b)=>Number(b.agent==='root')-Number(a.agent==='root'));
 const root=contexts.find(c=>c.agent==='root'),percent=percentage(root?.used,root?.capacity),tokensLabel=root?.used===undefined?undefined:`${short(root.used)} tokens`;
 // The header's chip is the one reader of a conversation's usage: the message box shows what it read.
 useEffect(()=>{if(compact&&executionId)publishContext(executionId,{percent,tokensLabel,popover:id});},[compact,executionId,percent,tokensLabel,id]);
 useEffect(()=>()=>{if(compact&&executionId&&contextReads.get(executionId)?.popover===id)publishContext(executionId,undefined);},[compact,executionId,id]);
 if(!workspaceId&&!executionId&&!conversationId)return <p className="module-empty">Select a workspace to view its usage.</p>;
 const known=groups.filter(g=>g.nanos!==undefined),usage=usageBreakdown(groups),cost=Number(usage.nanos??0n)/1e9;
 const costLabel=known.length?dollars(usage.nanos):groups.length?'Not priced':'—';
 const content=<>
  {error&&!connectionError&&<p role="status" className="telemetry-note">Usage unavailable; retrying automatically.</p>}
  {contexts.map(c=><ContextView key={`${c.scope?.execution}:${c.scope?.attempt}:${c.agent}`} context={c} messages={open && c.scope?.execution===executionId && c.agent==='root' && (c.scope?.provider==='codex'||c.source==='sdk_token_breakdown') ? messages : undefined} showAgent={contexts.length>1}/>)}
  {!contexts.length&&<p className="telemetry-note">{report?'Context usage not reported yet.':'Loading context…'}</p>}
  <details className="token-cost" ref={costDetails}><summary><span>Token cost</span>{usage.unpriced.length>0&&known.length>0&&<span className="token-cost-partial" title="Some usage has no price; open for which model and why">Partly priced</span>}<strong>{costLabel}</strong></summary><div className="token-cost-body">
   <p className="telemetry-note">API-equivalent estimate, separate from subscription allowance and billed spend.</p>
   {groups.length>0?<UsageTable usage={usage} label={conversationId||executionId?'This conversation':'This workspace'}/>:<p className="telemetry-note">No token usage recorded yet.</p>}
   {usage.unpriced.length>0&&<div className="usage-unpriced" role="note"><strong>Not priced</strong><ul>{usage.unpriced.map(u=><li key={`${u.model}/${u.agent}`}><span>{unpricedLine(u)}</span><small title={u.tokens===undefined?undefined:`${number(u.tokens)} tokens`}>{u.tokens===undefined?(u.records===1?'1 request':`${u.records} requests`):`${short(u.tokens)} tokens`}</small></li>)}</ul>{unpricedHelp(usage.unpriced.flatMap(u=>u.reasons))&&<p>{unpricedHelp(usage.unpriced.flatMap(u=>u.reasons))}</p>}</div>}
   {usage.incomplete>0&&<p className="telemetry-note">{usage.incomplete===1?'1 request did not report all of its usage.':`${usage.incomplete} requests did not report all of their usage.`}</p>}
   {groups.length>1&&<details className="usage-by-model"><summary>By model and agent <small>{groups.length}</small></summary>
    {groups.map((g,i)=><section className="token-cost-group" key={i}><div><strong>{g.model||'Model unavailable'}</strong><strong>{g.nanos===undefined?'Not priced':dollars(g.nanos)}</strong></div><p>{info?.profiles.find(p=>p.id===g.scope?.profile)?.label||g.scope?.profile} · {g.source==='gateway'?'Metered':'Provider'}{g.agent&&g.agent!=='root'?` · ${g.agent}`:''}</p><UsageTable usage={usageBreakdown([g])} compact/></section>)}
   </details>}
  </div></details>
 </>;
 if(!compact)return <section className="telemetry-panel" aria-label="Workspace usage">{content}</section>;
 return <div className="telemetry-panel compact"><button ref={trigger} className="context-trigger" popoverTarget={id} aria-expanded={open} data-empty={percent===undefined&&!tokensLabel&&!known.length&&!open&&!asked?true:undefined} aria-label="Context and token cost" onClick={event=>{event.currentTarget.focus();if(popover.current)placeUsagePopover(popover.current,event.currentTarget.getBoundingClientRect());}} title={`Context ${percent===undefined?(tokensLabel?`${tokensLabel} used`:'not reported yet'):`${percentLabel(percent)} used`}${known.length?` · ${costLabel}`:''}`}><span className="context-mini-bar" aria-hidden="true" data-level={percent===undefined?undefined:contextLevel(percent)}><i style={{width:`${Math.max(0,Math.min(100,percent??0))}%`}}/></span><span className="context-trigger-percent">{percent===undefined?(tokensLabel||'Context'):percentLabel(percent)}</span>{known.length>0&&<span className="context-trigger-cost">{cost>=0.01?`$${cost.toFixed(2)}`:costLabel}</span>}</button>
  {/* On the page, not in the header: a chip folded into "⋯" is hidden, and the message box opens this too. */}
  {createPortal(<div id={id} ref={popover} popover="auto" className="context-popover" onToggle={event=>{if(event.target!==event.currentTarget)return;/* a <details> inside toggled, not the popover */setOpen(event.newState==='open');if(event.newState!=='open')setAsked(false);}}><header><strong>Context</strong><button aria-label="Close context" popoverTarget={id} popoverTargetAction="hide">×</button></header>{content}</div>,document.body)}
 </div>;
}
