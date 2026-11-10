import type { Options } from '@anthropic-ai/claude-agent-sdk';

type ExecutionOptions = {
  cwd: string; model?: string; resume?: string; context?: string;
  purpose?: string; binary?: string; permission?: string; workflow?: string;
};

/** Resolved startup options; authentication remains in the selected environment. */
export function nativeOptions(o: ExecutionOptions, env: NodeJS.ProcessEnv): Options {
  return {
    cwd: o.cwd, model: o.model || undefined, resume: o.resume || undefined,
    ...(o.context ? { systemPrompt: { type: 'preset', preset: 'claude_code', append: o.context } } : {}),
    pathToClaudeCodeExecutable: o.binary || undefined,
    // Omit settingSources and permissionMode to retain the native CLI settings
    // chain and configured mode. The SDK handles deny/allow rules before callbacks.
    env, includePartialMessages: true,
    ...(['help', 'planning', 'summary'].includes(o.purpose || '') ? { tools: [], strictMcpConfig: true } : {}),
    ...(o.permission === 'read-only' || o.workflow === 'plan' ? { permissionMode: 'plan' as const }
      : o.permission === 'yolo' ? { permissionMode: 'bypassPermissions' as const, allowDangerouslySkipPermissions: true }
      : o.permission === 'ask' ? { permissionMode: 'default' as const, settings: { permissions: { ask: ['Bash', 'mcp__lazybox__command'] } } }
      : {}),
  };
}
