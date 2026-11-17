import type { AgentCliCheck } from '@lazybox/protocol';

/**
 * An agent may call the Lazybox CLI by its full path (quoted when it has
 * spaces) or through `$LAZYBOX_V2_CLI`: the briefing's fallback when another
 * lazybox, such as V1's, comes first on its shell's PATH. Mirrors
 * `lazyboxCLI` in services/runner/code/internal/serverlike.
 */
const LAZYBOX_CLI = /"[^"]*\/lazybox"|'[^']*\/lazybox'|"\$(?:LAZYBOX_V2_CLI|\{LAZYBOX_V2_CLI\})"|\$(?:\{LAZYBOX_V2_CLI\}|LAZYBOX_V2_CLI\b)|(?<![\w./-])\/[^\s'"`;&|()<>]*\/lazybox(?=[\s;&|)]|$)/g;

/** A command with the Lazybox CLI, however it was called, read as plain `lazybox`. */
export const plainLazybox = (command: string) => command.replace(LAZYBOX_CLI, 'lazybox');

/** A short status for the lazybox agents reach (Settings → About → Agents). */
export function agentCliStatus(check: Pick<AgentCliCheck, 'problem' | 'unfixed' | 'shadow'>): { label: string; tone: 'ok' | 'warn' } {
  if (check.problem) return { label: 'Not checked', tone: 'warn' };
  if (check.unfixed.length) return { label: 'Needs full path', tone: 'warn' };
  if (check.shadow) return { label: 'Fixed', tone: 'ok' };
  return { label: 'OK', tone: 'ok' };
}
