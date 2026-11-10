import { query, type AccountInfo, type SDKControlGetUsageResponse } from '@anthropic-ai/claude-agent-sdk';

// This version-pinned experimental SDK method is deliberately confined here.
// Percentages in this response are 0–100, unlike rate_limit_event fractions.
export function quotaObservation(report: SDKControlGetUsageResponse, account: Pick<AccountInfo, 'email'> = {}) {
  if (!report.rate_limits_available || !report.rate_limits) throw new Error('Claude subscription quota is unavailable');
  const windows: {name: string; usedPercent: number; durationMinutes: number; resetAt: number}[] = [];
  const add = (name: string, value: {utilization: number | null; resets_at: string | null} | null | undefined, durationMinutes: number) => {
    if (!value || typeof value.utilization !== 'number' || !Number.isFinite(value.utilization) || value.utilization < 0) return;
    const reset = value.resets_at ? Date.parse(value.resets_at) : NaN;
    windows.push({name, usedPercent: value.utilization, durationMinutes, resetAt: Number.isFinite(reset) ? Math.floor(reset / 1000) : 0});
  };
  add('five_hour', report.rate_limits.five_hour, 300);
  for (const name of ['seven_day', 'seven_day_oauth_apps', 'seven_day_opus', 'seven_day_sonnet'] as const) add(name, report.rate_limits[name], 10080);
  for (const value of report.rate_limits.model_scoped || []) add(`model:${value.display_name}`, value, 10080);
  if (!windows.length) throw new Error('Claude did not report subscription quota windows');
  const blocked = !report.rate_limits.extra_usage?.is_enabled && windows.filter(w => ['five_hour', 'seven_day'].includes(w.name) && w.usedPercent >= 100);
  const exhausted = blocked && blocked.length > 0;
  return {email: account.email?.trim() || '', state: exhausted ? 'exhausted' : 'available', reason: 'native_usage_query', plan: report.subscription_type || '', windows, resetAt: exhausted ? Math.max(...blocked.map(w => w.resetAt)) : 0};
}

export async function readQuota(options: {cwd: string; binary?: string}) {
  const controller = new AbortController();
  async function* noPrompts(): AsyncGenerator<never> {
    await new Promise<void>(resolve => { if (controller.signal.aborted) resolve(); else controller.signal.addEventListener('abort', () => resolve(), {once: true}); });
  }
  const q = query({prompt: noPrompts(), options: {
    cwd: options.cwd, env: process.env, pathToClaudeCodeExecutable: options.binary || undefined,
    abortController: controller, tools: [], mcpServers: {}, strictMcpConfig: true,
    settingSources: [], settings: {disableAllHooks: true}, persistSession: false,
  }});
  try {
    if (typeof q.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET !== 'function') throw new Error('Installed Claude SDK does not support usage queries');
    const [usage, identity] = await Promise.all([q.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET({skipBehaviors: true}), q.accountInfo().catch(() => ({}))]);
    return quotaObservation(usage, identity);
  } finally { controller.abort(); q.close(); }
}
