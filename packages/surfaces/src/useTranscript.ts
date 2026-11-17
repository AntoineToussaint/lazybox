import { useMemo, useSyncExternalStore } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import { conversationReader } from './transcript-cache';
const idle=()=>()=>{};
/** Cache projected events and reconnect from their cursor when revisiting a view. */
export function useTranscript(api: RunnerClient, sessionId: string, connected: boolean) {
 const reader=useMemo(()=>conversationReader(api,sessionId),[api,sessionId]);
 const transcript=useSyncExternalStore(connected&&sessionId?reader.subscribe:idle,reader.getSnapshot);
 return {...transcript,retry:reader.retry,reader};
}
