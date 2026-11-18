import { memo, useMemo } from 'react';
import { ScrollView, StyleSheet, Text, View } from 'react-native';
import { markdownBlocks, type Span } from '../models/markdown';
import { mono, radius, space, useTheme, type Theme } from './theme';

function Spans({ spans, theme }: { spans: Span[]; theme: Theme }) {
  return <>{spans.map((span, i) => <Text key={i} style={[
    span.code && { fontFamily: mono, fontSize: 14, color: theme.codeText, backgroundColor: theme.codeBg },
    span.bold && { fontWeight: '700', color: theme.textStrong },
    span.italic && { fontStyle: 'italic' },
  ]}>{span.code ? ` ${span.text} ` : span.text}</Text>)}</>;
}

/** An agent's message in native text: selectable, sized for reading. */
export const Markdown = memo(function Markdown({ text, color }: { text: string; color?: string }) {
  const theme = useTheme();
  const blocks = useMemo(() => markdownBlocks(text), [text]);
  const base = { color: color || theme.text };
  return <View style={styles.root}>
    {blocks.map((block, i) => {
      switch (block.kind) {
        case 'heading': return <Text key={i} selectable style={[styles.body, base, styles.heading, { fontSize: block.level <= 2 ? 19 : 17, color: theme.textStrong }]}><Spans spans={block.spans} theme={theme}/></Text>;
        case 'item': return <View key={i} style={[styles.item, { paddingLeft: block.depth * space.l }]}>
          <Text style={[styles.body, { color: theme.muted, minWidth: 18 }]}>{block.marker}</Text>
          <Text selectable style={[styles.body, base, { flex: 1 }]}><Spans spans={block.spans} theme={theme}/></Text>
        </View>;
        case 'quote': return <View key={i} style={[styles.quote, { borderLeftColor: theme.line }]}><Text selectable style={[styles.body, { color: theme.secondary }]}><Spans spans={block.spans} theme={theme}/></Text></View>;
        case 'code': return <ScrollView key={i} horizontal style={[styles.code, { backgroundColor: theme.codeBg, borderColor: theme.lineSubtle }]} contentContainerStyle={{ padding: space.m }}>
          <Text selectable style={{ fontFamily: mono, fontSize: 13, lineHeight: 19, color: theme.codeText }}>{block.text}</Text>
        </ScrollView>;
        case 'rule': return <View key={i} style={{ height: StyleSheet.hairlineWidth, backgroundColor: theme.line, marginVertical: space.s }}/>;
        default: return <Text key={i} selectable style={[styles.body, base]}><Spans spans={block.spans} theme={theme}/></Text>;
      }
    })}
  </View>;
});

const styles = StyleSheet.create({
  root: { gap: space.s },
  body: { fontSize: 16, lineHeight: 23 },
  heading: { fontWeight: '700', marginTop: space.xs },
  item: { flexDirection: 'row', gap: space.xs },
  quote: { borderLeftWidth: 3, paddingLeft: space.m },
  code: { borderRadius: radius.s, borderWidth: StyleSheet.hairlineWidth },
});
