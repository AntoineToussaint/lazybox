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

// Charge cached projections by the amount of journal input folded. This is a
// cache admission budget, not an estimate of the JavaScript heap.
const INACTIVE_INPUT_BUDGET=32<<20;
// Allow the most recently viewed reader alone to exceed the normal budget
// by half, so switching away briefly need not rebuild a large conversation.
const SINGLE_READER_BUDGET=48<<20;
function inputSize(event:JournalEvent){
 return 128+(event.event?.providerDataJson.length||0)+2*(event.event?.text.length||0);
}

/** Per-client readers survive navigation, without holding provider execution alive. */
export class ConversationReader {
 snapshot: Snapshot = {...emptyTranscript(),status:'Connecting',error:'',loading:true};
 session?: Session;
 view = {scrollTop:0,following:true,visibleMessages:INITIAL_MESSAGES,endId:"",anchorId:"",anchorOffset:0};
 private listeners = new Set<()=>void>();
 private controller?: AbortController;
 identity:string;
 inputBytes=0;
 constructor(private api:RunnerClient,private id:string,private onIdle?:()=>void){this.identity=id;}
 getSnapshot = () => this.snapshot;
 private publish(value:Partial<Snapshot>){this.snapshot={...this.snapshot,...value};this.listeners.forEach(listener=>listener());}
 subscribe = (listener:()=>void) => {
  this.listeners.add(listener);this.start();
  return()=>{this.listeners.delete(listener);if(!this.listeners.size){this.controller?.abort();this.controller=undefined;this.flush();this.onIdle?.();}};
 };
 get active(){return this.listeners.size>0;}
 private start(){
  if(this.controller||!this.id)return;
  const live=this.snapshot.status!=='Complete';
  if(!live&&!this.history)return;
  const controller=new AbortController();this.controller=controller;
  if(this.snapshot.cursor===0n&&!this.history){void this.open(controller);return;}
  if(live)this.follow(controller);
  if(this.history)void this.complete(controller);
 }
 private follow(controller:AbortController){
  void follow((after,signal)=>this.api.watchEvents({sessionId:this.id,after,includePrevious:true,heartbeats:true},{signal}),controller.signal,
   event=>{if(!controller.signal.aborted)this.receive(event);},
   status=>{if(!controller.signal.aborted)this.status(status);},
   error=>{if(!controller.signal.aborted){this.flush();this.publish({error:humanError(error),loading:false});}},this.snapshot.cursor);
 }
 // Rebuild the exact projection in cursor order. Live events are already in
 // the Runner journal, so the preview does not need a second copy of them.
 private history?:Transcript;
 private async open(controller:AbortController){
  let tail:{events:JournalEvent[];start:bigint}|undefined;
  try{tail=await this.api.readSessionTail({sessionId:this.id,turns:TAIL_TURNS,maxBytes:TAIL_BYTES},{signal:controller.signal});}
  // A Runner without the read, or one that cannot answer now: the replay
  // (which retries on its own) is how the conversation opens.
  catch{tail=undefined;}
  if(controller.signal.aborted)return;
  if(tail&&!contiguous(tail.events,tail.start)){this.publish({error:humanError(GAP),loading:false});return;}
  if(tail)for(const event of tail.events)this.inputBytes+=inputSize(event);
  if(tail&&tail.events.length&&tail.start>1n){
   this.history=emptyTranscript();
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
  const signal=controller.signal;
  let streamHistory=false;
  while(this.history&&!signal.aborted){
   this.flush();
   if(signal.aborted)return;
   const cursor=this.history.cursor,target=this.snapshot.cursor;
   if(cursor===target){
    const exact=this.history;
    this.history=undefined;
    this.snapshot={...exact,status:this.snapshot.status,error:this.snapshot.error,loading:false};
    this.listeners.forEach(listener=>listener());
    return;
   }
   let page:{events:JournalEvent[];start:bigint};
   try{
    if(streamHistory){
     await this.readLegacyHistory(cursor,target,signal);
     continue;
    }
    page=await this.api.readSessionTail({sessionId:this.id,fromCursor:cursor+1n,before:target+1n,maxBytes:PAGE_BYTES},{signal});
   }catch(error){
    if(signal.aborted)return;
    const failure=ConnectError.from(error);
    if(!streamHistory&&(failure.code===Code.InvalidArgument||failure.code===Code.Unimplemented)){streamHistory=true;continue;}
    if(FINAL.includes(failure.code)){this.failHistory(failure);return;}
    await pause(1000,signal);continue;
   }
   if(signal.aborted)return;
   // Older Runners ignore the forward bound and return the tail of the
   // range. Use their existing forward stream instead of probing byte sizes.
   if(page.start!==cursor+1n && page.events.at(-1)?.cursor===target){streamHistory=true;continue;}
   if(!page.events.length||page.start!==cursor+1n||!contiguous(page.events,page.start)||page.events.at(-1)!.cursor>target){
    this.failHistory(GAP);return;
   }
   this.foldHistory(page.events);
   await pause(0,signal);
  }
 }
 private foldHistory(events:JournalEvent[]){
  for(const event of events)this.inputBytes+=inputSize(event);
  this.history=foldTranscript(this.history!,events);
 }
 private async readLegacyHistory(cursor:bigint,target:bigint,signal:AbortSignal){
  const control=new AbortController();
  const stop=()=>control.abort();
  signal.addEventListener('abort',stop,{once:true});
  let batch:JournalEvent[]=[],bytes=0;
  try{
   for await(const event of this.api.watchEvents({sessionId:this.id,after:cursor,includePrevious:true},{signal:control.signal})){
    if(signal.aborted)return;
    if(event.heartbeat)continue;
    if(event.cursor!==cursor+1n)break;
    cursor=event.cursor;batch.push(event);bytes+=inputSize(event);
    if(bytes>=PAGE_BYTES||batch.length>=512||cursor===target){
     this.foldHistory(batch);batch=[];bytes=0;
    }
    if(cursor===target)return;
   }
   if(!signal.aborted)throw new ConnectError(GAP,Code.OutOfRange);
  }finally{stop();signal.removeEventListener('abort',stop);}
 }
 private failHistory(error:unknown){
  this.history=undefined;
  this.flush();
  this.publish({error:humanError(error),loading:false});
 }
 // Replaying a saved conversation delivers thousands of records. Publishing
 // each one re-rendered the view per streamed delta. Fold bounded batches
 // even when a legacy Runner replays continuously without yielding to timers.
 private queue:JournalEvent[]=[];
 private queuedBytes=0;
 private staged?:Transcript;
 private retained=0n;
 private timer?:ReturnType<typeof setTimeout>;
 private flushed=0;
 private behind=false;
 private receive(event:JournalEvent){
  this.queue.push(event);
  const bytes=inputSize(event);this.inputBytes+=bytes;this.queuedBytes+=bytes;
  this.retained=event.retained;
  if(this.queuedBytes>=PAGE_BYTES||this.queue.length>=512)this.foldQueue();
  const behind=event.retained>event.cursor,caughtUp=this.behind&&!behind;this.behind=behind;
  if(caughtUp){clearTimeout(this.timer);this.timer=setTimeout(this.flush,0);}
  else this.timer??=setTimeout(this.tick,behind?PROGRESS_INTERVAL:Math.max(0,this.flushed+PUBLISH_INTERVAL-Date.now()));
 }
 private tick=()=>{
  this.timer=undefined;
  const cursor=this.queue.at(-1)?.cursor??this.staged?.cursor;
  if(this.behind&&cursor&&!this.history)this.publish({replay:{loaded:Number(cursor),total:Number(this.retained)}});
  else this.flush();
 };
 private status(status:string){
  if(status==='Live'&&this.snapshot.status==='Live')return;
  if(status==='Live'&&(this.queue.length||this.staged)){this.pendingStatus=status;return;}
  this.flush();this.publish({status,...(status==='Complete'&&!this.history?{loading:false}:{})});
 }
 private pendingStatus?:string;
 private foldQueue(){
  if(!this.queue.length)return;
  this.staged=foldTranscript(this.staged??this.snapshot,this.queue);
  this.queue=[];
  this.queuedBytes=0;
 }
 /** Commit every delivered record, including any batches folded between renders. */
 flush=()=>{
  clearTimeout(this.timer);this.timer=undefined;
  this.foldQueue();
  const staged=this.staged,status=this.pendingStatus;
  this.staged=undefined;this.pendingStatus=undefined;this.flushed=Date.now();
  if(staged)this.publish({...staged,...(this.history||this.retained>staged.cursor?{}:{loading:false}),replay:undefined,...(status?{status}:{})});
  else if(this.snapshot.replay)this.publish({replay:undefined});
 };
 retry=()=>{
  this.controller?.abort();this.controller=undefined;
  clearTimeout(this.timer);this.timer=undefined;
  this.queue=[];this.staged=undefined;this.queuedBytes=0;this.inputBytes=0;
  this.behind=false;this.pendingStatus=undefined;this.history=undefined;
  this.snapshot={...emptyTranscript(),error:'',loading:true,status:'Connecting'};
  this.listeners.forEach(listener=>listener());
  if(this.active)this.start();
 };
}
// Keyed by the window's Runner link, not one client: a restarted Runner gives
// the window a new client, and its readers continue from their cursors.
// Another Runner of the window (Dev and Working) is another link: readers are
// never shared between Runners.
const caches=new WeakMap<object,Map<string,ConversationReader>>();
const owner=(api:RunnerClient):object=>runnerLink(api)||api;
export function conversationReader(api:RunnerClient,id:string){
 let cache=caches.get(owner(api));if(!cache){cache=new Map();caches.set(owner(api),cache);}
 let entry=cache.get(id);
 if(!entry){
  entry=new ConversationReader(api,id,()=>{
   // Keep navigation anchors even when the journal projection is evicted.
   if(!entry||cache.get(id)!==entry)return;
   cache.delete(id);cache.set(id,entry);
   let remaining=INACTIVE_INPUT_BUDGET;
   if(entry.inputBytes<=SINGLE_READER_BUDGET)remaining=Math.max(remaining,entry.inputBytes);
   for(const reader of [...cache.values()].reverse()){
    if(reader.active)continue;
    if(reader.inputBytes>remaining)reader.retry();
    else remaining-=reader.inputBytes;
   }
  });
  cache.set(id,entry);
 }
 // A lookup by an invisible panel does not count as viewing the conversation.
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
  next.inputBytes=previous.inputBytes;
  next.snapshot={...previous.snapshot,status:'Connecting',error:'',loading:false};
  next.view={...previous.view};
 }
 next.session=session;
 return next;
}
