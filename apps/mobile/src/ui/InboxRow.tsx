import { memo, useState } from 'react';
import { Pressable, StyleSheet, Text, View } from 'react-native';
import type { InboxRow as Row } from '../models/inbox';
import type { Deferred } from '../runner/actions';
import { Button, Icon, haptic, stateStyle } from './kit';
import { mono, radius, space, useTheme } from './theme';

export type RowHandlers = {
  open(row: Row): void;
  more(row: Row): void;
  allow(row: Row): void;
  deny(row: Row): void;
  answer(row: Row, labels: string[]): void;
  undo(row: Row): void;
};

/** One conversation in the Inbox: what it needs, and the answer right on the row when it is safe there. */
export const InboxRow = memo(function InboxRow({ row, deferred, handlers }: { row: Row; deferred?: Deferred; handlers: RowHandlers }) {
  const theme = useTheme();
  const look = stateStyle(row.state, theme);
  const plan = row.plan;
  const line = plan.kind === 'question' ? plan.question.text : plan.kind === 'approval' ? '' : row.line;
  return <View style={[styles.card, { backgroundColor: theme.surface, borderColor: row.state === 'needs-input' ? theme.warnLine : theme.lineSubtle }]} testID={`row-${row.sessionId}`}>
    <Pressable accessibilityRole="button" accessibilityLabel={`${row.title}, ${row.reason}`} accessibilityHint="Opens the conversation"
      onPress={() => { haptic.tap(); handlers.open(row); }} onLongPress={() => handlers.more(row)} delayLongPress={350}
      style={({ pressed }) => [styles.head, pressed && { backgroundColor: theme.pressed }]}>
      <View style={styles.icon}><Icon name={look.icon} size={18} color={look.color}/></View>
      <View style={styles.main}>
        <View style={styles.titleLine}>
          {row.unread && <View style={[styles.unread, { backgroundColor: theme.accent }]} accessibilityLabel="unread"/>}
          <Text style={[styles.title, { color: theme.textStrong }]} numberOfLines={1}>{row.title}</Text>
          {row.pinned && <Icon name="pin.fill" size={11} color={theme.faint}/>}
          <Text style={[styles.age, { color: theme.faint }]}>{row.age}</Text>
        </View>
        {row.meta ? <Text style={[styles.meta, { color: theme.muted }]} numberOfLines={1}>{row.meta}</Text> : null}
        <Text style={[styles.line, { color: theme.secondary }]} numberOfLines={2}>
          <Text style={{ color: look.color, fontWeight: '600' }}>{row.reason}</Text>{line ? ` · ${line}` : ''}
        </Text>
      </View>
    </Pressable>
    {plan.kind !== 'none' && <View style={styles.act}>
      {deferred ? <Progress state={deferred} onUndo={() => handlers.undo(row)}/>
        : plan.kind === 'approval' ? <>
          <View style={[styles.what, { backgroundColor: theme.codeBg, borderColor: theme.lineSubtle }]}>
            <Text style={[styles.code, { color: theme.codeText }]} numberOfLines={3} selectable>{plan.what}</Text>
          </View>
          {plan.risks.map(risk => <View key={risk.kind} style={styles.risk}><Icon name="exclamationmark.triangle.fill" size={12} color={theme.warn}/><Text style={[styles.riskText, { color: theme.warn }]}>{risk.text}</Text></View>)}
          {plan.allow ? <View style={styles.buttons}>
            <Button label="Deny" onPress={() => handlers.deny(row)} style={styles.flex} testID={`deny-${row.sessionId}`}/>
            <Button label={plan.confirm ? 'Allow…' : 'Allow'} tone="primary" icon="checkmark" onPress={() => handlers.allow(row)} style={styles.flex} testID={`allow-${row.sessionId}`}/>
          </View> : <View style={styles.buttons}>
            <Text style={[styles.reason, { color: theme.muted }]}>{plan.openReason}</Text>
            <Button label="Review" onPress={() => handlers.open(row)} compact/>
          </View>}
        </>
        : plan.kind === 'question' ? <Chips options={plan.question.options.map(option => option.label)} mode={plan.mode} onAnswer={labels => handlers.answer(row, labels)} onOpen={() => handlers.open(row)} testID={row.sessionId}/>
        : <View style={styles.risk}><Icon name="desktopcomputer" size={13} color={theme.muted}/><Text style={[styles.reason, { color: theme.muted }]}>{plan.text}</Text></View>}
    </View>}
  </View>;
});

function Chips({ options, mode, onAnswer, onOpen, testID }: { options: string[]; mode: 'chips' | 'multi' | 'open'; onAnswer(labels: string[]): void; onOpen(): void; testID: string }) {
  const theme = useTheme();
  const [picked, setPicked] = useState<string[]>([]);
  if (mode === 'open') return <Button label="Answer" icon="text.bubble" onPress={onOpen} compact/>;
  return <View style={styles.chips}>
    {options.map(label => {
      const on = picked.includes(label);
      return <Pressable key={label} accessibilityRole="button" accessibilityState={{ selected: on }} testID={`chip-${testID}-${label}`}
        onPress={() => { haptic.select(); if (mode === 'chips') onAnswer([label]); else setPicked(all => on ? all.filter(value => value !== label) : [...all, label]); }}
        style={({ pressed }) => [styles.chip, { backgroundColor: on ? theme.accentBg : theme.raised, borderColor: on ? theme.accent : theme.line, opacity: pressed ? 0.7 : 1 }]}>
        <Text style={{ color: on ? theme.accent : theme.text, fontSize: 15, fontWeight: '600' }}>{label}</Text>
      </Pressable>;
    })}
    {mode === 'multi' && <Button label="Send" tone="primary" disabled={!picked.length} onPress={() => onAnswer(picked)} compact/>}
  </View>;
}

function Progress({ state, onUndo }: { state: Deferred; onUndo(): void }) {
  const theme = useTheme();
  return <View style={styles.progress} accessibilityRole="text" accessibilityLiveRegion="polite" testID="row-progress">
    <Icon name={state.phase === 'sent' ? 'checkmark.circle.fill' : 'clock'} size={15} color={state.phase === 'sent' ? theme.ok : theme.muted}/>
    <Text style={{ flex: 1, color: theme.secondary, fontSize: 15 }}>{state.phase === 'sent' ? `${state.label} · waiting for the agent` : state.phase === 'sending' ? `${state.label}…` : state.label}</Text>
    {state.phase === 'waiting' && <Button label="Undo" tone="plain" onPress={onUndo} compact testID="undo"/>}
  </View>;
}

const styles = StyleSheet.create({
  card: { marginHorizontal: space.l, marginBottom: space.s, borderRadius: radius.l, borderWidth: StyleSheet.hairlineWidth, overflow: 'hidden' },
  head: { flexDirection: 'row', gap: space.m, padding: space.m, paddingRight: space.l, minHeight: 64 },
  icon: { width: 24, alignItems: 'center', paddingTop: 2 },
  main: { flex: 1, gap: 2 },
  titleLine: { flexDirection: 'row', alignItems: 'center', gap: 6 },
  unread: { width: 8, height: 8, borderRadius: 4 },
  title: { flex: 1, fontSize: 16, fontWeight: '600' },
  age: { fontSize: 13, fontVariant: ['tabular-nums'] },
  meta: { fontSize: 13 },
  line: { fontSize: 14, lineHeight: 19, marginTop: 2 },
  act: { paddingHorizontal: space.m, paddingBottom: space.m, paddingLeft: space.m + 24 + space.m, gap: space.s },
  what: { borderRadius: radius.s, borderWidth: StyleSheet.hairlineWidth, paddingHorizontal: space.m, paddingVertical: space.s },
  code: { fontFamily: mono, fontSize: 13, lineHeight: 18 },
  risk: { flexDirection: 'row', alignItems: 'center', gap: 6 },
  riskText: { fontSize: 13, fontWeight: '500' },
  buttons: { flexDirection: 'row', alignItems: 'center', gap: space.s },
  flex: { flex: 1 },
  reason: { flex: 1, fontSize: 13 },
  chips: { flexDirection: 'row', flexWrap: 'wrap', gap: space.s },
  chip: { minHeight: 40, paddingHorizontal: space.l, borderRadius: radius.pill, borderWidth: 1, justifyContent: 'center' },
  progress: { flexDirection: 'row', alignItems: 'center', gap: space.s, minHeight: 40 },
});
