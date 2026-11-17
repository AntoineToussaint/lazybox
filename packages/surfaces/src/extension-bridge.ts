/**
 * The extension bridge, protocol 1 (EXTENSIONS.md): what a sandboxed
 * extension may ask of Lazybox, and the host side that answers it. Pure: the
 * panel (ExtensionPanel.tsx) gives it the grants and the handlers that call
 * the Runner with the panel's own extension credential, so a grant is checked
 * twice — here, and by the Runner (transport/extension_auth.go).
 *
 * The extension's client is services/runner/code/internal/power/extensions/
 * runtime/lazybox.js, with its types in lazybox.d.ts beside it;
 * scripts/extension-bridge.test.ts keeps the three in step.
 */
export const BRIDGE_PROTOCOL = 1;

/** Every method, and the grant it needs (`do` needs do:<its action>). */
export const bridgeMethods = {
  hello: '',
  'workspaces.list': 'read:workspaces',
  'sessions.list': 'read:sessions',
  'attention.list': 'read:attention',
  'commands.list': 'read:commands',
  'missions.list': 'read:missions',
  'usage.read': 'read:usage',
  'conversations.show': 'show:conversations',
  'messages.send': 'send:messages',
  do: 'do:',
} as const;
export type BridgeMethod = keyof typeof bridgeMethods;

export type BridgeErrorCode = 'not-granted' | 'bad-request' | 'unknown-method' | 'unsupported-protocol' | 'rate-limited' | 'too-large' | 'refused' | 'failed' | 'unavailable';
export type BridgeError = { code: BridgeErrorCode; message: string; grant?: string };
export type BridgeReply = { lazybox: 1; id: string; result?: unknown; error?: BridgeError };
export type BridgeRequest = { id: string; method: BridgeMethod; params: Record<string, unknown> };

/** A handler's own refusal, with the code the extension sees. */
export class BridgeRefusal extends Error {
  constructor(readonly code: BridgeErrorCode, message: string, readonly grant?: string) { super(message); }
}

/** Limits on what an extension may send. */
export const bridgeLimits = { requestBytes: 64 << 10, messageChars: 32_000, burst: 20, perSecond: 5 };

export type BridgeHandlers = { [M in BridgeMethod]: (params: Record<string, unknown>) => Promise<unknown> };

/** Why a call was refused, said once to the person (the panel's notice). */
export type BridgeRefused = { method: string; grant?: string; code: BridgeErrorCode; message: string };

const idPattern = /^[\w.:-]{1,64}$/;

/** The size of a structured-clone value as JSON, or Infinity when it is not JSON. */
function jsonSize(value: unknown): number {
  try { return JSON.stringify(value)?.length ?? 0; } catch { return Infinity; }
}

/** Parses a message from the extension: a request, or undefined for anything else (ignored). */
export function parseBridgeRequest(data: unknown): { request?: BridgeRequest; reply?: BridgeReply } | undefined {
  if (!data || typeof data !== 'object' || (data as { lazybox?: unknown }).lazybox !== BRIDGE_PROTOCOL) return undefined;
  const { id, method, params } = data as { id?: unknown; method?: unknown; params?: unknown };
  if (typeof id !== 'string' || !idPattern.test(id)) return undefined;
  const fail = (code: BridgeErrorCode, message: string) => ({ reply: { lazybox: 1 as const, id, error: { code, message } } });
  if (jsonSize(data) > bridgeLimits.requestBytes) return fail('too-large', `A request is at most ${bridgeLimits.requestBytes >> 10} KiB`);
  if (typeof method !== 'string' || !Object.hasOwn(bridgeMethods, method)) return fail('unknown-method', `Lazybox has no method ${String(method).slice(0, 60)} (protocol ${BRIDGE_PROTOCOL})`);
  if (params !== undefined && (params === null || typeof params !== 'object' || Array.isArray(params))) return fail('bad-request', 'params is an object');
  return { request: { id, method: method as BridgeMethod, params: (params || {}) as Record<string, unknown> } };
}

/** The grant a request needs, or '' when none does. */
export function grantFor(request: Pick<BridgeRequest, 'method' | 'params'>): string {
  if (request.method === 'do') return `do:${typeof request.params.action === 'string' ? request.params.action : ''}`;
  return bridgeMethods[request.method];
}

/** A token bucket: burst calls at once, perSecond after. */
export function rateLimiter(now: () => number, burst = bridgeLimits.burst, perSecond = bridgeLimits.perSecond) {
  let tokens = burst, at = now();
  return () => {
    const t = now();
    tokens = Math.min(burst, tokens + (t - at) / 1000 * perSecond); at = t;
    if (tokens < 1) return false;
    tokens -= 1; return true;
  };
}

/**
 * The host side: answers one message from the extension. It never throws; a
 * handler's failure becomes an error reply, so an extension can never break
 * Lazybox by what it sends.
 */
export function createBridgeHost({ grants, handlers, now = () => Date.now(), onRefused }: { grants: readonly string[]; handlers: BridgeHandlers; now?: () => number; onRefused?(refused: BridgeRefused): void }) {
  const allow = rateLimiter(now);
  return async function handle(data: unknown): Promise<BridgeReply | undefined> {
    const parsed = parseBridgeRequest(data);
    if (!parsed) return undefined;
    if (parsed.reply) { onRefused?.({ method: String((data as { method?: unknown }).method || ''), code: parsed.reply.error!.code, message: parsed.reply.error!.message }); return parsed.reply; }
    const request = parsed.request!;
    const reply = (error: BridgeError): BridgeReply => { onRefused?.({ method: request.method, ...error }); return { lazybox: 1, id: request.id, error }; };
    if (!allow()) return reply({ code: 'rate-limited', message: 'Too many requests; slow down (about 5 a second)' });
    if (request.method === 'hello' && request.params.protocol !== undefined && request.params.protocol !== BRIDGE_PROTOCOL)
      return reply({ code: 'unsupported-protocol', message: `This Lazybox speaks extension protocol ${BRIDGE_PROTOCOL}` });
    const grant = grantFor(request);
    if (grant && !grants.includes(grant)) return reply({ code: 'not-granted', grant, message: `This extension was not given ${grant}` });
    try {
      return { lazybox: 1, id: request.id, result: await handlers[request.method](request.params) };
    } catch (error) {
      if (error instanceof BridgeRefusal) return reply({ code: error.code, message: error.message, ...(error.grant ? { grant: error.grant } : {}) });
      const code = (error as { code?: unknown })?.code;
      // The Runner's own check (Connect's PermissionDenied is 7, Unauthenticated 16).
      if (code === 7 || code === 16) return reply({ code: 'refused', message: 'Lazybox refused that for this extension' });
      if (code === 14) return reply({ code: 'unavailable', message: 'The Runner is not answering; try again in a moment' });
      return reply({ code: 'failed', message: String((error as Error)?.message || error).slice(0, 300) });
    }
  };
}

/** Requires a string param. */
export function stringParam(params: Record<string, unknown>, name: string, max = 200): string {
  const value = params[name];
  if (typeof value !== 'string' || !value || value.length > max) throw new BridgeRefusal('bad-request', `${name} is required (text, at most ${max} characters)`);
  return value;
}
