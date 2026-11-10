import {useLayoutEffect,useRef,useState,type UIEvent} from 'react';
import type {ConversationReader} from './transcript-cache';

/** Follow the rendered content, including asynchronous diagrams and viewport changes. */
export function useChatScroll(reader:ConversationReader,messages:unknown,pendingCount:number|undefined){
 const paneRef=useRef<HTMLDivElement>(null),contentRef=useRef<HTMLDivElement>(null);
 const following=useRef(reader.view.following);
 const [followingLatest,setFollowingLatest]=useState(reader.view.following);
 const [visibleMessages,setVisibleMessages]=useState(reader.view.visibleMessages);
 const prependHeight=useRef<number|undefined>(undefined);
 const dimensions=useRef({height:0,width:0,content:0});
 const measure=(pane:HTMLDivElement)=>{dimensions.current={height:pane.clientHeight,width:pane.clientWidth,content:pane.scrollHeight};};
 const pin=(pane:HTMLDivElement)=>{pane.scrollTop=pane.scrollHeight;measure(pane);};
 const setFollowing=(value:boolean)=>{following.current=value;reader.view.following=value;setFollowingLatest(value);};
 useLayoutEffect(()=>{
  following.current=reader.view.following;setFollowingLatest(reader.view.following);setVisibleMessages(reader.view.visibleMessages);
  const pane=paneRef.current,content=contentRef.current;if(!pane||!content)return;
  pane.scrollTop=reader.view.following?pane.scrollHeight:reader.view.scrollTop;measure(pane);
  const resize=new ResizeObserver(()=>{if(following.current)pin(pane);else measure(pane);});
  resize.observe(pane);resize.observe(content);
  return()=>{resize.disconnect();reader.view.scrollTop=pane.scrollTop;reader.view.following=following.current;};
 },[reader]);
 useLayoutEffect(()=>{
  const pane=paneRef.current;if(!pane)return;
  if(prependHeight.current!==undefined){pane.scrollTop+=pane.scrollHeight-prependHeight.current;prependHeight.current=undefined;measure(pane);}
  else if(following.current)pin(pane);
  reader.view.visibleMessages=visibleMessages;
 },[reader,messages,visibleMessages,pendingCount]);
 const onScroll=(event:UIEvent<HTMLDivElement>)=>{
  const pane=event.currentTarget,previous=dimensions.current;
  // Browser layout scrolls are not evidence that the user stopped following.
  if(following.current&&(previous.height!==pane.clientHeight||previous.width!==pane.clientWidth||previous.content!==pane.scrollHeight)){pin(pane);return;}
  setFollowing(pane.scrollHeight-pane.scrollTop-pane.clientHeight<48);measure(pane);reader.view.scrollTop=pane.scrollTop;
 };
 const jumpToLatest=()=>{setFollowing(true);if(paneRef.current)pin(paneRef.current);};
 const showEarlier=()=>{prependHeight.current=paneRef.current?.scrollHeight;setFollowing(false);setVisibleMessages(count=>count+100);};
 return{paneRef,contentRef,followingLatest,visibleMessages,onScroll,jumpToLatest,showEarlier};
}
