import type { AttentionHost } from '@lazybox/platform';
import type { Attention, AttentionNotice, NotificationPreferences } from './attention';
import { noticeFor } from './attention';
import { playAlert } from './sounds';
import { showToast } from './toasts';

const wanted: Record<AttentionNotice['kind'], keyof NotificationPreferences> = { 'needs-input': 'needsInput', failed: 'failed', review: 'review' };
const everything: NotificationPreferences = { needsInput: true, failed: true, review: true, badge: true, soundNeedsYou: true, soundDone: true };

/** Pure: what changed between two observations, before any preference applies. */
export function attentionChanges(before: Map<string, Attention>, rows: readonly Attention[], viewing: (row: Attention) => boolean, workspaceName: (row: Attention) => string | undefined): AttentionNotice[] {
  return rows.flatMap(row => {
    const notice = noticeFor(before.get(row.key), row, everything, viewing(row), workspaceName(row));
    return notice ? [notice] : [];
  });
}

/** Pure: which sound, if any, a set of changes deserves. "Needs you" (input or a failure) outranks "done". */
export function alertSoundFor(notices: readonly AttentionNotice[], preferences: NotificationPreferences): 'request' | 'done' | undefined {
  if (preferences.soundNeedsYou && notices.some(notice => notice.kind !== 'review')) return 'request';
  if (preferences.soundDone && notices.some(notice => notice.kind === 'review')) return 'done';
  return undefined;
}

/**
 * Tells the person: a toast when they are looking at this window (a system
 * banner would repeat what is on screen), a system notification otherwise,
 * and a short sound when they asked for one.
 */
export function deliverAttention(notices: readonly AttentionNotice[], preferences: NotificationPreferences, { host, focused, open, nextKey }: { host: AttentionHost; focused: boolean; open(notice: AttentionNotice): void; nextKey?: string }) {
  const sound = alertSoundFor(notices, preferences);
  if (sound) playAlert(sound);
  const shown = notices.filter(notice => preferences[wanted[notice.kind]]);
  if (!shown.length) return;
  if (focused) {
    for (const notice of shown.slice(-3)) showToast({
      id: `attention:${notice.key}`, tone: notice.kind, title: notice.title, body: notice.body,
      action: { label: 'Open', run: () => open(notice) }, hint: notice.kind === 'review' ? undefined : nextKey,
    });
    if (shown.length > 3) showToast({ id: 'attention:more', tone: 'info', title: `${shown.length - 3} more conversations need you`, body: 'Open the inbox to see them all.' });
    return;
  }
  // A burst (many agents failing at once) is one summary, not a wall of banners.
  const banners = shown.length > 3 ? [...shown.slice(0, 2), { ...shown[2], title: `${shown.length - 2} more conversations need you`, body: 'Open the Lazybox inbox to see them all.' }] : shown;
  for (const notice of banners) void host.notifyAttention?.(notice).catch(() => {});
}
