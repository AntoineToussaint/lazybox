import { follow, type RunnerClient } from '@lazybox/client-core';
import { appendTranscript, emptyTranscript } from './transcript-model';
import type { Session } from '@lazybox/protocol';

type Snapshot = ReturnType<typeof emptyTranscript> & { status: string; error: string; loading: boolean };
/** Per-client readers survive navigation, without holding provider execution alive. */
export class ConversationReader {
 snapshot: Snapshot = {...emptyTranscript(),status:'Connecting',error:'',loading:true};
 session?: Session;
 view = {scrollTop:0,following:true,visibleMessages:100};
 private listeners = new Set<()=>void>();
 private controller?: AbortController;
 identity:string;
 constructor(private api:RunnerClient,private id:string){this.identity=id;}
 getSnapshot = () => this.snapshot;
 private publish(value:Partial<Snapshot>){this.snapshot={...this.snapshot,...value};this.listeners.forEach(listener=>listener());}
 subscribe = (listener:()=>void) => {
  this.listeners.add(listener);this.start();
  return()=>{this.listeners.delete(listener);if(!this.listeners.size){this.controller?.abort();this.controller=undefined;}};
 };
 get active(){return this.listeners.size>0;}
 private start(){
  if(this.controller||!this.id||this.snapshot.status==='Complete')return;
  const controller=new AbortController();this.controller=controller;
  void follow((after,signal)=>this.api.watchEvents({sessionId:this.id,after,includePrevious:true},{signal}),controller.signal,
   event=>{if(!controller.signal.aborted)this.publish({...appendTranscript(this.snapshot,event),loading:false});},
   status=>{if(!controller.signal.aborted)this.publish({status,...(status==='Complete'?{loading:false}:{})});},
   error=>{if(!controller.signal.aborted)this.publish({error:error.message,loading:false});},this.snapshot.cursor);
 }
 retry=()=>{this.controller?.abort();this.controller=undefined;this.publish({...emptyTranscript(),error:'',loading:true,status:'Connecting'});if(this.active)this.start();};
}
const caches=new WeakMap<RunnerClient,Map<string,ConversationReader>>();
export function conversationReader(api:RunnerClient,id:string){
 let cache=caches.get(api);if(!cache){cache=new Map();caches.set(api,cache);}
 let entry=cache.get(id);if(!entry){entry=new ConversationReader(api,id);cache.set(id,entry);}
 // Keep recent inactive readers; never evict a reader mounted in another panel.
 cache.delete(id);cache.set(id,entry);
 for(const [key,value] of cache){if(cache.size<=16)break;if(key!==id&&!value.active)cache.delete(key);}
 return entry;
}

export function forgetConversation(api:RunnerClient,id:string){caches.get(api)?.delete(id);}

/** A successor extends the same contiguous Runner timeline; retain its rendered prefix. */
export function continueConversation(api:RunnerClient,previousId:string,session:Session){
 const next=conversationReader(api,session.id);
 if(session.previousExecutionId!==previousId)return next;
 const previous=conversationReader(api,previousId);
 next.identity=previous.identity;
 if(!next.active&&next.snapshot.cursor<previous.snapshot.cursor){
  next.snapshot={...previous.snapshot,status:'Connecting',error:'',loading:false};
  next.view={...previous.view};
 }
 next.session=session;
 return next;
}
