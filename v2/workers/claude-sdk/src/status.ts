import type { SDKMessage } from '@anthropic-ai/claude-agent-sdk';

type Event = { type: string; agentId?: string; parentAgentId?: string; nativeId?: string; name?: string; agentKind?: string; status?: string; sessionId?: string; text?: string; data?: unknown; account?: { state: string; reason?: string; resetAt?: number; windows?: { name: string; usedPercent: number; resetAt?: number; durationMinutes?: number }[] } };
// Inspection payloads must not exhaust the durable event budget. Tool execution
// receives the original values; this bounds only the retained UI preview.
function toolPreview(value:unknown):unknown {
 const text=typeof value==='string'?value:JSON.stringify(value);
 return text&&text.length>32768?text.slice(0,32768)+'\n[Tool preview truncated]':value;
}
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
  private tasks = new Map<string, { id: string; run?: string; status: string }>();
  private background = new Set<string>();
  private tools = new Map<string, string>();
  private contexts = new Map<string, { model: string; used: number }>();
  private capacities = new Map<string, number>();
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
    if (m.type === 'stream_event') {
      begin();
      const e = m.event;
      if (e.type === 'content_block_start') {
        const block=e.content_block;
        if(block.type==='thinking'||block.type==='redacted_thinking')emit({type:'activity',agentId:child,status:'thinking'});
        else if(block.type==='text')emit({type:'activity',agentId:child,status:'responding'});
        else if(block.type==='tool_use')emit({type:'activity',agentId:child,status:'tool',name:block.name});
      }
      if (e.type === 'content_block_delta' && e.delta.type === 'text_delta') emit({ type: 'message_delta', agentId: child, parentAgentId: child ? 'root' : undefined, text: e.delta.text });
    } else if (m.type === 'system' && m.subtype === 'status') {
      // requesting is the normal API wait, not compaction. null clears a phase.
      if (m.status) begin();
      emit({type:'activity',agentId:child,status:m.status === 'compacting' ? 'compacting' : m.status === 'requesting' ? 'requesting' : 'working'});
    } else if (m.type === 'system' && m.subtype === 'compact_boundary') {
      emit({type:'activity',agentId:child,status:'working'});
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
      for (const content of m.message.content) if (content.type === 'tool_use') emit({ type: 'tool_activity', agentId: child, data: {type:'tool_call',id:content.id,name:content.name,status:'running',input:toolPreview(content.input)} });
    } else if (m.type === 'user') {
      if(Array.isArray(m.message.content)) for (const content of m.message.content) if(content.type==='tool_result') {
        emit({type:'tool_activity',agentId:child,data:{type:'tool_call',id:content.tool_use_id,status:content.is_error?'failed':'completed',output:toolPreview(content.content)}});
        emit({type:'activity',agentId:child,status:'working'});
      }
    } else if (m.type === 'result') {
      for(const [model,usage] of Object.entries(m.modelUsage||{})) if(usage.contextWindow>0)this.capacities.set(model,usage.contextWindow);
      for(const [id,context] of this.contexts)emit({type:'context',agentId:id,data:{...context,capacity:this.capacities.get(context.model)}});
      emit({ type: 'usage', data: { totalCostUSD: m.total_cost_usd, modelUsage: m.modelUsage, cumulative: true } });
      if (m.is_error && !this.failure) {
        const state = limitState(('startup_failure_reason' in m ? m.startup_failure_reason : '') || m.subtype);
        emit({ type: 'failure', status: state === 'unknown' ? 'provider_error' : state, account: { state, reason: m.subtype }, text: 'errors' in m ? m.errors.join('\n') : m.result });
      }
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
      this.tasks.set(m.task_id, { id, run: m.run_id || old?.run, status });
      const parent = 'parent_task_id' in m && m.parent_task_id ? this.tasks.get(m.parent_task_id)?.id : undefined;
      emit({ type: 'agent_status', agentId: id, parentAgentId: parent || 'root', nativeId: m.task_id, status,
        name: 'description' in m ? m.description : undefined, agentKind: 'task_type' in m ? m.task_type : undefined });
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
        this.tasks.set(task.task_id, { id, run: task.run_id || old?.run, status: 'running' });
        emit({ type: 'agent_status', agentId: id, parentAgentId: this.tasks.get(task.parent_task_id || '')?.id || 'root', nativeId: task.task_id, name: task.description, agentKind: task.task_type, status: 'running' });
      }
      this.background = next;
    }
    return out;
  }
}
