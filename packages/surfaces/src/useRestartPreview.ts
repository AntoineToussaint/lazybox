import { useEffect, useState } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { TranscriptMessage } from './transcript-model';

type Preview = { key: string; boundaryError: string; fileError: string; members: string[] };

export function useRestartPreview(api: RunnerClient, source: string, prompt?: TranscriptMessage) {
  const cursor = prompt?.type === 'operation_accepted' ? prompt.id : '';
  const operation = prompt?.operationId;
  const key = source + '/' + cursor;
  const [preview, setPreview] = useState<Preview>();
  useEffect(() => {
    setPreview(undefined);
    if (!cursor) return;
    const control = new AbortController();
    void api.listTurnCheckpoints({ sessionId: source, beforePromptCursor: BigInt(cursor) }, { signal: control.signal, timeoutMs: 15000 }).then(result => {
      if (control.signal.aborted) return;
      const snapshots = result.turns.find(turn => turn.operationId === operation)?.before || [];
      const incomplete = snapshots.find(repo => repo.error || !repo.commit || repo.skippedCount || repo.skipped.length);
      const fileError = !snapshots.length ? 'No retained file checkpoint for this prompt.' : incomplete
        ? (incomplete.error || (incomplete.skippedCount || incomplete.skipped.length ? 'Files were skipped' : 'Missing checkpoint commit')) + ' (' + incomplete.member + ').' : '';
      setPreview({ key, boundaryError: result.restartBoundaryError ?? 'Update the Runner to verify this restart point.', fileError, members: snapshots.map(repo => repo.member) });
    }, error => {
      if (!control.signal.aborted) setPreview({ key, boundaryError: String(error), fileError: '', members: [] });
    });
    return () => control.abort();
  }, [api, key, source, cursor, operation]);
  return preview?.key === key ? preview : undefined;
}
