import { useMemo, useSyncExternalStore } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import { emptyTranscript } from './transcript-model';
import { conversationReader, type ConversationReader } from './transcript-cache';
const idle=()=>()=>{};
const hidden:ConversationReader['snapshot']={...emptyTranscript(),status:'Connecting',error:'',loading:true};
const hiddenSnapshot=()=>hidden;
/** Cache projected events and reconnect from their cursor when revisiting a view. */
export function useTranscript(api: RunnerClient, sessionId: string, connected: boolean, shown = true) {
 const reader=useMemo(()=>conversationReader(api,sessionId),[api,sessionId]);
 const transcript=useSyncExternalStore(shown&&connected&&sessionId?reader.subscribe:idle,shown?reader.getSnapshot:hiddenSnapshot);
 return {...transcript,retry:reader.retry,reader};
}
