import type { Options, SDKMessage } from '@anthropic-ai/claude-agent-sdk';
import type { StreamEnd } from './stream.js';

/**
 * Settings Claude did not apply.
 *
 * Since SDK 0.3.330 (Claude Code 2.1.330) Claude says when settings did not
 * load or were skipped: a system `informational` message at level `warning`,
 * tagged `settings_not_in_effect`, whose content lists the files and entries
 * (or, where the transport keeps messages, says so in general). The settings
 * the worker passes (`settings`, shown as "SDK inline settings") are among
 * those it may reject.
 *
 * Lazybox's "ask" permission and the help assistant rest on the worker's own
 * `ask` rules (options.ts): without them a saved allow rule or Claude's
 * read-only auto-allow would run a command nobody approved. So such a session
 * stops instead of continuing silently; any other session reports the warning
 * as a failure the person sees, and goes on.
 */
export const settingsNotInEffectTag = 'settings_not_in_effect';

/** The warning's words, when the message is Claude's report of settings not in effect. */
export function settingsNotInEffect(message: SDKMessage): string | undefined {
  const m = message as { type?: unknown; subtype?: unknown; tag?: unknown; content?: unknown };
  if (m.type !== 'system' || m.subtype !== 'informational' || m.tag !== settingsNotInEffectTag) return undefined;
  return typeof m.content === 'string' && m.content.trim() ? m.content.trim() : 'Some settings did not load or were skipped, so they are not in effect.';
}

/** Whether this session's approvals rest on the worker's own `ask` rules. */
export function asksBySettings(options: Pick<Options, 'settings'>): boolean {
  const s = options.settings;
  const ask = s && typeof s === 'object' ? (s as { permissions?: { ask?: unknown } }).permissions?.ask : undefined;
  return Array.isArray(ask) && ask.length > 0;
}

/**
 * What the worker does with the warning: end the session (`end`, reported as
 * `sdk_error`, as when its SDK stream fails: nothing takes the next message,
 * and Resume starts Claude again, which checks its settings again), or report
 * it and go on (`report`, a provider error).
 */
export function settingsFailure(warning: string, asks: boolean): { end: StreamEnd } | { report: { type: 'failure'; status: 'provider_error'; text: string } } {
  if (asks) return { end: { type: 'failure', status: 'sdk_error', text: `Claude did not apply some settings, and this conversation's approvals rely on Lazybox's own, so it stopped rather than run a command without asking you. Claude said: ${warning}` } };
  return { report: { type: 'failure', status: 'provider_error', text: `Claude did not apply some settings: ${warning}` } };
}

/** Watches the SDK stream for the warning and applies settingsFailure. */
export class SettingsWatch {
  private ended: StreamEnd | undefined;
  constructor(private readonly asks: boolean, private readonly stop: () => void, private readonly report: (failure: { type: 'failure'; status: 'provider_error'; text: string }) => void) {}
  /** Whether the message goes on to be read; once the watch ended the session, none does. */
  pass(message: SDKMessage): boolean {
    if (this.ended) return false;
    const warning = settingsNotInEffect(message);
    if (!warning) return true;
    const what = settingsFailure(warning, this.asks);
    if ('end' in what) {
      this.ended = what.end;
      this.stop();
      return false;
    }
    this.report(what.report);
    return true;
  }
  /** Why the session ended, when the watch ended it. */
  get end(): StreamEnd | undefined { return this.ended; }
}
