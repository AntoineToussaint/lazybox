import type { SDKMessage } from '@anthropic-ai/claude-agent-sdk';

type Event = { type: string; agentId?: string; parentAgentId?: string; nativeId?: string; name?: string; agentKind?: string; status?: string; sessionId?: string; text?: string; data?: unknown; account?: { state: string; reason?: string; resetAt?: number; windows?: { name: string; usedPercent: number; resetAt?: number; durationMinutes?: number }[] } };
// Inspection payloads must not exhaust the durable event budget. Tool execution
// receives the original values; this bounds only the retained UI preview.
function toolPreview(value:unknown):unknown {
 const text=typeof value==='string'?value:JSON.stringify(value);
 return text&&text.length>32768?text.slice(0,32768)+'\n[Tool preview truncated]':value;
}
// A sub-agent's brief is shown to the person, not executed: bound it.
const PROMPT_LIMIT=8000;
// A local slash command's printed output, shown as the agent's answer.
const OUTPUT_LIMIT=64<<10;
function bounded(text:unknown):string|undefined{
 return typeof text==='string'&&text?(text.length>PROMPT_LIMIT?text.slice(0,PROMPT_LIMIT)+'…':text):undefined;
}
type TaskUsage={total_tokens:number;tool_uses:number;duration_ms:number};
const usage=(u:TaskUsage|undefined)=>u?{totalTokens:u.total_tokens,toolUses:u.tool_uses,durationMs:u.duration_ms}:undefined;
// Drop absent fields, so the journal holds only what the provider reported.
const compact=(value:Record<string,unknown>)=>Object.fromEntries(Object.entries(value).filter(([,v])=>v!==undefined&&v!==''));
const TERMINAL=['done','failed','stopped'];
// The tools whose child runs are tasks: the provider reports their lifecycle.
const SPAWN_TOOLS=new Set(['Task','Agent']);
// Tool calls remembered for naming a forked run, and forked runs at once: the newest.
const CALL_LIMIT=512, FORK_LIMIT=256;
export function limitState(code: string): string {
  if (['authentication_failed', 'oauth_org_not_allowed', 'cloud_credential_error', 'gateway_signin_required', 'org_config_refused'].includes(code)) return 'auth_required';
  if (code === 'rate_limit') return 'throttled';
  if (code === 'billing_error') return 'billing_error';
  if (code === 'credits_required') return 'exhausted';
  if (code === 'context_length_exceeded') return 'context_full';
  if (['error_max_turns', 'error_max_budget_usd'].includes(code)) return 'budget_exceeded';
  return 'unknown';
}
/** Reduces structured CLI events. Text is never used as a control signal. */
export class NativeStatus {
  reference = '';
  private active = false;
  private failure = '';
  // `background` is what the provider said: true for a task that outlives the
  // tool call that started it, false for one that call blocks on, undefined
  // when it did not say.
  private tasks = new Map<string, { id: string; run?: string; status: string; background?: boolean }>();
  private background = new Set<string>();
  private tools = new Map<string, string>();
  // Every tool call by id, until its result: who made it, with what.
  private calls = new Map<string, { name: string; input: unknown; agent: string }>();
  // Forked runs in progress (a Skill with `context: fork`), by the call that
  // runs them: a child the provider reports no task for, only the call and
  // its result. Their lifecycle is the call's.
  private forks = new Map<string, string>();
  private contexts = new Map<string, { model: string; used: number }>();
  private capacities = new Map<string, number>();
  // Children whose text arrived as stream deltas since their last complete
  // message: that message's text was already emitted.
  private streamed = new Set<string>();
  normalize(message: SDKMessage): Event[] {
    const out: Event[] = [];
    const emit = (e: Event) => out.push({ sessionId: this.reference, ...e });
    const m = message;
    const child = 'parent_tool_use_id' in m && m.parent_tool_use_id ? (this.tools.get(m.parent_tool_use_id) || `claude-tool:${m.parent_tool_use_id}`) : undefined;
    if (m.type === 'system' && m.subtype === 'init') {
      if (!this.reference && !child) this.reference = m.session_id;
      if (!this.active) emit({ type: 'session_ready', sessionId: this.reference, data: { version: m.claude_code_version, model: m.model } });
    }
    if (m.type === 'system' && m.subtype === 'init' && !child) emit({type:'tool_catalog',data:{tools:m.tools}});
    const begin = () => { if (!this.active && !child) { this.active = true; emit({ type: 'turn_started' }); } };
    if (child && 'parent_tool_use_id' in m && m.parent_tool_use_id) this.fork(m.parent_tool_use_id, child, emit);
    if (m.type === 'stream_event') {
      begin();
      const e = m.event;
      if (e.type === 'content_block_start') {
        const block=e.content_block;
        if(block.type==='thinking'||block.type==='redacted_thinking')emit({type:'activity',agentId:child,status:'thinking'});
        else if(block.type==='text')emit({type:'activity',agentId:child,status:'responding'});
        else if(block.type==='tool_use')emit({type:'activity',agentId:child,status:'tool',name:block.name});
      }
      if (e.type === 'content_block_delta' && e.delta.type === 'text_delta' && child) this.streamed.add(child);
      if (e.type === 'content_block_delta' && e.delta.type === 'text_delta') emit({ type: 'message_delta', agentId: child, parentAgentId: child ? 'root' : undefined, text: e.delta.text });
    } else if (m.type === 'system' && m.subtype === 'status') {
      // requesting is the normal API wait, not compaction. null clears a phase.
      if (m.status) begin();
      emit({type:'activity',agentId:child,status:m.status === 'compacting' ? 'compacting' : m.status === 'requesting' ? 'requesting' : 'working'});
    } else if (m.type === 'system' && m.subtype === 'compact_boundary') {
      emit({type:'activity',agentId:child,status:'working'});
      // The conversation says it was compacted, and by how much.
      if (!child) {
        const meta = m.compact_metadata;
        const tokens = (n: unknown) => typeof n === 'number' && Number.isFinite(n) && n >= 0 ? n : undefined;
        emit({ type: 'compacted', data: compact({ trigger: meta?.trigger === 'auto' ? 'auto' : 'manual', preTokens: tokens(meta?.pre_tokens), postTokens: tokens(meta?.post_tokens), durationMs: tokens(meta?.duration_ms) }) });
      }
    } else if (m.type === 'system' && m.subtype === 'local_command_output' && !child) {
      // What a slash command the agent ran itself printed (/usage, say) is its answer.
      const output = typeof m.content === 'string' ? m.content : '';
      if (output) emit({ type: 'message_delta', text: output.length > OUTPUT_LIMIT ? output.slice(0, OUTPUT_LIMIT) + '\n[Output truncated]' : output });
    } else if (m.type === 'rate_limit_event') {
      const limit = m.rate_limit_info;
      const state = limit.status === 'rejected' ? (limit.rateLimitType || limit.errorCode === 'credits_required' ? 'exhausted' : 'throttled') : 'available';
      if (limit.status === 'rejected') this.failure = state;
      emit({ type: limit.status === 'rejected' ? 'failure' : 'capacity_observation', status: state, account: { state, reason: limit.errorCode || limit.rateLimitType, resetAt: limit.resetsAt,
        windows: typeof limit.utilization === 'number' && Number.isFinite(limit.utilization) && limit.utilization >= 0 && limit.rateLimitType ? [{ name: limit.rateLimitType, usedPercent: limit.utilization * 100, resetAt: limit.resetsAt, durationMinutes: limit.rateLimitType === 'five_hour' ? 300 : limit.rateLimitType.startsWith('seven_day') ? 10080 : undefined }] : undefined }, data: limit });
    } else if (m.type === 'auth_status' && m.error) {
      this.failure = 'auth_required'; emit({ type: 'failure', status: this.failure, account: { state: this.failure, reason: 'auth_status' } });
    } else if (m.type === 'assistant') {
      begin();
      const usage=m.message.usage;
      emit({type:'usage',agentId:child,parentAgentId:child?'root':undefined,data:{requestId:m.message.id,model:m.message.model,usage}});
      if(usage){
      const used=usage.input_tokens+(usage.cache_read_input_tokens||0)+(usage.cache_creation_input_tokens||0);
      this.contexts.set(child||'root',{model:m.message.model,used});
      emit({type:'context',agentId:child,data:{model:m.message.model,used,capacity:this.capacities.get(m.message.model)}});
      }
      if (m.error) {
        let state = limitState(m.error);
        if (!child && this.failure === 'exhausted' && (state === 'throttled' || state === 'unknown')) state = this.failure;
        if (!child) this.failure = state;
        emit({ type: 'failure', agentId: child, status: state, account: { state, reason: m.error }, text: m.error });
      }
      // A sub-agent's prose arrives as whole messages (forwardSubagentText),
      // unless it already streamed. Thinking is never forwarded.
      if (child) {
        const streamed = this.streamed.delete(child);
        if (!streamed) for (const content of m.message.content) if (content.type === 'text' && content.text) emit({ type: 'message_delta', agentId: child, parentAgentId: 'root', text: content.text });
      }
      for (const content of m.message.content) if (content.type === 'tool_use') {
        if (this.calls.size >= CALL_LIMIT) this.calls.delete(this.calls.keys().next().value!);
        this.calls.set(content.id, { name: content.name, input: content.input, agent: child || 'root' });
        emit({ type: 'tool_activity', agentId: child, data: {type:'tool_call',id:content.id,name:content.name,status:'running',input:toolPreview(content.input)} });
      }
    } else if (m.type === 'user') {
      if(Array.isArray(m.message.content)) for (const content of m.message.content) if(content.type==='tool_result') {
        emit({type:'tool_activity',agentId:child,data:{type:'tool_call',id:content.tool_use_id,status:content.is_error?'failed':'completed',output:toolPreview(content.content)}});
        emit({type:'activity',agentId:child,status:'working'});
        // A forked run ends with the call that ran it.
        const fork = this.forks.get(content.tool_use_id);
        if (fork) { this.forks.delete(content.tool_use_id); emit({ type: 'agent_status', agentId: fork, status: content.is_error ? 'failed' : 'done', data: { toolUseId: content.tool_use_id } }); }
        this.calls.delete(content.tool_use_id);
      }
    } else if (m.type === 'result') {
      for(const [model,usage] of Object.entries(m.modelUsage||{})) if(usage.contextWindow>0)this.capacities.set(model,usage.contextWindow);
      for(const [id,context] of this.contexts)emit({type:'context',agentId:id,data:{...context,capacity:this.capacities.get(context.model)}});
      emit({ type: 'usage', data: { totalCostUSD: m.total_cost_usd, modelUsage: m.modelUsage, cumulative: true } });
      if (m.is_error && !this.failure) {
        const state = limitState(('startup_failure_reason' in m ? m.startup_failure_reason : '') || m.subtype);
        emit({ type: 'failure', status: state === 'unknown' ? 'provider_error' : state, account: { state, reason: m.subtype }, text: 'errors' in m ? m.errors.join('\n') : m.result });
      }
      if (!child) this.endTurn(emit);
      emit({ type: 'turn_ended', status: m.is_error ? 'failed' : 'completed', data: { subtype: m.subtype, resultUUID: m.uuid } });
      this.active = false; this.failure = '';
    } else if (m.type === 'system' && (m.subtype === 'task_started' || m.subtype === 'task_progress' || m.subtype === 'task_notification')) {
      if ('ambient' in m && m.ambient) return out;
      const old = this.tasks.get(m.task_id);
      if (m.run_id && old?.run && m.run_id < old.run) return out;
      const id = old?.id || (m.tool_use_id ? `claude-tool:${m.tool_use_id}` : `claude-task:${m.task_id}`);
      const status = m.subtype === 'task_notification' ? ({ completed: 'done', failed: 'failed', stopped: 'stopped' }[m.status]) : 'running';
      if (this.tasks.size >= 256 && !old) return out;
      if (m.tool_use_id) this.tools.set(m.tool_use_id, id);
      const background = m.subtype === 'task_started' && typeof m.is_backgrounded === 'boolean' ? m.is_backgrounded : old?.background;
      this.tasks.set(m.task_id, { id, run: m.run_id || old?.run, status, background });
      // A task is not also a forked run.
      if (m.tool_use_id) this.forks.delete(m.tool_use_id);
      const parent = 'parent_task_id' in m && m.parent_task_id ? this.tasks.get(m.parent_task_id)?.id : undefined;
      const data = m.subtype === 'task_started'
        ? compact({ toolUseId: m.tool_use_id, prompt: bounded(m.prompt), subagentType: m.subagent_type, backgrounded: m.is_backgrounded, depth: m.spawn_depth, taskType: m.task_type })
        : m.subtype === 'task_progress'
          ? compact({ toolUseId: m.tool_use_id, usage: usage(m.usage), lastTool: m.last_tool_name, summary: m.summary, subagentType: m.subagent_type })
          : compact({ toolUseId: m.tool_use_id, summary: m.summary, usage: usage(m.usage), reason: m.reason, outputFile: m.output_file });
      emit({ type: 'agent_status', agentId: id, parentAgentId: parent || 'root', nativeId: m.task_id, status,
        name: 'description' in m ? m.description : undefined, agentKind: 'task_type' in m ? m.task_type : undefined, data });
    } else if (m.type === 'system' && m.subtype === 'task_updated') {
      const old = this.tasks.get(m.task_id);
      if (!old || (m.run_id && old.run && m.run_id < old.run)) return out;
      const status = ({ completed: 'done', failed: 'failed', killed: 'stopped', running: 'running', pending: 'starting' } as Record<string, string>)[m.patch.status || ''];
      const error = m.patch.error;
      // A foreground task moved to the background outlives the turn.
      const backgrounded = m.patch.is_backgrounded === true && old.background !== true;
      if (backgrounded) old.background = true;
      if ((!status || status === old.status) && !error && !backgrounded) return out;
      // A finished run is not restarted by a late patch of the same run.
      if (status && TERMINAL.includes(old.status) && !TERMINAL.includes(status) && (!m.run_id || m.run_id === old.run)) return out;
      const next = status || old.status;
      this.tasks.set(m.task_id, { ...old, run: m.run_id || old.run, status: next });
      emit({ type: 'agent_status', agentId: old.id, nativeId: m.task_id, status: next, data: error || backgrounded ? compact({ error, backgrounded: backgrounded || undefined }) : undefined });
    } else if (m.type === 'system' && m.subtype === 'background_tasks_changed') {
      const next = new Set(m.tasks.filter(t => !t.ambient).map(t => t.task_id));
      for (const id of this.background) if (!next.has(id)) {
        const task = this.tasks.get(id);
        if (task?.status === 'running') { task.status = 'unknown'; emit({ type: 'agent_status', agentId: task.id, status: 'unknown' }); }
      }
      for (const task of m.tasks) {
        if (task.ambient || (this.tasks.size >= 256 && !this.tasks.has(task.task_id))) continue;
        const old = this.tasks.get(task.task_id);
        if (old && ['done', 'failed', 'stopped'].includes(old.status) && (!task.run_id || task.run_id === old.run)) continue;
        if (task.run_id && old?.run && task.run_id < old.run) continue;
        const id = old?.id || `claude-task:${task.task_id}`;
        this.tasks.set(task.task_id, { id, run: task.run_id || old?.run, status: 'running', background: true });
        emit({ type: 'agent_status', agentId: id, parentAgentId: this.tasks.get(task.parent_task_id || '')?.id || 'root', nativeId: task.task_id, name: task.description, agentKind: task.task_type, status: 'running', data: { backgrounded: true } });
      }
      this.background = next;
    }
    return out;
  }

  /**
   * The first word of a child no task names: a run the call `id` forked (a
   * Skill with `context: fork`). It is a sub-agent from now until the
   * call's result; the provider reports nothing else of it.
   */
  private fork(id: string, child: string, emit: (e: Event) => void) {
    if (this.tools.has(id) || this.forks.has(id) || this.forks.size >= FORK_LIMIT) return;
    const call = this.calls.get(id);
    // A task's own events may come before its task_started: it reports itself.
    if (!call || SPAWN_TOOLS.has(call.name)) return;
    this.forks.set(id, child);
    const input = call.input && typeof call.input === 'object' ? call.input as Record<string, unknown> : {};
    const skill = typeof input.skill === 'string' && input.skill.trim() ? input.skill.trim() : '';
    emit({ type: 'agent_status', agentId: child, parentAgentId: call.agent, status: 'running', name: skill || call.name, agentKind: skill ? 'skill' : call.name, data: { toolUseId: id } });
  }

  /**
   * The main agent's turn ended: what the turn blocked on cannot still run.
   * A foreground task (the spawning call waits for it) and a forked run end
   * with it; one the provider did not report the end of ended unreported.
   * Background tasks and commands outlive the turn and are left alone.
   */
  private endTurn(emit: (e: Event) => void) {
    for (const [call, id] of this.forks) emit({ type: 'agent_status', agentId: id, status: 'unknown', data: { toolUseId: call, reason: 'turn_ended' } });
    this.forks.clear();
    for (const [taskId, task] of this.tasks) {
      if (task.background !== false || this.background.has(taskId) || !['running', 'starting'].includes(task.status)) continue;
      task.status = 'unknown';
      emit({ type: 'agent_status', agentId: task.id, nativeId: taskId, status: 'unknown', data: { reason: 'turn_ended' } });
    }
  }
}
