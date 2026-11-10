import { createClient, ConnectError, Code } from '@connectrpc/connect';
import { createConnectTransport } from '@connectrpc/connect-web';
export { ConnectError, Code } from '@connectrpc/connect';
import { RunnerService } from '@lazybox/protocol';

export function createRunnerClient(baseUrl: string, token: string, fetch?: typeof globalThis.fetch) {
  return createClient(RunnerService, createConnectTransport({ baseUrl, fetch, interceptors: [next => async request => {
    request.header.set('Authorization', `Bearer ${token}`);
    return next(request);
  }] }));
}
export type RunnerClient = ReturnType<typeof createRunnerClient>;

// Each view owns its own cursor. Rendering speed never participates in the
// producer's delivery and disconnecting a view only aborts its subscription.
export async function follow<T extends { cursor: bigint }>(
  open: (after: bigint, signal: AbortSignal) => AsyncIterable<T>,
  signal: AbortSignal, receive: (event: T) => void, status: (value: string) => void,
  failed?: (error: ConnectError) => void,
  initialCursor = 0n,
) {
  let after = initialCursor;
  let firstAttempt = true;
  while (!signal.aborted) {
    try {
      status(firstAttempt ? 'Connecting' : 'Reconnecting'); firstAttempt=false;
      for await (const event of open(after, signal)) {
        if (event.cursor <= after) continue;
        if (event.cursor !== after + 1n) throw new ConnectError('Replay gap: retained cursor is not contiguous', Code.OutOfRange);
        if (signal.aborted) return;
        receive(event); after = event.cursor; status('Live');
      }
      if (!signal.aborted) status('Complete'); return;
    } catch (error) {
      if (signal.aborted) return;
      const code = ConnectError.from(error).code;
      if ([Code.OutOfRange, Code.Unauthenticated, Code.PermissionDenied, Code.FailedPrecondition, Code.ResourceExhausted, Code.NotFound].includes(code)) { status(String(error)); failed?.(ConnectError.from(error)); return; }
      status(`Disconnected: ${String(error)}`);
      await new Promise<void>(resolve => {
        const done = () => { clearTimeout(timer); signal.removeEventListener('abort', done); resolve(); };
        const timer = setTimeout(done, 1000); signal.addEventListener('abort', done, { once: true });
      });
    }
  }
}

export function providerData(data: Uint8Array): any {
  if (!data.length) return {};
  try { return JSON.parse(new TextDecoder().decode(data)); } catch { return { error: 'Invalid provider data' }; }
}
