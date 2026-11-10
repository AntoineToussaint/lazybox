import { query, createSdkMcpServer, tool, type Query, type SDKUserMessage, type PermissionResult } from '@anthropic-ai/claude-agent-sdk';
import { z } from 'zod';
import { createInterface } from 'node:readline';
import { randomUUID } from 'node:crypto';
import { nativeOptions } from './options.js';
import { readQuota } from './usage.js';
import { NativeStatus } from './status.js';
import { contextSnapshot } from './context.js';

type Request = { id?: string; method?: string; params?: any; result?: any; error?: any };
let session: Query | undefined;
let reference = '';
let interrupted = false;
let inputWake: (() => void) | undefined;
const messages: SDKUserMessage[] = [];
const requests = new Map<string, { resolve: (value: PermissionResult) => void; input: Record<string, unknown>; kind: string }>();
const tools = new Map<string, { resolve: (value: any) => void; reject: (error: Error) => void }>();
const send = (value: unknown) => process.stdout.write(JSON.stringify(value) + '\n');
const event = (value: unknown) => send({ method: 'event', params: value });
const reply = (id: string | undefined, result: unknown) => send({ id, result });

async function* input(): AsyncGenerator<SDKUserMessage> {
  while (true) {
    if (messages.length === 0) await new Promise<void>(resolve => { inputWake = resolve; });
    const next = messages.shift();
    if (next) yield next;
  }
}

async function open(options: any) {
  const commandTool = tool('command', 'Start/read/wait/cancel runner-owned commands with shared console output. Start returns an id; reuse it for read/wait/cancel, never restart a command to read output.', {
    action: z.enum(['start', 'read', 'wait', 'cancel']), argv: z.array(z.string()).optional(), id: z.string().optional(), offset: z.number().int().nonnegative().optional(),
  }, async args => {
    const id = randomUUID();
    const result = await new Promise<string>((resolve, reject) => {
      tools.set(id, { resolve, reject }); send({ id, method: 'tool', params: { name: 'lazybox_command', arguments: args } });
    });
    return { content: [{ type: 'text', text: result }] };
  });
  session = query({ prompt: input(), options: {
    ...nativeOptions(options, process.env),
    mcpServers: ['help', 'planning', 'summary'].includes(options.purpose) ? {} : { lazybox: createSdkMcpServer({ name: 'lazybox', version: '0.1.0', tools: [commandTool] }) },
    canUseTool: async (name, original, context): Promise<PermissionResult> => {
      const kind = name === 'AskUserQuestion' ? 'question' : 'approval';
      const id = context.requestId;
      if (!id) throw new Error('SDK omitted the native permission request id');
      if (requests.size >= 64) throw new Error('Pending request limit exceeded');
      return new Promise<PermissionResult>(resolve => {
        const finish = (result: PermissionResult) => {
          context.signal.removeEventListener('abort', abort);
          requests.delete(id); event({type:'activity',status:result.behavior==='allow'?'tool':'working',name:result.behavior==='allow'?name:undefined,sessionId:reference,agentId:context.agentID?`claude-agent:${context.agentID}`:undefined}); event({ type: 'request_resolved', requestId: id, status: 'callback_resolved', sessionId: reference }); resolve(result);
        };
        const abort = () => finish({ behavior: 'deny', message: 'Request canceled by provider' });
        requests.set(id, { resolve: finish, input: original, kind });
        event({ type: kind, requestId: id, sessionId: reference, agentId: context.agentID ? `claude-agent:${context.agentID}` : undefined, parentAgentId: context.agentID ? 'root' : undefined, text: name, data: { tool: name, input: original, toolUseId: context.toolUseID } });
        context.signal.addEventListener('abort', abort, { once: true });
        if (context.signal.aborted) abort();
      });
    },
  } });
  void receive(session);
  return { reference: options.resume || '' };
}

async function receive(q: Query) {
  try {
    const status = new NativeStatus();
    let readingContext=false;
    for await (const message of q) {
      for (const normalized of status.normalize(message)) {
        if (normalized.type === 'turn_ended') { if (interrupted) normalized.status = 'interrupted'; interrupted = false; }
        event(normalized);
      }
      reference = status.reference || reference;
      if(message.type==='result'&&!readingContext&&typeof q.getContextUsage==='function'){
        readingContext=true;
        void q.getContextUsage({detail:'summary'}).then(report=>event({type:'context',agentId:'root',sessionId:reference,data:contextSnapshot(report)})).catch(()=>{/* Older CLIs retain their last observed totals. */}).finally(()=>{readingContext=false;});
      }
    }
    event({ type: 'failure', status: 'provider_process_exit', text: 'SDK stream ended' });
  } catch (error) {
    event({ type: 'failure', status: 'sdk_error', text: String(error) });
  }
}

async function handle(request: Request) {
  if (!request.method && request.id && tools.has(request.id)) {
    const pending = tools.get(request.id)!; tools.delete(request.id);
    if (request.error) pending.reject(new Error(JSON.stringify(request.error))); else pending.resolve(request.result);
    return;
  }
  try {
    const p = request.params;
    if (request.method === 'account_usage') { if (session) throw new Error('account query requires a dedicated worker'); reply(request.id, await readQuota(p)); return; }
    if (request.method === 'open') { if (session) throw new Error('already opened'); reply(request.id, await open(p)); return; }
    if (!session) throw new Error('session not opened');
    switch (request.method) {
      case 'message': {
        const content: any[] = [];
        if (p.text) content.push({ type: 'text', text: p.text });
        for (const image of p.images || []) content.push({ type: 'image', source: { type: 'base64', media_type: image.mediaType, data: image.data } });
        messages.push({ type: 'user', uuid: randomUUID(), session_id: reference, message: { role: 'user', content }, parent_tool_use_id: null });
        inputWake?.(); inputWake = undefined;
        reply(request.id, { delivery: 'submitted' }); break;
      }
      case 'interrupt': interrupted = true; await session.interrupt(); reply(request.id, { delivery: 'acknowledged' }); break;
      case 'answer': case 'approve': {
        const pending = requests.get(p.requestId);
        if (!pending || (request.method === 'answer') !== (pending.kind === 'question')) throw new Error('unknown, resolved or mismatched request');
        if (request.method === 'answer') {
          const answers = Object.fromEntries(Object.entries(p.answers || {}).map(([k, v]) => [k, (v as string[]).join(', ')]));
          pending.resolve({ behavior: 'allow', updatedInput: { ...pending.input, answers } });
        } else pending.resolve(p.decision === 'accept' ? { behavior: 'allow', updatedInput: pending.input } : { behavior: 'deny', message: 'Declined by operator' });
        reply(request.id, { delivery: 'submitted' }); break;
      }
      default: throw new Error('Unsupported worker operation');
    }
  } catch (error) { send({ id: request.id, error: { message: String(error) } }); }
}

const reader = createInterface({ input: process.stdin, crlfDelay: Infinity });
reader.on('line', line => {
  if (line.length > 16 << 20) { process.exitCode = 1; reader.close(); return; }
  try { void handle(JSON.parse(line)); } catch { process.exitCode = 1; reader.close(); }
});
reader.on('close', () => { session?.close(); process.exit(); });
