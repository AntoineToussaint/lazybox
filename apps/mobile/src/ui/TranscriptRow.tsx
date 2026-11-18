import { memo } from 'react';
import { ActivityIndicator, StyleSheet, Text, View } from 'react-native';
import type { SFSymbol } from 'expo-symbols';
import type { TranscriptRow as Row } from '../models/conversation';
import { Markdown } from './Markdown';
import { Icon } from './kit';
import { radius, space, useTheme, type Theme } from './theme';

const noteIcon: Record<Extract<Row, { kind: 'note' }>['icon'], SFSymbol> = { check: 'checkmark.circle', stop: 'stop.circle', info: 'info.circle', chat: 'bubble.left.and.text.bubble.right', alert: 'exclamationmark.triangle', arrow: 'chevron.right' };
const toneColor = (tone: Extract<Row, { kind: 'note' }>['tone'], theme: Theme) => ({ ok: theme.ok, warn: theme.warn, muted: theme.faint, ask: theme.accent, neutral: theme.muted })[tone];

/** One line of the conversation, drawn natively from the shared fold. */
export const TranscriptRow = memo(function TranscriptRow({ row, agentName }: { row: Row; agentName: string }) {
  const theme = useTheme();
  switch (row.kind) {
    case 'you': return <View style={styles.youWrap}>
      <View style={[styles.you, { backgroundColor: theme.accentBg, borderColor: theme.accentLine }]} accessibilityLabel={`You: ${row.text}`}>
        {row.steered && <Text style={[styles.tag, { color: theme.accent }]}>STEERED</Text>}
        <Text selectable style={[styles.youText, { color: theme.textStrong }]}>{row.text}</Text>
      </View>
    </View>;
    case 'agent': return <View style={styles.agent}>
      <View style={styles.eyebrow}><Text style={[styles.eyebrowText, { color: theme.muted }]}>{agentName}</Text>{row.streaming && <ActivityIndicator size="small" color={theme.running} style={{ transform: [{ scale: 0.7 }] }}/>}</View>
      <Markdown text={row.text}/>
    </View>;
    case 'failure': return <View style={[styles.failure, { backgroundColor: theme.dangerBg, borderColor: theme.dangerLine }]}>
      <Icon name="exclamationmark.octagon.fill" size={15} color={theme.danger}/>
      <Text selectable style={{ flex: 1, color: theme.text, fontSize: 15, lineHeight: 21 }}>{row.text}</Text>
    </View>;
    case 'activity': return <View style={styles.note} accessibilityLabel={`${row.text}${row.meta ? `, ${row.meta}` : ''}`}>
      {row.running ? <ActivityIndicator size="small" color={theme.running} style={{ width: 15, transform: [{ scale: 0.7 }] }}/> : <Icon name={row.failed ? 'exclamationmark.triangle' : 'hammer'} size={13} color={row.failed ? theme.warn : theme.faint}/>}
      <Text style={[styles.noteText, { color: theme.secondary }]} numberOfLines={2}>{row.text}{row.meta ? <Text style={{ color: theme.faint }}> · {row.meta}</Text> : null}</Text>
    </View>;
    case 'note': return <View style={styles.note}>
      <Icon name={noteIcon[row.icon]} size={13} color={toneColor(row.tone, theme)}/>
      <Text style={[styles.noteText, { color: row.tone === 'muted' ? theme.muted : theme.secondary }]} numberOfLines={3}>{row.text}</Text>
    </View>;
  }
});

const styles = StyleSheet.create({
  youWrap: { alignItems: 'flex-end', paddingHorizontal: space.l, paddingVertical: space.s, paddingLeft: 56 },
  you: { borderRadius: radius.l, borderBottomRightRadius: 6, paddingHorizontal: 14, paddingVertical: 10, borderWidth: StyleSheet.hairlineWidth, gap: 2 },
  youText: { fontSize: 16, lineHeight: 22 },
  tag: { fontSize: 10, fontWeight: '800', letterSpacing: 0.6 },
  agent: { paddingHorizontal: space.l, paddingVertical: space.s, gap: space.xs },
  eyebrow: { flexDirection: 'row', alignItems: 'center', gap: 4, height: 18 },
  eyebrowText: { fontSize: 12, fontWeight: '700', letterSpacing: 0.3 },
  failure: { flexDirection: 'row', gap: space.s, marginHorizontal: space.l, marginVertical: space.s, padding: space.m, borderRadius: radius.m, borderWidth: StyleSheet.hairlineWidth },
  note: { flexDirection: 'row', alignItems: 'center', gap: space.s, paddingHorizontal: space.l + 2, paddingVertical: 6 },
  noteText: { flex: 1, fontSize: 14, lineHeight: 19 },
});
