import {useCallback,useLayoutEffect,useMemo,useRef,useState,type UIEvent} from 'react';
import {EARLIER_MESSAGES,INITIAL_MESSAGES,type ConversationReader} from './transcript-cache';
/** How close to the bottom a reader scrolling down picks up following again. */
const FOLLOW_THRESHOLD=48;

/** The reader's own scrolling: which way, until when a scroll is theirs, and
 * whether it aims at the bottom (measured when they asked, as a reply streaming
 * in can grow the content before the scroll lands). */
type Intent={direction:'up'|'down'|'any';until:number;bottom?:boolean;touch?:boolean};
const UP_KEYS=new Set(['PageUp','ArrowUp','Home']),DOWN_KEYS=new Set(['PageDown','ArrowDown','End',' ']);

/** A scroller between the event target and the pane that takes this scroll itself (a code block, a console). */
function innerScroller(target:EventTarget|null,pane:HTMLElement,up:boolean){
 for(let node=target instanceof Element?target:null;node&&node!==pane;node=node.parentElement){
  if(node.scrollHeight<=node.clientHeight+1)continue;
  const style=getComputedStyle(node);if(!/auto|scroll|overlay/.test(style.overflowY))continue;
  if(style.overscrollBehaviorY!=='auto'||(up?node.scrollTop>0:node.scrollTop+node.clientHeight<node.scrollHeight-1))return true;
 }
 return false;
}
const editable=(target:EventTarget|null)=>target instanceof HTMLElement&&(target.isContentEditable||/^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName));

/** Follow the rendered content, including asynchronous diagrams and viewport changes.
 * Earlier history is mounted as the reader nears the top, keeping what they look at in place.
 *
 * The reader's scrolling always wins: a wheel, key, touch or pointer (the
 * scrollbar, a selection) that moves the pane up stops following at once, even
 * while a reply streams in, and only moving back to the bottom (or Jump to
 * latest, or sending) resumes it. Scrolls the reader did not make, such as the
 * browser clamping after a resize or our own pinning, never change it. */
export function useChatScroll(reader:ConversationReader,messages:readonly {id:string}[],pendingCount:number|undefined,shown:boolean){
 const paneRef=useRef<HTMLDivElement>(null),contentRef=useRef<HTMLDivElement>(null);
 const waitingForAnchor=useRef(false);
 const following=useRef(reader.view.following);
 const [followingLatest,setFollowingLatest]=useState(reader.view.following);
 const [visibleMessages,setVisibleMessages]=useState(reader.view.visibleMessages);
 // A detached reader keeps a window ending at a particular message. New
 // activity and the completion of older history cannot shift that window.
 const [endId,setEndId]=useState(reader.view.endId);
 const end=useMemo(()=>{
  if(!endId)return messages.length;
  const index=messages.findIndex(message=>message.id===endId);
  return index<0?messages.length:index+1;
 },[endId,messages]);
 const range=useRef({messages,end,visibleMessages});range.current={messages,end,visibleMessages};
 const moreBelow=end<messages.length;
 const limit=EARLIER_MESSAGES*3;
 const pinAfterTrim=useRef(false);
 const prependHeight=useRef<number|undefined>(undefined);
 const dimensions=useRef({height:0,width:0,content:0});
 const hidden=useRef(0);hidden.current=Math.max(0,end-visibleMessages);
 const measure=(pane:HTMLDivElement)=>{dimensions.current={height:pane.clientHeight,width:pane.clientWidth,content:pane.scrollHeight};};
 const resized=(pane:HTMLDivElement)=>dimensions.current.height!==pane.clientHeight||dimensions.current.width!==pane.clientWidth;
 // Where the pane was last left, by the reader or by us: a scroll event's
 // movement is measured from it, so our own scrolling never reads as theirs.
 const lastTop=useRef(0);
 const place=(pane:HTMLDivElement,top:number)=>{pane.scrollTop=top;lastTop.current=pane.scrollTop;};
 const intent=useRef<Intent|undefined>(undefined),pressed=useRef<'pointer'|'touch'|undefined>(undefined);
 const readerScrolling=()=>!!pressed.current||(!!intent.current&&performance.now()<intent.current.until);
 // Wheel, key and touch gestures can page ahead. A scrollbar drag cannot:
 // the browser maps the thumb to the content height it had at the start.
 // Some engines never send pointer events for a native scrollbar; an
 // unclassified scroll should therefore keep its range until the edge.
 const canPageAhead=(direction:'up'|'down')=>pressed.current==='touch'||(!pressed.current&&readerScrolling()&&
  (intent.current?.direction===direction||!!intent.current?.touch));
 // WebKit has no scroll anchoring (and Chromium's is off on the pane). While
 // the reader is not following, the element at the top of the viewport keeps
 // its place when content above it changes height, such as an image or a
 // diagram rendering in an earlier message, or a tool group folding.
 const anchor=useRef<{node:Element;offset:number}|undefined>(undefined);
 const capture=(pane:HTMLDivElement)=>{
  if(waitingForAnchor.current)return;
  anchor.current=undefined;
  const rect=pane.getBoundingClientRect(),top=rect.top;
  const hit=pane.ownerDocument.elementFromPoint(rect.left+pane.clientWidth/2,top+1);
  const row=hit?.closest<HTMLElement>('[data-message-id]');
  if(hit&&row&&contentRef.current?.contains(row)){
   reader.view.anchorId=row.dataset.messageId||'';
   reader.view.anchorOffset=row.getBoundingClientRect().top-top;
   anchor.current={node:hit,offset:hit.getBoundingClientRect().top-top};
   return;
  }
  // Padding, gaps and paging controls can occupy the top edge. A paging
  // control stays put when history is prepended, while the messages move.
  for(const message of contentRef.current?.querySelectorAll<HTMLElement>('[data-message-id]')??[]){
   const box=message.getBoundingClientRect();
   if(box.height>0&&box.bottom>top&&box.top<rect.bottom){
    reader.view.anchorId=message.dataset.messageId||'';
    reader.view.anchorOffset=box.top-top;
    anchor.current={node:message,offset:box.top-top};
    return;
   }
  }
  reader.view.anchorId='';
 };
 const restore=(pane:HTMLDivElement)=>{
  const held=anchor.current;
  // Replaying a turn or rendering rich text can replace the element the
  // reader was looking at. Its message ID survives the DOM replacement.
  const live=held&&contentRef.current?.contains(held.node);
  const node=live?held.node:reader.view.anchorId&&contentRef.current?.querySelector<HTMLElement>(`[data-message-id="${CSS.escape(reader.view.anchorId)}"]`);
  if(!node)return false;
  const offset=live?held.offset:reader.view.anchorOffset;
  const shift=node.getBoundingClientRect().top-pane.getBoundingClientRect().top-offset;
  if(Math.abs(shift)>=1)place(pane,pane.scrollTop+shift);
  if(!live)capture(pane);
  return true;
 };
 const pin=(pane:HTMLDivElement)=>{place(pane,pane.scrollHeight);measure(pane);};
 // A move up and away from the bottom in a pane that kept its size is no
 // layout scroll (those only clamp to the bottom): it is the reader's, perhaps
 // through a scrollbar an engine reports no pointer for.
 const readerMovedUp=(pane:HTMLDivElement)=>!resized(pane)&&pane.scrollTop<lastTop.current-1&&pane.scrollHeight-pane.scrollTop-pane.clientHeight>=FOLLOW_THRESHOLD;
 // New content follows, unless the reader moved up by a scroll whose event has
 // not arrived yet: pinning now would undo it.
 const follow=(pane:HTMLDivElement)=>{
  if(readerMovedUp(pane)){setFollowing(false);capture(pane);measure(pane);}
  else pin(pane);
 };
 const setFollowing=useCallback((value:boolean)=>{
  if(following.current!==value){
   if(value){setEndId('');reader.view.anchorId='';}
   else {const {messages,end}=range.current;setEndId(messages[end-1]?.id||'');}
  }
  following.current=value;reader.view.following=value;setFollowingLatest(value);
 },[reader]);
 const loadEarlier=useCallback((pane:HTMLDivElement|null)=>{
  if(!hidden.current||prependHeight.current!==undefined)return;
  const {messages,end,visibleMessages}=range.current;
  const start=Math.max(0,end-(following.current?Math.min(limit,visibleMessages+EARLIER_MESSAGES):visibleMessages+EARLIER_MESSAGES));
  prependHeight.current=pane?.scrollHeight;
  if(pane)capture(pane);
  const nextEnd=Math.min(end,start+limit);
  // A short tail may auto-fill the viewport while still following live output.
  if(nextEnd<messages.length)setFollowing(false);
  setEndId(following.current?'':messages[nextEnd-1]?.id||'');setVisibleMessages(nextEnd-start);
 },[reader,setFollowing]);
 // Within a screen of the top, the next step of history is mounted before the reader reaches it.
 const nearTop=(pane:HTMLDivElement)=>hidden.current>0&&(following.current
  ? pane.scrollHeight<=pane.clientHeight+1&&range.current.visibleMessages<limit
  : pane.scrollTop<=1||(canPageAhead('up')&&pane.scrollTop<Math.max(400,pane.clientHeight)));
 useLayoutEffect(()=>{
  if(!shown)return;
  following.current=reader.view.following;setFollowingLatest(reader.view.following);setVisibleMessages(reader.view.visibleMessages);setEndId(reader.view.endId);
  const pane=paneRef.current,content=contentRef.current;if(!pane||!content)return;
  waitingForAnchor.current=!!reader.view.anchorId&&!reader.view.following&&reader.snapshot.loading;
  place(pane,reader.view.following?pane.scrollHeight:reader.view.scrollTop);measure(pane);
  if(!reader.view.following){
   const row=reader.view.anchorId&&content.querySelector<HTMLElement>(`[data-message-id="${CSS.escape(reader.view.anchorId)}"]`);
   if(row)place(pane,pane.scrollTop+row.getBoundingClientRect().top-pane.getBoundingClientRect().top-reader.view.anchorOffset);
   capture(pane);
  }
  const resize=new ResizeObserver(()=>{
   // A compositor scroll can land before its scroll event. Keep the new
   // reading position when a streamed delta resizes the content in between.
   if(following.current)follow(pane);
   else{
    if(readerScrolling() && Math.abs(pane.scrollTop-lastTop.current)>=1)capture(pane);
    else restore(pane);
    measure(pane);
   }
  });
  resize.observe(pane);resize.observe(content);
  // The reader's intent arrives before the scroll it causes. Stopping on it
  // means a delta rendered in between cannot pin the pane back down.
  const intend=(direction:Intent['direction'],ms:number,distance=0)=>{
   waitingForAnchor.current=false;
   intent.current={direction,until:performance.now()+ms,bottom:direction==='down'&&pane.scrollTop+distance>=pane.scrollHeight-pane.clientHeight-FOLLOW_THRESHOLD};
   if(direction==='up'&&following.current&&pane.scrollTop>0){setFollowing(false);capture(pane);}
  };
  // A compact history window can end without any room left to scroll.
  // In that case the user's input is the only signal: no scroll event fires.
  const resumeAtEdge=()=>{
   if(range.current.end>=range.current.messages.length||pane.scrollHeight-pane.clientHeight-pane.scrollTop>1)return false;
   jumpRef.current(range.current.visibleMessages);
   return true;
  };
  const wheel=(event:WheelEvent)=>{
   if(!event.deltaY||event.ctrlKey||innerScroller(event.target,pane,event.deltaY<0))return;
   if(event.deltaY>0&&resumeAtEdge())return;
   intend(event.deltaY<0?'up':'down',300,event.deltaY*(event.deltaMode===1?16:event.deltaMode===2?pane.clientHeight:1));
  };
  // On the window, after the app's own handlers: a key one of them used is not a scroll.
  const key=(event:KeyboardEvent)=>{
   if(event.defaultPrevented||event.altKey||event.ctrlKey||event.metaKey||!pane.contains(event.target as Node)||editable(event.target))return;
   // End is an explicit request to follow the latest output. Do not wait
   // for native smooth scrolling: streaming can move its target meanwhile.
   if(event.key==='End'&&!innerScroller(event.target,pane,false)){
    event.preventDefault();jumpRef.current();return;
   }
   const up=UP_KEYS.has(event.key)||(event.key===' '&&event.shiftKey);
   if(!(up||DOWN_KEYS.has(event.key))||innerScroller(event.target,pane,up))return;
   if(event.key===' '&&(event.target as Element)?.closest?.('button, a[href], summary, [role="button"]'))return;
   if(!up&&resumeAtEdge()){event.preventDefault();return;}
   intend(up?'up':'down',800,event.key==='ArrowDown'?40:pane.clientHeight*.85);
  };
  // A held pointer or finger (the scrollbar, a selection, a swipe) makes every
  // scroll the reader's, and so does the momentum after it. Touch is followed
  // by its own events: the browser cancels the pointer once a finger pans.
  const press=(event:PointerEvent)=>{if(event.pointerType!=='touch'&&event.button===0){waitingForAnchor.current=false;pressed.current='pointer';intent.current=undefined;}};
  const touch=()=>{waitingForAnchor.current=false;pressed.current='touch';};
  const release=(ms:number)=>{if(!pressed.current)return;intent.current={direction:'any',until:performance.now()+ms,touch:pressed.current==='touch'};pressed.current=undefined;};
  const releasePointer=(event:PointerEvent)=>{if(event.pointerType!=='touch')release(150);},releaseTouch=()=>release(1500);
  // A scrollbar drag may end without a pointerup reaching the page.
  const hover=(event:PointerEvent)=>{if(event.pointerType!=='touch'&&!event.buttons)release(150);};
  pane.addEventListener('wheel',wheel,{passive:true});pane.addEventListener('pointerdown',press);pane.addEventListener('touchstart',touch,{passive:true});
  window.addEventListener('keydown',key);window.addEventListener('pointerup',releasePointer);window.addEventListener('pointercancel',releasePointer);window.addEventListener('pointermove',hover);
  window.addEventListener('touchend',releaseTouch);window.addEventListener('touchcancel',releaseTouch);
  return()=>{
   resize.disconnect();anchor.current=undefined;
   if(!waitingForAnchor.current)reader.view.scrollTop=lastTop.current;
   reader.view.following=following.current;
   pane.removeEventListener('wheel',wheel);pane.removeEventListener('pointerdown',press);pane.removeEventListener('touchstart',touch);
   window.removeEventListener('keydown',key);window.removeEventListener('pointerup',releasePointer);window.removeEventListener('pointercancel',releasePointer);window.removeEventListener('pointermove',hover);
   window.removeEventListener('touchend',releaseTouch);window.removeEventListener('touchcancel',releaseTouch);
  };
 },[reader,setFollowing,shown]);
 useLayoutEffect(()=>{
  const pane=paneRef.current;if(!shown||!pane)return;
  if(waitingForAnchor.current){
   const row=reader.view.anchorId&&contentRef.current?.querySelector<HTMLElement>(`[data-message-id="${CSS.escape(reader.view.anchorId)}"]`);
   if(!row&&reader.snapshot.loading)return;
   waitingForAnchor.current=false;
   if(row)place(pane,pane.scrollTop+row.getBoundingClientRect().top-pane.getBoundingClientRect().top-reader.view.anchorOffset);
   else place(pane,reader.view.scrollTop);
   measure(pane);capture(pane);
  }
  if(pinAfterTrim.current){pinAfterTrim.current=false;if(following.current)pin(pane);}
  else if(prependHeight.current!==undefined){if(following.current)pin(pane);else if(!restore(pane))place(pane,pane.scrollTop+pane.scrollHeight-prependHeight.current);prependHeight.current=undefined;measure(pane);capture(pane);}
  else if(following.current)follow(pane);
  reader.view.visibleMessages=visibleMessages;reader.view.endId=endId;
  if(reveal.current&&revealNow())return;
  if(nearTop(pane)){
   // Many hidden rows can leave successive pages shorter than the viewport.
   // Let layout and input settle between pages.
   const frame=requestAnimationFrame(()=>{if(nearTop(pane))loadEarlier(pane);});
   return()=>cancelAnimationFrame(frame);
  }
 },[reader,messages,visibleMessages,endId,pendingCount,loadEarlier,shown]);
 const loadLater=useCallback((scrolling:boolean)=>{
  const pane=paneRef.current;
  const {messages,end,visibleMessages}=range.current;
  if(!pane||end>=messages.length||prependHeight.current!==undefined)return false;
  let step=EARLIER_MESSAGES,drop=0;
  if(scrolling){
   // Slide a full window forward instead of doubling the scrollbar range
   // when paging near the bottom. Keep the visible rows and some history
   // above the viewport so that reversing direction can still scroll.
   const paneTop=pane.getBoundingClientRect().top;
   const first=Array.from(pane.querySelectorAll<HTMLElement>('[data-message-id]')).find(row=>row.getBoundingClientRect().bottom>paneTop);
   const start=Math.max(0,end-visibleMessages);
   const above=messages.slice(start,end).findIndex(message=>message.id===first?.dataset.messageId);
   if(above<2&&visibleMessages>=limit)return false;
   if(above>1){
    step=Math.min(step,Math.floor(above/2));
    drop=Math.min(step,messages.length-end);
   }
  }
  capture(pane);prependHeight.current=pane.scrollHeight;
  const nextEnd=Math.min(messages.length,end+step);
  setVisibleMessages(Math.min(limit,Math.min(end,visibleMessages)+nextEnd-end-drop));
  setEndId(messages[nextEnd-1].id);
  return true;
 },[reader]);
 const showLater=()=>loadLater(false);
 const onScroll=(event:UIEvent<HTMLDivElement>)=>{
  if(!shown)return;
  const pane=event.currentTarget,top=pane.scrollTop,moved=top-lastTop.current,movedUp=readerMovedUp(pane);
  if(waitingForAnchor.current){
   // Ignore our own placement and layout clamps. A real move in the
   // preview (including find-in-page or a native scrollbar) supersedes
   // the saved position even while the Runner is unavailable.
   if(Math.abs(moved)<1||resized(pane)||dimensions.current.content!==pane.scrollHeight)return;
   waitingForAnchor.current=false;
  }
  lastTop.current=top;
  const gap=pane.scrollHeight-top-pane.clientHeight;
  // Paging preserves the visible row while reading. Once the reader reaches
  // the bottom, preserving that row would move them back into history.
  if(moreBelow&&moved>0&&(gap<=1||(readerScrolling()&&intent.current?.direction==='down'&&intent.current.bottom))){
   jumpRef.current(visibleMessages);return;
  }
  if(moreBelow&&moved>0&&canPageAhead('down')&&gap<Math.max(400,pane.clientHeight)&&loadLater(true))return;
  if(readerScrolling()){
   // Up by any amount stops following, even within the threshold; down to near the bottom resumes it.
   // A pane clamped to its new bottom (content folding below), or moved by its
   // own resize, is not the reader moving.
   if(moved<0&&gap>1&&!resized(pane))setFollowing(false);
   // A compositor/anchor correction during an upward gesture is not a
   // request to follow. Only downward intent gets the near-bottom tolerance.
   else if(!moreBelow&&moved>0&&intent.current?.direction!=='up'&&(gap<=1||(intent.current?.direction==='down'&&(gap<FOLLOW_THRESHOLD||intent.current.bottom))))setFollowing(true);
  }else if(following.current){
   if(movedUp)setFollowing(false);
   // Browser layout scrolls are not evidence that the user stopped following.
   else if(resized(pane)||dimensions.current.content!==pane.scrollHeight){pin(pane);return;}
   // A scroll nobody pressed for (find in page) that leaves the bottom.
   else if(gap>=FOLLOW_THRESHOLD)setFollowing(false);
  }else if(!moreBelow&&moved>0&&gap<=1)setFollowing(true);
  measure(pane);reader.view.scrollTop=top;
  if(following.current)anchor.current=undefined;else capture(pane);
  if(nearTop(pane))loadEarlier(pane);
 };
 const goToTail=(count=INITIAL_MESSAGES)=>{
  waitingForAnchor.current=false;intent.current=undefined;pressed.current=undefined;anchor.current=undefined;
  reveal.current=undefined;prependHeight.current=undefined;
  setFollowing(true);
  if(paneRef.current)pin(paneRef.current);
  if(endId||visibleMessages!==count){
   // Dropping earlier rows changes the scroll range. Pin after React commits
   // the short tail, before a resize observer can interpret the clamp as input.
   pinAfterTrim.current=true;
   setEndId('');setVisibleMessages(count);
  }
 };
 // A normal downward scroll keeps the mounted window size. Only an
 // explicit jump trims expanded history back to the initial tail.
 const jumpToLatest=()=>goToTail();
 const jumpRef=useRef(goToTail);jumpRef.current=goToTail;
 // Jumping to a message (a prompt) may need earlier history mounted first:
 // widen the window, then scroll it to the top once it is rendered.
 const reveal=useRef<string|undefined>(undefined);
 const revealNow=()=>{
  const id=reveal.current,pane=paneRef.current;if(!id||!pane)return false;
  const node=contentRef.current?.querySelector<HTMLElement>(`[data-message-id="${CSS.escape(id)}"]`);if(!node)return false;
  reveal.current=undefined;
  const decision=node.querySelector('[data-terminal-decision]');
  const header=node.getBoundingClientRect().top-12;
  // Keep the command in view. For a taller card, move just far enough to reach its controls.
  const actions=decision && pane.parentElement?.querySelector('.chat-scroll-actions');
  const paneTop=pane.getBoundingClientRect().top;
  const available=actions?Math.min(pane.clientHeight,actions.getBoundingClientRect().top-paneTop):pane.clientHeight;
  const top=decision?Math.max(header,decision.getBoundingClientRect().bottom-available+12):header;
  place(pane,pane.scrollTop+top-pane.getBoundingClientRect().top);measure(pane);capture(pane);reader.view.scrollTop=pane.scrollTop;
  node.classList.remove('message-flash');void node.offsetWidth;node.classList.add('message-flash');
  return true;
 };
 const revealMessage=useCallback((id:string)=>{
  const index=messages.findIndex(m=>m.id===id);if(index<0)return;
  setFollowing(false);reveal.current=id;prependHeight.current=undefined;
  if(index<end-visibleMessages||index>=end){
   const nextEnd=Math.min(messages.length,index+EARLIER_MESSAGES);
   setEndId(messages[nextEnd-1].id);setVisibleMessages(Math.min(nextEnd,limit));
  }else requestAnimationFrame(()=>{revealNow();});
 },[messages,end,visibleMessages,setFollowing]);
 const showEarlier=useCallback(()=>{setFollowing(false);loadEarlier(paneRef.current);},[setFollowing,loadEarlier]);
 return{paneRef,contentRef,followingLatest,visibleMessages,end,onScroll,jumpToLatest,showEarlier,showLater,revealMessage};
}
