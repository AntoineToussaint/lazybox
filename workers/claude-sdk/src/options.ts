import type { Options, Settings } from '@anthropic-ai/claude-agent-sdk';
import { helpPermissions } from './settings.js';
import { browserRule } from './browser.js';
import { COMMAND_TOOL_NAMES } from './permissions.js';

type ExecutionOptions = {
  cwd: string; model?: string; effort?: Options['effort']; resume?: string; context?: string;
  purpose?: string; binary?: string; permission?: string; workflow?: string;
  /** The Runner offers the agent browser: its tools need no per-call prompt (the Runner asks per site). */
  browser?: boolean;
  /** The session may load the person's claude.ai connectors; anything but true turns them off. */
  claudeAiConnectors?: boolean;
};

/**
 * Turns the person's claude.ai connectors (Claude's cloud MCP servers: Claude
 * Docs, Gmail, Notion…) off for one session, so an agent's documents become
 * Lazybox artifacts rather than pages in another product. The setting is the
 * typed switch of the CLI this SDK ships; the environment variable is the
 * older spelling, for a configured Claude binary that predates the setting.
 * Neither touches MCP servers the person configured locally (user, project,
 * .mcp.json or plugins), nor the connectors on claude.ai itself.
 */
export const connectorsOff = { setting: { disableClaudeAiConnectors: true } satisfies Settings, env: { ENABLE_CLAUDEAI_MCP_SERVERS: 'false' } };

/** Resolved startup options; authentication remains in the selected environment. */
export function nativeOptions(o: ExecutionOptions, env: NodeJS.ProcessEnv): Options {
  const options = baseOptions(o, env);
  // The browser asks the person per site, download and sign-in itself; a
  // prompt per click on top would teach them to click through.
  if (o.browser && o.purpose !== 'help') {
    const permissions = options.settings && typeof options.settings === 'object' ? (options.settings as { permissions?: Record<string, string[]> }).permissions || {} : {};
    options.settings = { ...(typeof options.settings === 'object' ? options.settings : {}), permissions: { ...permissions, allow: [...(permissions.allow || []), browserRule] } };
  }
  return options;
}

function baseOptions(o: ExecutionOptions, env: NodeJS.ProcessEnv): Options {
  const connectors = o.claudeAiConnectors === true;
  const off: Settings = connectors ? {} : connectorsOff.setting;
  const permission: Partial<Options> =
    // The help assistant's only tools are Lazybox's settings: reading them is
    // allowed, and every update asks, whatever the Runner permission says.
    o.purpose === 'help' ? { permissionMode: 'default', settings: { permissions: helpPermissions, ...off } }
      : o.permission === 'read-only' || o.workflow === 'plan' ? { permissionMode: 'plan' }
      : o.permission === 'yolo' ? { permissionMode: 'bypassPermissions', allowDangerouslySkipPermissions: true }
      // Every tool that runs a command asks (Bash, Monitor, PowerShell, the
      // managed command tool): the rule outranks a saved allow rule and
      // Claude's read-only auto-allow, which would otherwise run it unasked.
      : o.permission === 'ask' ? { permissionMode: 'default', settings: { permissions: { ask: [...COMMAND_TOOL_NAMES] }, ...off } }
      : {};
  const flagSettings = permission.settings ?? (connectors ? undefined : off);
  return {
    cwd: o.cwd, model: o.model || undefined, effort: o.effort || undefined, resume: o.resume || undefined,
    ...(o.context ? { systemPrompt: { type: 'preset', preset: 'claude_code', append: o.context } } : {}),
    pathToClaudeCodeExecutable: o.binary || undefined,
    // Omit settingSources and permissionMode to retain the native CLI settings
    // chain and configured mode. The SDK handles deny/allow rules before callbacks.
    // Flag settings layer on top of that chain; they replace none of it.
    env: connectors ? env : { ...env, ...connectorsOff.env }, includePartialMessages: true,
    // A sub-agent's own prose reaches the conversation, so its timeline can be
    // read; coding agents also report a one-line progress summary while they run.
    forwardSubagentText: true,
    ...(['help', 'planning', 'summary'].includes(o.purpose || '') ? {} : { agentProgressSummaries: true }),
    ...(['help', 'planning', 'summary'].includes(o.purpose || '') ? { tools: [], strictMcpConfig: true } : {}),
    ...permission,
    ...(flagSettings ? { settings: flagSettings } : {}),
  };
}
