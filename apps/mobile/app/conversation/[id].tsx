import { useCallback, useEffect, useMemo, useState, useSyncExternalStore } from 'react';
import { Alert, FlatList, KeyboardAvoidingView, Pressable, StyleSheet, Text, View } from 'react-native';
import { Stack, useLocalSearchParams } from 'expo-router';
import { useHeaderHeight } from 'expo-router/react-navigation';
import type { Session } from '@lazybox/protocol';
import { humanError } from '@lazybox/surfaces/models';
import { agentTitle, conversationName, conversationReader, deriveAttention, liveStatus, queuedMessages, seenMark, sessionStatus } from '@lazybox/surfaces/models';
import { randomUUID } from 'expo-crypto';
import { requestView, transcriptRows, turnRunning } from '../../src/models/conversation';
import { connectionView } from '../../src/models/connection';
import { useRunner } from '../../src/runner/RunnerProvider';
import { markRead } from '../../src/runner/seen';
import { answer, applyOperation, approval, askDenyReason, confirmAllow } from '../../src/runner/actions';
import { Composer } from '../../src/ui/Composer';
import { RequestCard } from '../../src/ui/RequestCard';
import { TranscriptRow } from '../../src/ui/TranscriptRow';
import { Button, ConnectionBanner, Icon, LiveDot, haptic } from '../../src/ui/kit';
import { radius, space, useTheme } from '../../src/ui/theme';

const idle = () => () => {};

/**
 * A conversation: its latest turns first (ReadSessionTail), then live
 * (WatchEvents from the cursor) through the shared ConversationReader, the
 * pending request with Allow, Deny or the answer, and the composer. The
 * stream runs only while the app is in front; coming back reads on from the
 * cursor, so nothing is missed or shown twice.
 */
export default function ConversationScreen() {
  const { id } = useLocalSearchParams<{ id: string }>();
  const { api, info, collections, device, error: pollError, foreground, refresh } = useRunner();
  const theme = useTheme();
  const headerHeight = useHeaderHeight();
  const [fetched, setFetched] = useState<Session>();
  const [error, setError] = useState('');
  const live = info?.sessions.find(value => value.id === id);
  const session = live || fetched;
  // An ended conversation is not in the live list: read it, and keep reading while it is shown.
  useEffect(() => {
    if (!api || !foreground) return;
    const control = new AbortController();
    const load = () => void api.getSession({ sessionId: id }, { signal: control.signal, timeoutMs: 10000 }).then(result => { if (!control.signal.aborted) setFetched(result.session); }, () => {});
    load(); const timer = setInterval(load, 4000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, id, foreground]);

  const reader = useMemo(() => api ? conversationReader(api, id) : undefined, [api, id]);
  const transcript = useSyncExternalStore(reader && foreground ? reader.subscribe : idle, () => reader?.getSnapshot());
  const agentName = agentTitle(session?.provider, info?.profiles);
  const rows = useMemo(() => transcriptRows(transcript?.messages || [], { agentName, turnRunning: !!transcript?.turnRunning && !session?.pending.length }), [transcript?.messages, transcript?.turnRunning, session?.pending.length, agentName]);
  const reversed = useMemo(() => [...rows].reverse(), [rows]);
  const respond = !!device?.grants.includes('respond');
  const workspace = collections.find(value => value.id === session?.collectionId)?.name || '';
  const connection = connectionView({ hasInfo: !!info, error: pollError, stream: transcript?.status, foreground });
  const [now, setNow] = useState(Date.now);
  useEffect(() => { if (!session?.busy) return; const timer = setInterval(() => setNow(Date.now()), 1000); return () => clearInterval(timer); }, [session?.busy]);
  const status = liveStatus(session, now, transcript?.recent);
  // Opening a conversation marks what it shows as read.
  useEffect(() => { if (session) { const row = deriveAttention(session); markRead(row.key, seenMark(row)); } }, [session]);

  const busy = turnRunning(session, transcript);
  // What waits behind the turn: the Runner's queue and the transcript's, merged as on the computer.
  const queued = useMemo(() => transcript ? queuedMessages(session?.queuedMessages, transcript) : [], [session?.queuedMessages, transcript?.pending, transcript?.messages]);
  const [delivering, setDelivering] = useState('');
  const sendNow = useCallback(async (operationId: string) => {
    if (!api || !session) return;
    setDelivering(operationId);
    try { await api.apply({ sessionId: session.id, operationId: randomUUID(), operation: { case: 'deliverQueued', value: { operationId, how: 'now' } } }, { timeoutMs: 30000 }); haptic.success(); void refresh(); }
    catch (e) { haptic.error(); setError(humanError(e)); }
    finally { setDelivering(''); }
  }, [api, session, refresh]);
  const requests = session && !session.closed ? session.pending.map(requestView) : [];
  const decide = useCallback(async (requestId: string, accept: boolean, what: string, risks: Parameters<typeof confirmAllow>[0]['risks'], defaultToNo: boolean, confirm: boolean) => {
    if (!api || !session) return;
    let reason = '';
    if (accept && confirm && !(await confirmAllow({ what, risks, defaultToNo, agentName }))) return;
    if (!accept) { const given = await askDenyReason(what, agentName); if (given === undefined) return; reason = given; }
    try { await applyOperation(api, session.id, approval(requestId, accept, reason)); haptic.success(); setError(''); void refresh(); }
    catch (e) { haptic.error(); setError(e instanceof Error ? e.message : String(e)); }
  }, [api, session, agentName, refresh]);

  const header = <>
    {requests.map(view => <RequestCard key={view.requestId} view={view} agentName={agentName} respond={respond}
      onAllow={() => view.kind === 'approval' ? decide(view.requestId, true, view.what, view.risks, view.defaultToNo, view.confirm) : Promise.resolve()}
      onDeny={() => view.kind === 'approval' ? decide(view.requestId, false, view.what, view.risks, view.defaultToNo, view.confirm) : Promise.resolve()}
      onAnswer={async answers => {
        if (!api || !session) return;
        try { await applyOperation(api, session.id, answer(view.requestId, answers)); haptic.success(); setError(''); void refresh(); }
        catch (e) { haptic.error(); setError(e instanceof Error ? e.message : String(e)); }
      }}/>)}
    {session && !session.closed && busy && status.tone === 'busy' && <View style={styles.status} testID="live-status">
      <View style={[styles.pulse, { backgroundColor: theme.running }]}/>
      <Text style={{ color: theme.muted, fontSize: 14 }}>{status.text}</Text>
    </View>}
    {queued.length > 0 && <View style={[styles.queued, { backgroundColor: theme.surface, borderColor: theme.lineSubtle }]} testID="queued">
      <Text style={[styles.queuedTitle, { color: theme.muted }]}>QUEUED · GOES AFTER THIS TURN</Text>
      {queued.map(entry => <View key={entry.operationId} style={styles.queuedRow}>
        <Icon name="clock" size={13} color={theme.faint}/>
        <Text style={{ flex: 1, color: theme.text, fontSize: 15 }} numberOfLines={2}>{entry.text}</Text>
        {busy && respond && <Button label="Send now" compact busy={delivering === entry.operationId} disabled={!!delivering} onPress={() => void sendNow(entry.operationId)}/>}
      </View>)}
    </View>}
    {error ? <Pressable onPress={() => setError('')} style={[styles.error, { backgroundColor: theme.dangerBg, borderColor: theme.dangerLine }]} accessibilityRole="alert">
      <Icon name="exclamationmark.circle.fill" size={15} color={theme.danger}/><Text style={{ flex: 1, color: theme.text, fontSize: 14 }}>{error}</Text>
    </Pressable> : null}
  </>;

  return <KeyboardAvoidingView behavior="padding" keyboardVerticalOffset={headerHeight} style={{ flex: 1, backgroundColor: theme.bg }}>
    <Stack.Screen options={{ headerTitle: () => <Pressable style={styles.title} accessibilityRole="button"
      accessibilityLabel={[conversationName(session, 'Conversation'), workspace, connection.label].filter(Boolean).join(', ')}
      accessibilityHint="Shows conversation details"
      onPress={() => Alert.alert(conversationName(session, 'Conversation'), [workspace, connection.label, sessionStatus(session)].filter(Boolean).join(' · '))}>
      <Text style={[styles.titleText, { color: theme.textStrong }]} numberOfLines={1}>{conversationName(session, 'Conversation')}</Text>
      <View style={styles.subtitle}>
        <LiveDot tone={connection.tone} label={connection.label}/>
        <Text style={[styles.subtitleText, { color: theme.muted }]} numberOfLines={1}>{[workspace, sessionStatus(session)].filter(Boolean).join(' · ')}</Text>
      </View>
    </Pressable> }}/>
    {connection.banner && <ConnectionBanner label={connection.label} detail={connection.detail} onRetry={() => { void refresh(); reader?.retry(); }}/>}
    {transcript?.error ? <ConnectionBanner label="This conversation could not be loaded" detail={transcript.error} onRetry={() => reader?.retry()}/> : null}
    <FlatList
      testID="transcript"
      inverted
      data={reversed}
      keyExtractor={row => row.id}
      renderItem={({ item }) => <TranscriptRow row={item} agentName={agentName}/>}
      ListHeaderComponent={header}
      ListFooterComponent={transcript?.loading && !rows.length ? <Text style={[styles.loading, { color: theme.muted }]}>Loading conversation…</Text>
        : transcript?.partial ? <Text style={[styles.loading, { color: theme.faint }]}>Loading earlier messages…</Text> : undefined}
      keyboardDismissMode="interactive"
      keyboardShouldPersistTaps="handled"
      contentContainerStyle={{ paddingVertical: space.s }}
      maintainVisibleContentPosition={{ minIndexForVisible: 0 }}
    />
    {api && <Composer key={session?.id} session={session} busy={busy} grants={device?.grants || []} api={api} onSent={() => void refresh()} onError={setError}/>}
  </KeyboardAvoidingView>;
}

const styles = StyleSheet.create({
  title: { alignItems: 'center', maxWidth: 260 },
  titleText: { fontSize: 16, fontWeight: '600' },
  subtitle: { flexDirection: 'row', alignItems: 'center', gap: 6, marginTop: 1 },
  subtitleText: { fontSize: 12, flexShrink: 1 },
  status: { flexDirection: 'row', alignItems: 'center', gap: space.s, paddingHorizontal: space.l + 2, paddingVertical: space.s },
  pulse: { width: 8, height: 8, borderRadius: 4 },
  error: { flexDirection: 'row', alignItems: 'center', gap: space.s, marginHorizontal: space.l, marginVertical: space.s, padding: space.m, borderRadius: radius.m, borderWidth: StyleSheet.hairlineWidth },
  loading: { textAlign: 'center', padding: space.l, fontSize: 14 },
  queued: { marginHorizontal: space.l, marginVertical: space.s, padding: space.m, borderRadius: radius.m, borderWidth: StyleSheet.hairlineWidth, gap: space.s },
  queuedTitle: { fontSize: 11, fontWeight: '700', letterSpacing: 0.6 },
  queuedRow: { flexDirection: 'row', alignItems: 'center', gap: space.s },
});
