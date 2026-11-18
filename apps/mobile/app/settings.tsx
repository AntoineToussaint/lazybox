import { Alert, ScrollView, StyleSheet, Text, View } from 'react-native';
import { useRouter } from 'expo-router';
import { useRunner } from '../src/runner/RunnerProvider';
import { Button, Icon } from '../src/ui/kit';
import { mono, radius, space, useTheme } from '../src/ui/theme';

const grantText: Record<string, string> = { read: 'See conversations', respond: 'Reply, answer and approve', start: 'Start and continue conversations' };

/** This phone: which computer it is paired with, what it may do, and forgetting the pairing. */
export default function SettingsScreen() {
  const theme = useTheme();
  const router = useRouter();
  const { phase, device, forget } = useRunner();
  const gateway = phase.kind === 'ready' || phase.kind === 'offline' || phase.kind === 'signing-in' ? phase.pairing.baseUrl : '';
  const row = (label: string, value: string, monospace = false) => <View style={[styles.row, { borderBottomColor: theme.lineSubtle }]}>
    <Text style={[styles.label, { color: theme.muted }]}>{label}</Text>
    <Text selectable style={[styles.value, { color: theme.text }, monospace && { fontFamily: mono, fontSize: 14 }]}>{value}</Text>
  </View>;
  return <ScrollView contentContainerStyle={styles.page}>
    {device ? <>
      <View style={[styles.card, { backgroundColor: theme.surface, borderColor: theme.lineSubtle }]}>
        {row('Paired as', device.name)}
        {row('Computer', gateway.replace(/^https?:\/\//, ''))}
        {row('Key', device.fingerprint, true)}
      </View>
      <Text style={[styles.section, { color: theme.muted }]}>THIS PHONE CAN</Text>
      <View style={[styles.card, { backgroundColor: theme.surface, borderColor: theme.lineSubtle }]}>
        {device.grants.map(grant => <View key={grant} style={[styles.grant, { borderBottomColor: theme.lineSubtle }]}><Icon name="checkmark" size={14} color={theme.ok} weight="bold"/><Text style={{ color: theme.text, fontSize: 16 }}>{grantText[grant] || grant}</Text></View>)}
      </View>
      <Text style={[styles.hint, { color: theme.muted }]}>The owner of the computer chose these when pairing. To change them, revoke this phone in Settings → Devices on your computer and pair again.</Text>
      <Button label="Forget this pairing" tone="danger" onPress={() => Alert.alert('Forget this pairing?', 'This phone forgets its key. It stays listed on your computer until you revoke it there.', [
        { text: 'Cancel', style: 'cancel' },
        { text: 'Forget', style: 'destructive', onPress: () => void forget().then(() => router.dismissTo('/')) },
      ])} style={{ marginTop: space.l }}/>
    </> : <Text style={[styles.hint, { color: theme.muted }]}>This phone is not paired.</Text>}
  </ScrollView>;
}

const styles = StyleSheet.create({
  page: { padding: space.l, gap: space.s },
  card: { borderRadius: radius.l, borderWidth: StyleSheet.hairlineWidth, overflow: 'hidden' },
  row: { paddingHorizontal: space.l, paddingVertical: space.m, borderBottomWidth: StyleSheet.hairlineWidth, gap: 2 },
  label: { fontSize: 13 },
  value: { fontSize: 16 },
  section: { fontSize: 12, fontWeight: '700', letterSpacing: 0.6, marginTop: space.l, marginLeft: space.xs },
  grant: { flexDirection: 'row', alignItems: 'center', gap: space.m, paddingHorizontal: space.l, minHeight: 48, borderBottomWidth: StyleSheet.hairlineWidth },
  hint: { fontSize: 13, lineHeight: 18, marginHorizontal: space.xs, marginTop: space.s },
});
