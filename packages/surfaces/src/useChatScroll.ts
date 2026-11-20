import {useCallback,useLayoutEffect,useRef,useState,type UIEvent} from 'react';
import {EARLIER_MESSAGES,type ConversationReader} from './transcript-cache';
/** How close to the bottom a reader scrolling down picks up following again. */
const FOLLOW_THRESHOLD=48;

/** The reader's own scrolling: which way, until when a scroll is theirs, and
 * whether it aims at the bottom (measured when they asked, as a reply streaming
 * in can grow the content before the scroll lands). */
type Intent={direction:'up'|'down'|'any';until:number;bottom?:boolean};
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
export function useChatScroll(reader:ConversationReader,messages:readonly {id:string}[],pendingCount:number|undefined){
 const paneRef=useRef<HTMLDivElement>(null),contentRef=useRef<HTMLDivElement>(null);
 const following=useRef(reader.view.following);
 const [followingLatest,setFollowingLatest]=useState(reader.view.following);
 const [visibleMessages,setVisibleMessages]=useState(reader.view.visibleMessages);
 const prependHeight=useRef<number|undefined>(undefined);
 const dimensions=useRef({height:0,width:0,content:0});
 const hidden=useRef(0);hidden.current=Math.max(0,messages.length-visibleMessages);
 const measure=(pane:HTMLDivElement)=>{dimensions.current={height:pane.clientHeight,width:pane.clientWidth,content:pane.scrollHeight};};
 const resized=(pane:HTMLDivElement)=>dimensions.current.height!==pane.clientHeight||dimensions.current.width!==pane.clientWidth;
 // Where the pane was last left, by the reader or by us: a scroll event's
 // movement is measured from it, so our own scrolling never reads as theirs.
 const lastTop=useRef(0);
 const place=(pane:HTMLDivElement,top:number)=>{pane.scrollTop=top;lastTop.current=pane.scrollTop;};
 const intent=useRef<Intent|undefined>(undefined),pressed=useRef(false);
 const readerScrolling=()=>pressed.current||(!!intent.current&&performance.now()<intent.current.until);
 // WebKit has no scroll anchoring (and Chromium's is off on the pane). While
 // the reader is not following, the element at the top of the viewport keeps
 // its place when content above it changes height, such as an image or a
 // diagram rendering in an earlier message, or a tool group folding.
 const anchor=useRef<{node:Element;offset:number}|undefined>(undefined);
 const capture=(pane:HTMLDivElement)=>{
  anchor.current=undefined;const top=pane.getBoundingClientRect().top;
  // Descend into the row across the top edge, so a change above the viewport
  // inside one long message is caught too.
  let parent:Element|null|undefined=contentRef.current;
  for(let depth=0;parent&&depth<6;depth++){
   let next:Element|undefined;
   for(const node of parent.children){const box=node.getBoundingClientRect();if(box.bottom>top&&box.height>0){next=node;anchor.current={node,offset:box.top-top};if(box.top>=top)return;break;}}
   parent=next;
  }
 };
 const restore=(pane:HTMLDivElement)=>{
  const held=anchor.current;if(!held?.node.isConnected)return;
  const shift=held.node.getBoundingClientRect().top-pane.getBoundingClientRect().top-held.offset;
  if(Math.abs(shift)>=1)place(pane,pane.scrollTop+shift);
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
 const setFollowing=useCallback((value:boolean)=>{following.current=value;reader.view.following=value;setFollowingLatest(value);},[reader]);
 const loadEarlier=useCallback((pane:HTMLDivElement|null)=>{
  if(!hidden.current||prependHeight.current!==undefined)return;
  prependHeight.current=pane?.scrollHeight;setVisibleMessages(count=>count+EARLIER_MESSAGES);
 },[]);
 // Within a screen of the top, the next step of history is mounted before the reader reaches it.
 const nearTop=(pane:HTMLDivElement)=>hidden.current>0&&pane.scrollTop<Math.max(400,pane.clientHeight);
 useLayoutEffect(()=>{
  following.current=reader.view.following;setFollowingLatest(reader.view.following);setVisibleMessages(reader.view.visibleMessages);
  const pane=paneRef.current,content=contentRef.current;if(!pane||!content)return;
  place(pane,reader.view.following?pane.scrollHeight:reader.view.scrollTop);measure(pane);
  if(!reader.view.following)capture(pane);
  const resize=new ResizeObserver(()=>{if(following.current)follow(pane);else{restore(pane);measure(pane);}});
  resize.observe(pane);resize.observe(content);
  // The reader's intent arrives before the scroll it causes. Stopping on it
  // means a delta rendered in between cannot pin the pane back down.
  const intend=(direction:Intent['direction'],ms:number,distance=0)=>{
   intent.current={direction,until:performance.now()+ms,bottom:direction==='down'&&pane.scrollTop+distance>=pane.scrollHeight-pane.clientHeight-FOLLOW_THRESHOLD};
   if(direction==='up'&&following.current&&pane.scrollTop>0){setFollowing(false);capture(pane);}
  };
  const wheel=(event:WheelEvent)=>{
   if(!event.deltaY||event.ctrlKey||innerScroller(event.target,pane,event.deltaY<0))return;
   intend(event.deltaY<0?'up':'down',300,event.deltaY*(event.deltaMode===1?16:event.deltaMode===2?pane.clientHeight:1));
  };
  // On the window, after the app's own handlers: a key one of them used is not a scroll.
  const key=(event:KeyboardEvent)=>{
   if(event.defaultPrevented||event.altKey||event.ctrlKey||event.metaKey||!pane.contains(event.target as Node)||editable(event.target))return;
   // End is an explicit request to follow the latest output. Do not wait
   // for native smooth scrolling: streaming can move its target meanwhile.
   if(event.key==='End'&&!innerScroller(event.target,pane,false)){
    event.preventDefault();intent.current=undefined;setFollowing(true);pin(pane);return;
   }
   const up=UP_KEYS.has(event.key)||(event.key===' '&&event.shiftKey);
   if((up||DOWN_KEYS.has(event.key))&&!innerScroller(event.target,pane,up))intend(up?'up':'down',800,event.key==='End'?Infinity:event.key==='ArrowDown'?40:pane.clientHeight*.85);
  };
  // A held pointer or finger (the scrollbar, a selection, a swipe) makes every
  // scroll the reader's, and so does the momentum after it. Touch is followed
  // by its own events: the browser cancels the pointer once a finger pans.
  const press=(event:PointerEvent)=>{if(event.pointerType!=='touch'&&event.button===0)pressed.current=true;};
  const touch=()=>{pressed.current=true;};
  const release=(ms:number)=>{if(!pressed.current)return;pressed.current=false;intent.current={direction:'any',until:performance.now()+ms};};
  const releasePointer=(event:PointerEvent)=>{if(event.pointerType!=='touch')release(150);},releaseTouch=()=>release(1500);
  // A scrollbar drag may end without a pointerup reaching the page.
  const hover=(event:PointerEvent)=>{if(event.pointerType!=='touch'&&!event.buttons)release(150);};
  pane.addEventListener('wheel',wheel,{passive:true});pane.addEventListener('pointerdown',press);pane.addEventListener('touchstart',touch,{passive:true});
  window.addEventListener('keydown',key);window.addEventListener('pointerup',releasePointer);window.addEventListener('pointercancel',releasePointer);window.addEventListener('pointermove',hover);
  window.addEventListener('touchend',releaseTouch);window.addEventListener('touchcancel',releaseTouch);
  return()=>{
   resize.disconnect();reader.view.scrollTop=pane.scrollTop;reader.view.following=following.current;
   pane.removeEventListener('wheel',wheel);pane.removeEventListener('pointerdown',press);pane.removeEventListener('touchstart',touch);
   window.removeEventListener('keydown',key);window.removeEventListener('pointerup',releasePointer);window.removeEventListener('pointercancel',releasePointer);window.removeEventListener('pointermove',hover);
   window.removeEventListener('touchend',releaseTouch);window.removeEventListener('touchcancel',releaseTouch);
  };
 },[reader,setFollowing]);
 useLayoutEffect(()=>{
  const pane=paneRef.current;if(!pane)return;
  if(prependHeight.current!==undefined){place(pane,pane.scrollTop+pane.scrollHeight-prependHeight.current);prependHeight.current=undefined;measure(pane);capture(pane);}
  else if(following.current)follow(pane);
  reader.view.visibleMessages=visibleMessages;
  if(reveal.current&&revealNow())return;
  if(nearTop(pane))loadEarlier(pane);
 },[reader,messages,visibleMessages,pendingCount,loadEarlier]);
 const onScroll=(event:UIEvent<HTMLDivElement>)=>{
  const pane=event.currentTarget,top=pane.scrollTop,moved=top-lastTop.current,movedUp=readerMovedUp(pane);lastTop.current=top;
  const gap=pane.scrollHeight-top-pane.clientHeight;
  if(readerScrolling()){
   // Up by any amount stops following, even within the threshold; down to near the bottom resumes it.
   // A pane clamped to its new bottom (content folding below), or moved by its
   // own resize, is not the reader moving.
   if(moved<0&&gap>1&&!resized(pane))setFollowing(false);
   // A compositor/anchor correction during an upward gesture is not a
   // request to follow. Only downward intent gets the near-bottom tolerance.
   else if(moved>0&&intent.current?.direction!=='up'&&(gap<=1||(intent.current?.direction==='down'&&(gap<FOLLOW_THRESHOLD||intent.current.bottom))))setFollowing(true);
  }else if(following.current){
   if(movedUp)setFollowing(false);
   // Browser layout scrolls are not evidence that the user stopped following.
   else if(resized(pane)||dimensions.current.content!==pane.scrollHeight){pin(pane);return;}
   // A scroll nobody pressed for (find in page) that leaves the bottom.
   else if(gap>=FOLLOW_THRESHOLD)setFollowing(false);
  }else if(moved>0&&gap<=1)setFollowing(true);
  measure(pane);reader.view.scrollTop=top;
  if(following.current)anchor.current=undefined;else capture(pane);
  if(nearTop(pane))loadEarlier(pane);
 };
 const jumpToLatest=()=>{intent.current=undefined;setFollowing(true);if(paneRef.current)pin(paneRef.current);};
 // Jumping to a message (a prompt) may need earlier history mounted first:
 // widen the window, then scroll it to the top once it is rendered.
 const reveal=useRef<string|undefined>(undefined);
 const revealNow=()=>{
  const id=reveal.current,pane=paneRef.current;if(!id||!pane)return false;
  const node=contentRef.current?.querySelector<HTMLElement>(`[data-message-id="${CSS.escape(id)}"]`);if(!node)return false;
  reveal.current=undefined;
  place(pane,pane.scrollTop+node.getBoundingClientRect().top-pane.getBoundingClientRect().top-12);measure(pane);capture(pane);reader.view.scrollTop=pane.scrollTop;
  node.classList.remove('message-flash');void node.offsetWidth;node.classList.add('message-flash');
  return true;
 };
 const revealMessage=useCallback((id:string)=>{
  const index=messages.findIndex(m=>m.id===id);if(index<0)return;
  setFollowing(false);reveal.current=id;prependHeight.current=undefined;
  const need=messages.length-index+1;
  if(need>visibleMessages)setVisibleMessages(need);else requestAnimationFrame(()=>{revealNow();});
 },[messages,visibleMessages,setFollowing]);
 const showEarlier=useCallback(()=>{setFollowing(false);loadEarlier(paneRef.current);},[setFollowing,loadEarlier]);
 return{paneRef,contentRef,followingLatest,visibleMessages,onScroll,jumpToLatest,showEarlier,revealMessage};
}
