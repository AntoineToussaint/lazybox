import type { Options, SDKMessage } from '@anthropic-ai/claude-agent-sdk';
import type { StreamEnd } from './stream.js';

/**
 * Settings Claude did not apply.
 *
 * Since SDK 0.3.330 (Claude Code 2.1.330) Claude says when settings did not
 * load or were skipped: a system `informational` message at level `warning`,
 * tagged `settings_not_in_effect`, whose content lists the files and entries
 * (or, where the transport keeps messages, says so in general). The settings
 * the worker passes (`settings`) are among those it may reject; Claude names
 * them "SDK inline settings".
 *
 * Nothing should stop an agent from working, but approvals must never turn
 * off silently. Lazybox's "ask" permission and the help assistant rest on the
 * worker's own `ask` rules (options.ts): without them a saved allow rule or
 * Claude's read-only auto-allow would run a command nobody approved. So such a
 * session stops only when Claude rejected Lazybox's own permission settings
 * (or did not say which settings it rejected); when it rejected only the
 * person's settings files, or Lazybox's other settings, the warning shows in
 * the conversation, naming them, and the agent goes on.
 */
/** Claude's name for the settings the SDK passes inline: the worker's own. */
export const inlineSettingsName = 'SDK inline settings';
// The headings of the lists in Claude Code 2.1.330's warning.
const NOT_LOADED = 'These settings did not load';
const SKIPPED = 'These settings entries were skipped';
const NOT_USED = 'These settings files are not used';

/**
 * Whether the warning says Lazybox's own permission settings are not in
 * effect: its inline settings did not load at all, or an entry under
 * `permissions` (or one with no path) was skipped. A warning that lists
 * nothing (Claude's general wording) cannot rule that out, so it counts.
 */
export function rejectsLazyboxPermissions(warning: string): boolean {
  let section: 'not_loaded' | 'skipped' | 'other' | undefined;
  let listed = false;
  for (const line of warning.split('\n')) {
    if (line.startsWith(NOT_LOADED)) { section = 'not_loaded'; listed = true; continue; }
    if (line.startsWith(SKIPPED)) { section = 'skipped'; listed = true; continue; }
    if (line.startsWith(NOT_USED)) { section = 'other'; listed = true; continue; }
    if (!line.startsWith('  ')) { section = undefined; continue; }
    if (section !== 'not_loaded' && section !== 'skipped') continue;
    // "  SDK inline settings (permissions.ask[0]): …", or "  … and 3 more in SDK inline settings".
    const entry = line.trim();
    const more = entry.match(/^… and \d+ more in (.+)$/);
    const name = more ? more[1] : entry.replace(/( \(.*\))?: .*$/, '');
    if (name !== inlineSettingsName) continue;
    if (section === 'not_loaded' || more) return true;
    const path = entry.match(/^[^(]*\((.*?)\): /)?.[1] ?? '';
    if (path === '' || path === 'permissions' || path.startsWith('permissions.') || path.startsWith('permissions[')) return true;
  }
  return !listed;
}
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
 * and Resume starts Claude again, which checks its settings again) when its
 * approvals rest on Lazybox's permission settings and those are not in
 * effect; otherwise report it, naming what did not load, and go on
 * (`report`, a provider error).
 */
export function settingsFailure(warning: string, asks: boolean): { end: StreamEnd } | { report: { type: 'failure'; status: 'provider_error'; text: string } } {
  if (asks && rejectsLazyboxPermissions(warning)) return { end: { type: 'failure', status: 'sdk_error', text: `Claude did not apply Lazybox's approval settings, which this conversation relies on, so it stopped rather than run a command without asking you. Claude said: ${warning}` } };
  const still = asks ? ' Lazybox\'s approval settings are in effect, and the agent goes on.' : '';
  return { report: { type: 'failure', status: 'provider_error', text: `Claude did not apply some settings.${still} Claude said: ${warning}` } };
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
