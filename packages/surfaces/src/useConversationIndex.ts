import {Code,ConnectError} from '@lazybox/client-core';
import { useEffect, useMemo, useRef, useState } from "react";
import type { Session } from "@lazybox/protocol";
import { useRunner } from "./RunnerContext";
import { mergeConversations } from "./conversation-model";
import { humanError } from "./human-error";

const pageSize = 50;
const transientHistoryError = (error: unknown) => [Code.Unavailable, Code.DeadlineExceeded, Code.ResourceExhausted].includes(ConnectError.from(error).code);

/** One durable navigation index per sidebar, independent of workspace selection. */
export function useConversationIndex() {
  const { api, info, revision, slow, error: connectionError } = useRunner();
  const paused = slow || !!connectionError;
  const pauseHistory = useRef(paused);
  pauseHistory.current = paused;
  const [saved, setSaved] = useState<Session[]>([]);
  const [complete, setComplete] = useState(false);
  const [error, setError] = useState("");
  const [unsupported,setUnsupported]=useState(false);
  const refresh = useRef<() => void>(() => {});
  useEffect(() => {
    setSaved([]);
    setComplete(false);
    setError('');
    setUnsupported(false);
  }, [api]);
  useEffect(() => {
    const control = new AbortController();
    let pending = false;
    let refreshRequested = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const readPage = async (offset: number) => {
      const read = () => api.listHistory({ limit: pageSize, offset }, { signal: control.signal, timeoutMs: 15000 });
      try { return await read(); }
      catch (error) {
        if (control.signal.aborted || pauseHistory.current || !transientHistoryError(error)) throw error;
        await new Promise(resolve => setTimeout(resolve, 650));
        if (pauseHistory.current) throw error;
        return read();
      }
    };
    const load = async () => {
      if (control.signal.aborted || pauseHistory.current) return;
      if (pending) {
        refreshRequested = true;
        return;
      }
      clearTimeout(timer);
      pending = true;
      try {
        const rows: Session[] = [];
        for (let offset = 0; ; offset += pageSize) {
          const page = await readPage(offset);
          rows.push(...page.sessions);
          if (!page.hasMore) break;
          if (pauseHistory.current) return;
        }
        if (!control.signal.aborted) {
          setSaved(rows);
          setComplete(true);setUnsupported(false);
          setError("");
        }
      } catch (error) {
        if (!control.signal.aborted) {
          const code = ConnectError.from(error).code;
          if (code === Code.Unimplemented) { setUnsupported(true); setError(''); }
          else setError(humanError(error));
        }
      } finally {
        pending = false;
        if (control.signal.aborted) return;
        if (refreshRequested) {
          refreshRequested = false;
          void load();
        } else timer = setTimeout(() => void load(), 15000);
      }
    };
    refresh.current = () => void load();
    return () => {
      control.abort();
      clearTimeout(timer);
    };
  }, [api]);
  useEffect(() => { if (!paused) refresh.current(); }, [api, revision, paused]);
  return {
    sessions: useMemo(
      () => mergeConversations(saved, info?.sessions || []),
      [saved, info?.sessions],
    ),
    complete,
    error,
    unsupported,
    refresh: () => refresh.current(),
  };
}
