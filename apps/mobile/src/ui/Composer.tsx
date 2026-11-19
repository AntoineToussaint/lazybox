import { useRef, useState } from 'react';
import { StyleSheet, Text, TextInput, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { randomUUID } from 'expo-crypto';
import { ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { Session } from '@lazybox/protocol';
import { humanError, mergeDictation } from '@lazybox/surfaces/models';
import { composerView } from '../models/conversation';
import { Button, IconButton, haptic } from './kit';
import { radius, space, useTheme } from './theme';
import { useDictation } from './useDictation';

/** Drafts outlive leaving the conversation (the PWA keeps them per tab session). */
const drafts = new Map<string, string>();

/**
 * The composer above the keyboard. Idle, Send sends. While a turn runs, the
 * message queues behind it (Queue), and Send now queues it and stops the
 * current step so it goes next, as on the computer (DeliverQueued "now").
 * A retried send keeps its operation id, so it is never delivered twice.
 */
export function Composer({ session, busy, grants, api, onSent, onError }: { session?: Session; busy: boolean; grants: readonly string[]; api: RunnerClient; onSent(target: string): void; onError(message: string): void }) {
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const offered = composerView(session, grants, busy);
  const key = session?.id || '';
  const [text, setText] = useState(() => drafts.get(key) || '');
  const [sending, setSending] = useState<'send' | 'now' | 'stop'>();
  const attempt = useRef('');
  const change = (value: string) => { setText(value); attempt.current = ''; if (value) drafts.set(key, value); else drafts.delete(key); };
  const dictation = useDictation(offered.mode !== 'readonly' && offered.mode !== 'ended', transcript => {
    change(mergeDictation(text, '', transcript).text);
  });
  const dictating = dictation.phase !== 'idle';
  const pad = { paddingBottom: Math.max(insets.bottom, space.s) };
  if (offered.mode === 'readonly' || offered.mode === 'ended') return <View style={[styles.bar, styles.readonly, pad, { borderTopColor: theme.lineSubtle, backgroundColor: theme.bg }]}>
    <Text style={{ color: theme.muted, fontSize: 14, textAlign: 'center' }}>{offered.note}</Text>
  </View>;
  const view = offered;
  const send = async (now: boolean) => {
    const message = text.trim();
    if (!message || sending || !session) return;
    setSending(now ? 'now' : 'send'); onError('');
    attempt.current ||= randomUUID();
    try {
      let target = session.id;
      if (view.resume) {
        const resumed = await api.resumeSession({ sessionId: session.id }, { timeoutMs: 60000 });
        if (!resumed.session) throw new Error('The conversation could not be continued');
        target = resumed.session.id;
      }
      await api.apply({ sessionId: target, operationId: attempt.current, operation: { case: 'message', value: { text: message, attachments: [] } } }, { timeoutMs: 30000 });
      // Behind a busy turn the message was queued: Send now delivers it next, the turn stopped for it.
      if (now) await api.apply({ sessionId: target, operationId: randomUUID(), operation: { case: 'deliverQueued', value: { operationId: attempt.current, how: 'now' } } }, { timeoutMs: 30000 });
      haptic.success();
      attempt.current = ''; change(''); onSent(target);
    } catch (error) {
      haptic.error();
      onError(`Not sent: ${humanError(ConnectError.from(error))} Your message is kept; try again.`);
    } finally { setSending(undefined); }
  };
  const interrupt = async () => {
    if (!session) return;
    setSending('stop');
    try { await api.apply({ sessionId: session.id, operationId: randomUUID(), operation: { case: 'interrupt', value: {} } }, { timeoutMs: 15000 }); haptic.success(); }
    catch (error) { haptic.error(); onError(humanError(ConnectError.from(error))); }
    finally { setSending(undefined); }
  };
  const ready = !!text.trim() && !sending && !dictating;
  const status = dictation.phase === 'starting' ? 'Starting dictation…'
    : dictation.phase === 'listening' ? 'Listening…'
    : dictation.phase === 'finishing' ? 'Finishing dictation…' : '';
  return <View style={[styles.bar, pad, { borderTopColor: theme.lineSubtle, backgroundColor: theme.bg }]}>
    {status ? <Text accessibilityLiveRegion="polite" style={{ color: theme.accent, fontSize: 13 }}>{status}</Text> : null}
    {dictation.error ? <Text accessibilityRole="alert" style={{ color: theme.danger, fontSize: 13 }}>{dictation.error}</Text> : null}
    {!dictating && view.mode === 'busy' && (!session?.pending.length || ready) && <View style={styles.busy}>
      <Text style={{ flex: 1, color: theme.muted, fontSize: 13 }}>{ready ? 'Queue for after this turn' : 'Agent working'}</Text>
      {view.sendNow && ready && <Button label="Send now" onPress={() => void send(true)} testID="send-now"/>}
      {view.interrupt && !session?.pending.length && <IconButton label="Stop turn" icon="stop.fill" color={theme.danger} disabled={!!sending} onPress={() => void interrupt()} testID="interrupt"/>}
    </View>}
    <View style={styles.row}>
      <TextInput value={dictation.preview ? mergeDictation(text, '', dictation.preview).text : text} onChangeText={change} editable={!dictating && !sending} multiline placeholder={view.placeholder} placeholderTextColor={theme.faint} testID="composer"
        accessibilityLabel="Message" style={[styles.input, { color: theme.text, backgroundColor: theme.surface, borderColor: theme.line }]}/>
      <IconButton icon={dictating ? 'stop.circle.fill' : 'mic'} label={dictating ? 'Finish dictation' : 'Dictate message'}
        disabled={!!sending || dictation.phase === 'starting' || dictation.phase === 'finishing'} onPress={() => void dictation.toggle()} testID="dictate"/>
      <IconButton icon={view.mode === 'busy' ? 'text.append' : 'arrow.up.circle.fill'} size={view.mode === 'busy' ? 22 : 32} label={view.send === 'Queue' ? 'Queue message' : 'Send message'}
        color={ready ? theme.accentSolid : theme.disabled} disabled={!ready} onPress={() => void send(false)} testID="send"/>
    </View>
  </View>;
}

const styles = StyleSheet.create({
  bar: { borderTopWidth: StyleSheet.hairlineWidth, paddingTop: space.s, paddingHorizontal: space.m, gap: space.s },
  readonly: { paddingHorizontal: space.xl, paddingTop: space.m },
  busy: { flexDirection: 'row', alignItems: 'center', gap: space.s, paddingHorizontal: space.xs },
  row: { flexDirection: 'row', alignItems: 'flex-end', gap: space.xs },
  input: { flex: 1, minHeight: 40, maxHeight: 140, borderRadius: radius.l + 4, borderWidth: StyleSheet.hairlineWidth, paddingHorizontal: 14, paddingTop: 10, paddingBottom: 10, fontSize: 16, lineHeight: 21 },
});
