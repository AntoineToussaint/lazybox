import { useCallback, useEffect, useMemo, useState } from 'react';
import { ActivityIndicator, Alert, RefreshControl, SectionList, StyleSheet, Text, View } from 'react-native';
import { Stack, useRouter } from 'expo-router';
import { attentionTitle, chipAnswer, seenMark } from '@lazybox/surfaces/models';
import { inboxView, type InboxRow as Row } from '../src/models/inbox';
import { connectionView } from '../src/models/connection';
import { useRunner } from '../src/runner/RunnerProvider';
import { markRead, useSeen } from '../src/runner/seen';
import { actionSheet, answer, applyOperation, approval, askDenyReason, confirmAllow, useDeferred } from '../src/runner/actions';
import { InboxRow, type RowHandlers } from '../src/ui/InboxRow';
import { Button, ConnectionBanner, Icon, IconButton, haptic, stateStyle } from '../src/ui/kit';
import { space, useTheme } from '../src/ui/theme';

const agentNameOf = (row: Row) => row.attention.session.provider === 'codex' ? 'Codex' : 'Claude';

export default function InboxScreen() {
  const runner = useRunner();
  const router = useRouter();
  const theme = useTheme();
  const { phase } = runner;
  // The Inbox's title belongs to the Inbox: the screens before it stand alone.
  const bare = <Stack.Screen options={{ headerShown: false }}/>;
  if (phase.kind === 'loading' || phase.kind === 'signing-in') return <Center>{bare}<ActivityIndicator color={theme.muted}/><Text style={[styles.hint, { color: theme.muted }]}>Opening Lazybox…</Text></Center>;
  if (phase.kind === 'unpaired' || phase.kind === 'revoked') return <>{bare}<Welcome revoked={phase.kind === 'revoked'} onPair={() => router.push('/pair')}/></>;
  if (phase.kind === 'offline') return <Center>
    {bare}
    <Icon name="wifi.exclamationmark" size={40} color={theme.warn}/>
    <Text style={[styles.title, { color: theme.textStrong }]}>Can’t reach your computer</Text>
    <Text style={[styles.body, { color: theme.secondary }]}>{phase.message}</Text>
    <Text style={[styles.hint, { color: theme.muted }]}>Lazybox keeps working there. Check that the computer is awake and that this phone is on the same tailnet.</Text>
    <Button label="Try again" tone="primary" onPress={runner.retry} style={styles.wide}/>
    <Button label="Forget this pairing" tone="plain" onPress={() => void runner.forget()}/>
  </Center>;
  return <Inbox/>;
}

function Inbox() {
  const { api, info, collections, device, error, refresh } = useRunner();
  const router = useRouter();
  const theme = useTheme();
  const seen = useSeen();
  const deferred = useDeferred();
  const [now, setNow] = useState(Date.now);
  const [showAllDone, setShowAllDone] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 30_000); return () => clearInterval(timer); }, []);
  const respond = !!device?.grants.includes('respond');
  const view = useMemo(() => inboxView({ sessions: info?.sessions || [], collections, profiles: info?.profiles || [], seen, respond, now: Date.now(), showAllDone }),
    // `now` re-renders the ages every 30 s.
    [info, collections, seen, respond, showAllDone, now]);
  const connection = connectionView({ hasInfo: !!info, error });
  // An answer sent from a row stays "sent" until the Runner no longer lists its request.
  useEffect(() => { if (info) deferred.settle(new Set(info.sessions.flatMap(session => session.pending.map(request => request.requestId)))); }, [info, deferred.settle]);

  const stop = useCallback((row: Row) => {
    if (!api) return;
    haptic.warning();
    Alert.alert('Stop this turn?', `${agentNameOf(row)} stops what it is doing in “${row.title}”. The conversation stays, and you can reply to continue.`, [
      { text: 'Cancel', style: 'cancel' },
      { text: 'Stop', style: 'destructive', onPress: () => void applyOperation(api, row.sessionId, { case: 'interrupt', value: {} }).then(() => { haptic.success(); void refresh(); }, () => haptic.error()) },
    ]);
  }, [api, refresh]);

  const handlers = useMemo<RowHandlers>(() => ({
    open: row => router.push({ pathname: '/conversation/[id]', params: { id: row.sessionId } }),
    more: row => {
      const plan = row.plan;
      const choices = [{ label: 'Open conversation', run: () => handlers.open(row) }];
      if (plan.kind === 'approval' && plan.allow && !deferred.states[plan.request.requestId]) choices.push({ label: plan.confirm ? 'Allow once…' : 'Allow once', run: () => handlers.allow(row) });
      if (plan.kind === 'approval' && !deferred.states[plan.request.requestId]) choices.push({ label: 'Deny…', run: () => handlers.deny(row) });
      if (respond && api) choices.push({ label: row.pinned ? 'Unpin' : 'Pin', run: () => void api.pinConversation({ sessionId: row.sessionId, pinned: !row.pinned }, { timeoutMs: 15000 }).then(() => { haptic.success(); void refresh(); }, () => haptic.error()) });
      if (row.unread) choices.push({ label: 'Mark as read', run: () => markRead(row.key, seenMark(row.attention)) });
      if (row.stoppable && api) choices.push({ label: 'Stop this turn…', destructive: true, run: () => stop(row) } as typeof choices[number]);
      actionSheet(row.title, [row.meta, row.reason].filter(Boolean).join(' · '), choices);
    },
    allow: async row => {
      const plan = row.plan;
      if (plan.kind !== 'approval' || !api) return;
      if (plan.confirm && !(await confirmAllow({ what: plan.what, risks: plan.risks, defaultToNo: plan.confirm && !plan.risks.length, agentName: agentNameOf(row) }))) return;
      deferred.schedule(plan.request.requestId, `Allowed · ${row.title}`, () => applyOperation(api, row.sessionId, approval(plan.request.requestId, true)));
    },
    deny: async row => {
      const plan = row.plan;
      if (plan.kind !== 'approval' || !api) return;
      const reason = await askDenyReason(plan.what, agentNameOf(row));
      if (reason === undefined) return;
      deferred.schedule(plan.request.requestId, `Denied · ${row.title}`, () => applyOperation(api, row.sessionId, approval(plan.request.requestId, false, reason)), 0);
    },
    answer: (row, labels) => {
      const plan = row.plan;
      if (plan.kind !== 'question' || !api) return;
      deferred.schedule(plan.request.requestId, `Answered “${labels.join(', ')}”`, () => applyOperation(api, row.sessionId, answer(plan.request.requestId, chipAnswer(plan.question, labels))));
    },
    undo: row => { if ('request' in row.plan) deferred.cancel(row.plan.request.requestId); },
  }), [api, router, respond, deferred.schedule, deferred.cancel, deferred.states, refresh, stop]);
  const onRefresh = useCallback(async () => { setRefreshing(true); try { await refresh(); } finally { setRefreshing(false); } }, [refresh]);
  const sections = view.sections.map(section => ({ ...section, data: section.rows }));
  return <>
    <Stack.Screen options={{ headerShown: true, headerRight: () => <IconButton icon="gearshape" label="Settings" onPress={() => router.push('/settings')} testID="settings"/> }}/>
    <SectionList
      testID="inbox"
      contentInsetAdjustmentBehavior="automatic"
      sections={sections}
      keyExtractor={row => row.key}
      stickySectionHeadersEnabled={false}
      refreshControl={<RefreshControl refreshing={refreshing} onRefresh={onRefresh} tintColor={theme.muted}/>}
      ListHeaderComponent={<>
        {connection.banner && <ConnectionBanner label={connection.label} detail={connection.detail} onRetry={() => void refresh()}/>}
        {view.summary ? <Text style={[styles.summary, { color: view.needsYou ? theme.warn : theme.muted }]} testID="inbox-summary">{view.summary}</Text> : null}
      </>}
      ListEmptyComponent={!info ? <View style={styles.empty}><ActivityIndicator color={theme.muted}/><Text style={[styles.hint, { color: theme.muted }]}>Loading conversations…</Text></View>
        : <View style={styles.empty}><Icon name="tray" size={34} color={theme.faint}/><Text style={[styles.emptyTitle, { color: theme.text }]}>No conversations yet</Text><Text style={[styles.hint, { color: theme.muted }]}>Agents that need you will show up here.</Text></View>}
      renderSectionHeader={({ section }) => {
        const look = stateStyle(section.state, theme);
        return <View style={styles.sectionHead} accessibilityRole="header">
          <Icon name={look.icon} size={12} color={look.color}/>
          <Text style={[styles.sectionTitle, { color: theme.muted }]}>{(section.state ? attentionTitle[section.state] : section.title).toUpperCase()}</Text>
          <Text style={[styles.sectionCount, { color: theme.faint }]}>{section.count}</Text>
        </View>;
      }}
      renderSectionFooter={({ section }) => section.hidden ? <Button label={`Show ${section.hidden} more`} tone="plain" onPress={() => setShowAllDone(true)}/> : null}
      renderItem={({ item }) => <InboxRow row={item} handlers={handlers} deferred={'request' in item.plan ? deferred.states[item.plan.request.requestId] : undefined}/>}
      contentContainerStyle={{ paddingBottom: space.xl }}
    />
  </>;
}

function Welcome({ revoked, onPair }: { revoked: boolean; onPair(): void }) {
  const theme = useTheme();
  return <Center>
    <View style={[styles.mark, { backgroundColor: theme.accentBg }]}><Text style={{ fontSize: 34, color: theme.accent }}>✳︎</Text></View>
    <Text style={[styles.title, { color: theme.textStrong }]}>{revoked ? 'This phone was signed out' : 'Your agents, in your pocket'}</Text>
    <Text style={[styles.body, { color: theme.secondary }]}>{revoked ? 'It was revoked on your computer, so it can no longer see or answer your agents.' : 'See what needs you, approve, answer and reply to the agents running on your computer.'}</Text>
    <View style={styles.steps}>
      {['On your computer, open Lazybox → Settings → Devices.', 'Choose Pair a phone.', 'Scan the code with this app, or paste its link.'].map((step, i) =>
        <View key={step} style={styles.step}><Text style={[styles.stepNumber, { color: theme.accent, borderColor: theme.accentLine }]}>{i + 1}</Text><Text style={[styles.stepText, { color: theme.text }]}>{step}</Text></View>)}
    </View>
    <Button label="Pair with your computer" tone="primary" icon="qrcode.viewfinder" onPress={onPair} style={styles.wide} testID="pair"/>
  </Center>;
}

function Center({ children }: { children: React.ReactNode }) {
  return <View style={styles.center}>{children}</View>;
}

const styles = StyleSheet.create({
  center: { flex: 1, alignItems: 'center', justifyContent: 'center', padding: space.xl, gap: space.m },
  mark: { width: 72, height: 72, borderRadius: 20, alignItems: 'center', justifyContent: 'center', marginBottom: space.s },
  title: { fontSize: 24, fontWeight: '700', textAlign: 'center' },
  body: { fontSize: 16, lineHeight: 22, textAlign: 'center' },
  hint: { fontSize: 14, lineHeight: 20, textAlign: 'center' },
  wide: { alignSelf: 'stretch', marginTop: space.s },
  steps: { alignSelf: 'stretch', gap: space.m, marginVertical: space.m },
  step: { flexDirection: 'row', alignItems: 'center', gap: space.m },
  stepNumber: { width: 28, height: 28, borderRadius: 14, borderWidth: 1, textAlign: 'center', lineHeight: 26, fontWeight: '700', overflow: 'hidden' },
  stepText: { flex: 1, fontSize: 15, lineHeight: 20 },
  summary: { fontSize: 15, fontWeight: '600', marginHorizontal: space.l, marginTop: space.xs, marginBottom: space.s },
  sectionHead: { flexDirection: 'row', alignItems: 'center', gap: 6, paddingHorizontal: space.l + space.xs, paddingTop: space.l, paddingBottom: space.s },
  sectionTitle: { fontSize: 12, fontWeight: '700', letterSpacing: 0.6 },
  sectionCount: { fontSize: 12, fontWeight: '600' },
  empty: { alignItems: 'center', gap: space.s, paddingTop: 80, paddingHorizontal: space.xl },
  emptyTitle: { fontSize: 17, fontWeight: '600' },
});
