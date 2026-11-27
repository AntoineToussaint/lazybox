import { Code, ConnectError, follow, runnerLink, type RunnerClient } from '@lazybox/client-core';
import { foldTranscript, emptyTranscript, hasAcceptedMessage, type Transcript } from './transcript-model';
import type { JournalEvent } from '@lazybox/protocol';
import type { Session } from '@lazybox/protocol';
import { humanError } from './human-error';

/** Messages mounted when a conversation opens, and added per step toward its start (useChatScroll, the phone's list). */
export const INITIAL_MESSAGES=40,EARLIER_MESSAGES=60;
/** Milliseconds between published batches while records stream in, and between progress reports of a replay. */
const PUBLISH_INTERVAL=32,PROGRESS_INTERVAL=100;
/**
 * Opening a conversation reads its latest turns first (at most about
 * TAIL_BYTES of journal), then the earlier history in pages of about
 * PAGE_BYTES. Both are ReadSessionTail reads: one response each, where a
 * WatchEvents replay is one message per record.
 */
export const TAIL_TURNS=6,TAIL_BYTES=384<<10,PAGE_BYTES=1<<20;
/** Failures a retry cannot fix: shown, as for the live stream (client-core `follow`). */
const FINAL=[Code.OutOfRange,Code.PermissionDenied,Code.FailedPrecondition,Code.ResourceExhausted,Code.NotFound];
/** How far a replay of retained history has come, while it is held back. */
export type ReplayProgress = { loaded: number; total: number };
type Snapshot = Transcript & { status: string; error: string; loading: boolean; replay?: ReplayProgress;
 /** Only the latest turns are shown: the earlier history is still being read. `loading` stays set until it is in. */
 partial?: boolean };
/** A page's records are its cursors from `start`, one after another, as the live stream checks (client-core `follow`). */
const contiguous=(events:readonly JournalEvent[],start:bigint)=>events.every((event,i)=>event.cursor===start+BigInt(i));
const GAP='Replay gap: retained cursor is not contiguous';
const pause=(ms:number,signal:AbortSignal)=>new Promise<void>(resolve=>{const done=()=>{clearTimeout(timer);signal.removeEventListener('abort',done);resolve();};const timer=setTimeout(done,ms);signal.addEventListener('abort',done,{once:true});});

/** Per-client readers survive navigation, without holding provider execution alive. */
export class ConversationReader {
 snapshot: Snapshot = {...emptyTranscript(),status:'Connecting',error:'',loading:true};
 session?: Session;
 view = {scrollTop:0,following:true,visibleMessages:INITIAL_MESSAGES,endId:"",anchorId:"",anchorOffset:0};
 private listeners = new Set<()=>void>();
 private controller?: AbortController;
 identity:string;
 constructor(private api:RunnerClient,private id:string){this.identity=id;}
 getSnapshot = () => this.snapshot;
 private publish(value:Partial<Snapshot>){this.snapshot={...this.snapshot,...value};this.listeners.forEach(listener=>listener());}
 subscribe = (listener:()=>void) => {
  this.listeners.add(listener);this.start();
  return()=>{this.listeners.delete(listener);if(!this.listeners.size){this.controller?.abort();this.controller=undefined;this.flush();}};
 };
 get active(){return this.listeners.size>0;}
 private start(){
  if(this.controller||!this.id)return;
  const live=this.snapshot.status!=='Complete';
  if(!live&&!this.since)return;
  const controller=new AbortController();this.controller=controller;
  if(this.snapshot.cursor===0n&&!this.since){void this.open(controller);return;}
  if(live)this.follow(controller);
  if(this.since)void this.complete(controller);
 }
 private follow(controller:AbortController){
  void follow((after,signal)=>this.api.watchEvents({sessionId:this.id,after,includePrevious:true,heartbeats:true},{signal}),controller.signal,
   event=>{if(!controller.signal.aborted)this.receive(event);},
   status=>{if(!controller.signal.aborted)this.status(status);},
   error=>{if(!controller.signal.aborted){this.flush();this.publish({error:humanError(error),loading:false});}},this.snapshot.cursor);
 }
 // A long conversation's whole replay took the Runner and the window most of
 // a second before anything was shown. Opening one now shows its latest turns
 // from one read, follows it live from there, and reads the earlier history
 // behind them. The fold of a conversation depends on everything before (its
 // sub-agents, slash commands, questions, review cards), so what is shown
 // from the tail is a preview: once the earlier history is in, the whole
 // conversation is folded from its first record, and everything recorded
 // since the tail's start folded again on top of it.
 /** Records from the tail's first on, while the earlier history is read; then folded again onto it. */
 private since?:JournalEvent[];
 /** The earlier history read so far (pages, newest first), back to `before`. */
 private earlier?:{before:bigint;pages:JournalEvent[][];folded:Transcript};
 private async open(controller:AbortController){
  let tail:{events:JournalEvent[];start:bigint}|undefined;
  try{tail=await this.api.readSessionTail({sessionId:this.id,turns:TAIL_TURNS,maxBytes:TAIL_BYTES},{signal:controller.signal});}
  // A Runner without the read, or one that cannot answer now: the replay
  // (which retries on its own) is how the conversation opens.
  catch{tail=undefined;}
  if(controller.signal.aborted)return;
  if(tail&&!contiguous(tail.events,tail.start)){this.publish({error:humanError(GAP),loading:false});return;}
  if(tail&&tail.events.length&&tail.start>1n){
   this.since=[...tail.events];
   this.earlier={before:tail.start,pages:[],folded:emptyTranscript()};
   this.publish({...foldTranscript(this.snapshot,tail.events),partial:true});
   this.follow(controller);
   void this.complete(controller);
   return;
  }
  // The tail is the whole conversation: it is the conversation.
  if(tail?.events.length)this.publish({...foldTranscript(this.snapshot,tail.events),loading:false});
  this.follow(controller);
 }
 /** Read the history before the tail, then show the whole conversation. */
 private async complete(controller:AbortController){
  const earlier=this.earlier!,signal=controller.signal;
  while(earlier.before>1n){
   let page:{events:JournalEvent[];start:bigint};
   try{page=await this.api.readSessionTail({sessionId:this.id,before:earlier.before,maxBytes:PAGE_BYTES},{signal});}
   catch(error){
    if(signal.aborted)return;
    const failure=ConnectError.from(error);
    if(FINAL.includes(failure.code)){this.flush();this.publish({error:humanError(failure),loading:false});return;}
    // The Runner restarting or out of reach: read the page again shortly.
    await pause(1000,signal);if(signal.aborted)return;continue;
   }
   if(signal.aborted)return;
   const events=page.events;
   if(!events.length||page.start>=earlier.before||!contiguous(events,page.start)||events.at(-1)!.cursor!==earlier.before-1n){this.flush();this.publish({error:humanError(GAP),loading:false});return;}
   earlier.pages.push(events);earlier.before=page.start;
  }
  // Fold from the first record a page at a time, yielding between pages so
  // the window keeps answering; live records go on arriving meanwhile.
  while(earlier.pages.length){
   earlier.folded=foldTranscript(earlier.folded,earlier.pages.pop()!);
   await pause(0,signal);if(signal.aborted)return;
  }
  this.flush();
  const exact=foldTranscript(earlier.folded,this.since!);
  this.since=undefined;this.earlier=undefined;
  this.publish({...exact,loading:false,partial:undefined});
 }
 // Replaying a saved conversation delivers thousands of records. Publishing
 // each one re-rendered the view per streamed delta and made opening a long
 // conversation take tens of seconds. While the Runner says retained history
 // is still coming, records are held and only progress is reported; the
 // conversation is published once, at its latest message, when the replay
 // catches up. Live records are published in batches, at most every couple
 // of frames.
 private queue:JournalEvent[]=[];
 private timer?:ReturnType<typeof setTimeout>;
 private flushed=0;
 private behind=false;
 private receive(event:JournalEvent){
  this.queue.push(event);this.since?.push(event);
  const behind=event.retained>event.cursor,caughtUp=this.behind&&!behind;this.behind=behind;
  if(caughtUp){clearTimeout(this.timer);this.timer=setTimeout(this.flush,0);}
  else this.timer??=setTimeout(this.tick,behind?PROGRESS_INTERVAL:Math.max(0,this.flushed+PUBLISH_INTERVAL-Date.now()));
 }
 private tick=()=>{
  this.timer=undefined;
  const last=this.queue.at(-1);
  if(this.behind&&last&&!this.since)this.publish({replay:{loaded:Number(last.cursor),total:Number(last.retained)}});
  else this.flush();
 };
 private status(status:string){
  if(status==='Live'&&this.snapshot.status==='Live')return;
  if(status==='Live'&&this.queue.length){this.pendingStatus=status;return;}
  this.flush();this.publish({status,...(status==='Complete'&&!this.since?{loading:false}:{})});
 }
 private pendingStatus?:string;
 /** Fold every queued record now; the cursor is current once this returns. */
 flush=()=>{
  clearTimeout(this.timer);this.timer=undefined;
  const batch=this.queue,status=this.pendingStatus;this.queue=[];this.pendingStatus=undefined;this.flushed=Date.now();
  if(batch.length)this.publish({...foldTranscript(this.snapshot,batch),...(this.since?{}:{loading:false}),replay:undefined,...(status?{status}:{})});
  else if(this.snapshot.replay)this.publish({replay:undefined});
 };
 retry=()=>{this.controller?.abort();this.controller=undefined;clearTimeout(this.timer);this.timer=undefined;this.queue=[];this.behind=false;this.pendingStatus=undefined;this.since=undefined;this.earlier=undefined;this.publish({...emptyTranscript(),error:'',loading:true,status:'Connecting',replay:undefined,partial:undefined});if(this.active)this.start();};
}
// Keyed by the window's Runner link, not one client: a restarted Runner gives
// the window a new client, and its readers continue from their cursors.
// Another Runner of the window (Dev and Working) is another link: readers are
// never shared between Runners.
const caches=new WeakMap<object,Map<string,ConversationReader>>();
const owner=(api:RunnerClient):object=>runnerLink(api)||api;
export function conversationReader(api:RunnerClient,id:string){
 let cache=caches.get(owner(api));if(!cache){cache=new Map();caches.set(owner(api),cache);}
 let entry=cache.get(id);if(!entry){entry=new ConversationReader(api,id);cache.set(id,entry);}
 // Keep recent inactive readers; never evict a reader mounted in another panel.
 cache.delete(id);cache.set(id,entry);
 for(const [key,value] of cache){if(cache.size<=16)break;if(key!==id&&!value.active)cache.delete(key);}
 return entry;
}

/** An accepted journal entry and the RPC receipt identify the same ordinary send. */
export async function confirmMessage<T>(reader: ConversationReader, operationId: string, request: Promise<T>): Promise<T | undefined> {
 let unsubscribe: (() => void) | undefined;
 try {
  return await Promise.race([request, new Promise<undefined>(resolve => {
   const check = () => { if (hasAcceptedMessage(reader.getSnapshot(), operationId)) resolve(undefined); };
   unsubscribe = reader.subscribe(check);
   check();
  })]);
 } finally { unsubscribe?.(); }
}

export function forgetConversation(api:RunnerClient,id:string){caches.get(owner(api))?.delete(id);}

/** A successor extends the same contiguous Runner timeline; retain its rendered prefix. */
export function continueConversation(api:RunnerClient,previousId:string,session:Session){
 const next=conversationReader(api,session.id);
 if(session.previousExecutionId!==previousId)return next;
 const previous=conversationReader(api,previousId);
 next.identity=previous.identity;
 // Only a whole conversation is a prefix: a preview of its latest turns is not.
 if(!next.active&&!previous.snapshot.partial&&next.snapshot.cursor<previous.snapshot.cursor){
  next.snapshot={...previous.snapshot,status:'Connecting',error:'',loading:false};
  next.view={...previous.view};
 }
 next.session=session;
 return next;
}
