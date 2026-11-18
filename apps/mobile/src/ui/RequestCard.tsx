import { useState } from 'react';
import { Pressable, StyleSheet, Text, TextInput, View } from 'react-native';
import { answerPayload, type AnswerDraft, type RequestView } from '../models/conversation';
import { Button, Icon, haptic } from './kit';
import { mono, radius, space, useTheme } from './theme';

/** The pending request, in the conversation: Allow and Deny, or the answer form. */
export function RequestCard({ view, agentName, respond, onAllow, onDeny, onAnswer }: {
  view: RequestView; agentName: string; respond: boolean;
  onAllow(): Promise<void>; onDeny(): Promise<void>; onAnswer(answers: Record<string, { values: string[] }>): Promise<void>;
}) {
  const theme = useTheme();
  const [busy, setBusy] = useState<'allow' | 'deny' | 'answer'>();
  const [draft, setDraft] = useState<AnswerDraft>({});
  const run = (which: 'allow' | 'deny' | 'answer', action: () => Promise<void>) => { setBusy(which); void action().finally(() => setBusy(undefined)); };
  const accent = view.kind === 'approval' && view.confirm ? theme.warn : theme.accent;
  return <View style={[styles.card, { backgroundColor: theme.surface, borderColor: view.kind === 'elsewhere' ? theme.lineSubtle : theme.warnLine }]} testID={`request-${view.requestId}`}>
    <View style={styles.head}>
      <Icon name={view.kind === 'question' ? 'questionmark.bubble.fill' : view.kind === 'approval' ? 'hand.raised.fill' : 'desktopcomputer'} size={16} color={accent}/>
      <Text style={[styles.kind, { color: accent }]}>{view.kind === 'question' ? `${agentName} asks` : view.kind === 'approval' ? 'Approval required' : 'On your computer'}</Text>
    </View>
    {view.kind === 'elsewhere' && <Text style={[styles.body, { color: theme.secondary }]}>{view.text}</Text>}
    {view.kind === 'approval' && <>
      <Text style={[styles.title, { color: theme.textStrong }]}>{view.title}</Text>
      <View style={[styles.code, { backgroundColor: theme.codeBg, borderColor: theme.lineSubtle }]}><Text selectable style={[styles.codeText, { color: theme.codeText }]}>{view.what}</Text></View>
      {view.description ? <Text style={[styles.body, { color: theme.secondary }]}>{view.description}</Text> : null}
      {view.patch ? <Text style={[styles.meta, { color: theme.muted }]}>+{view.added} −{view.removed} lines</Text> : null}
      {view.risks.map(risk => <View key={risk.kind} style={styles.risk}><Icon name="exclamationmark.triangle.fill" size={13} color={theme.warn}/><Text style={[styles.riskText, { color: theme.warn }]}>{risk.text}</Text></View>)}
      {view.defaultToNo && !view.risks.length && <View style={styles.risk}><Icon name="exclamationmark.triangle.fill" size={13} color={theme.warn}/><Text style={[styles.riskText, { color: theme.warn }]}>{agentName} suggests saying no unless you are sure</Text></View>}
      {!respond ? <Text style={[styles.meta, { color: theme.muted }]}>This phone can only read. Answer on your computer.</Text>
        : view.allow ? <View style={styles.buttons}>
          <Button label="Deny" busy={busy === 'deny'} disabled={!!busy} onPress={() => run('deny', onDeny)} style={styles.flex} testID="deny"/>
          <Button label={view.confirm ? 'Allow…' : 'Allow once'} tone="primary" icon="checkmark" busy={busy === 'allow'} disabled={!!busy} onPress={() => run('allow', onAllow)} style={styles.flex} testID="allow"/>
        </View>
        : <Text style={[styles.meta, { color: theme.muted }]}>{view.reason}</Text>}
    </>}
    {view.kind === 'question' && <>
      {view.questions.map(question => {
        const mine = draft[question.key] || { picked: [], typed: '' };
        const update = (next: Partial<typeof mine>) => setDraft(all => ({ ...all, [question.key]: { ...mine, ...next } }));
        return <View key={question.key} style={styles.question}>
          {question.header && question.header !== question.text ? <Text style={[styles.meta, { color: theme.muted }]}>{question.header.toUpperCase()}</Text> : null}
          <Text style={[styles.title, { color: theme.textStrong }]}>{question.text}</Text>
          {question.options.map(option => {
            const on = mine.picked.includes(option.label);
            return <Pressable key={option.label} accessibilityRole={question.multi ? 'checkbox' : 'radio'} accessibilityState={{ checked: on }} disabled={!respond}
              onPress={() => { haptic.select(); update({ picked: question.multi ? (on ? mine.picked.filter(v => v !== option.label) : [...mine.picked, option.label]) : [option.label], typed: question.multi ? mine.typed : '' }); }}
              style={({ pressed }) => [styles.option, { borderColor: on ? theme.accent : theme.line, backgroundColor: on ? theme.accentBg : theme.raised, opacity: pressed ? 0.7 : 1 }]} testID={`option-${option.label}`}>
              <Icon name={on ? (question.multi ? 'checkmark.square.fill' : 'largecircle.fill.circle') : (question.multi ? 'square' : 'circle')} size={18} color={on ? theme.accent : theme.faint}/>
              <View style={styles.flex}>
                <Text style={{ color: theme.text, fontSize: 16, fontWeight: '600' }}>{option.label}</Text>
                {option.description ? <Text style={{ color: theme.muted, fontSize: 13, marginTop: 2 }}>{option.description}</Text> : null}
              </View>
            </Pressable>;
          })}
          {(question.other || !question.options.length) && respond && <TextInput value={mine.typed} onChangeText={typed => update({ typed, picked: question.multi ? mine.picked : [] })}
            placeholder={question.options.length ? 'Or type your own answer' : 'Your answer'} placeholderTextColor={theme.faint} secureTextEntry={question.secret} multiline={!question.secret}
            style={[styles.input, { color: theme.text, backgroundColor: theme.raised, borderColor: theme.line }]} testID="answer-text"/>}
        </View>;
      })}
      {respond ? <Button label="Send answer" tone="primary" icon="paperplane.fill" busy={busy === 'answer'} disabled={!answerPayload(view.questions, draft) || !!busy}
        onPress={() => { const answers = answerPayload(view.questions, draft); if (answers) run('answer', () => onAnswer(answers)); }} testID="send-answer"/>
        : <Text style={[styles.meta, { color: theme.muted }]}>This phone can only read. Answer on your computer.</Text>}
    </>}
  </View>;
}

const styles = StyleSheet.create({
  card: { marginHorizontal: space.l, marginVertical: space.s, padding: space.l, borderRadius: radius.l, borderWidth: 1, gap: space.m },
  head: { flexDirection: 'row', alignItems: 'center', gap: space.s },
  kind: { fontSize: 13, fontWeight: '700', letterSpacing: 0.3 },
  title: { fontSize: 17, fontWeight: '600', lineHeight: 23 },
  body: { fontSize: 15, lineHeight: 21 },
  meta: { fontSize: 13, lineHeight: 18 },
  code: { borderRadius: radius.s, borderWidth: StyleSheet.hairlineWidth, padding: space.m },
  codeText: { fontFamily: mono, fontSize: 14, lineHeight: 20 },
  risk: { flexDirection: 'row', alignItems: 'center', gap: 6 },
  riskText: { fontSize: 14, fontWeight: '500', flex: 1 },
  buttons: { flexDirection: 'row', gap: space.s, marginTop: space.xs },
  flex: { flex: 1 },
  question: { gap: space.s },
  option: { flexDirection: 'row', alignItems: 'center', gap: space.m, minHeight: 48, paddingHorizontal: space.m, paddingVertical: space.s, borderRadius: radius.m, borderWidth: 1 },
  input: { minHeight: 44, borderWidth: StyleSheet.hairlineWidth, borderRadius: radius.m, paddingHorizontal: space.m, paddingVertical: 10, fontSize: 16 },
});
