import {Code,ConnectError} from '@lazybox/client-core';
import { useEffect, useMemo, useState } from "react";
import type { Session } from "@lazybox/protocol";
import { useRunner } from "./RunnerContext";
import { mergeConversations } from "./conversation-model";

/** One durable navigation index per sidebar, independent of workspace selection. */
export function useConversationIndex() {
  const { api, info, revision } = useRunner();
  const [saved, setSaved] = useState<Session[]>([]);
  const [complete, setComplete] = useState(false);
  const [error, setError] = useState("");
  const [unsupported,setUnsupported]=useState(false);
  useEffect(()=>{setSaved([]);setComplete(false);setError('');setUnsupported(false);},[api]);
  useEffect(() => {
    const control = new AbortController();
    let pending = false;
    const load = async () => {
      if (pending) return;
      pending = true;
      try {
        const rows: Session[] = [];
        for (let offset = 0; ; offset += 100) {
          const page = await api.listHistory(
            { limit: 100, offset },
            { signal: control.signal, timeoutMs: 5000 },
          );
          rows.push(...page.sessions);
          if (!page.hasMore) break;
        }
        if (!control.signal.aborted) {
          setSaved(rows);
          setComplete(true);setUnsupported(false);
          setError("");
        }
      } catch (error) {
        if (!control.signal.aborted) {if(ConnectError.from(error).code===Code.Unimplemented)setUnsupported(true);else setError(String(error));}
      } finally {
        pending = false;
      }
    };
    void load();
    const timer = setInterval(() => void load(), 15000);
    return () => {
      control.abort();
      clearInterval(timer);
    };
  }, [api, revision]);
  return {
    sessions: useMemo(
      () => mergeConversations(saved, info?.sessions || []),
      [saved, info?.sessions],
    ),
    complete,
    error,
    unsupported,
  };
}
