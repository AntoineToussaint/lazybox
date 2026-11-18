import { useEffect, useMemo, useRef, useState } from 'react';
import { KeyboardAvoidingView, Platform, ScrollView, StyleSheet, Text, TextInput, View } from 'react-native';
import { useRouter } from 'expo-router';
import * as Linking from 'expo-linking';
import { CameraView, useCameraPermissions } from 'expo-camera';
import * as Clipboard from 'expo-clipboard';
import { parsePairingLink, type DeviceRecord } from '@lazybox/client-core';
import { deepLinkedPairing } from '../src/models/pairing';
import { useRunner } from '../src/runner/RunnerProvider';
import { Button, Icon, haptic } from '../src/ui/kit';
import { mono, radius, space, useTheme } from '../src/ui/theme';

const grantText: Record<string, string> = { read: 'See conversations', respond: 'Reply, answer and approve', start: 'Start conversations' };

/**
 * Pairing (REMOTE.md): the computer shows a one-time code for five minutes.
 * The phone scans it (or the person pastes its link, or a `lazybox://pair?link=`
 * deep link brings it), creates its key in the Keychain, and proves it holds
 * it. Both screens then show the same key fingerprint.
 */
export default function PairScreen() {
  const theme = useTheme();
  const router = useRouter();
  const { pair } = useRunner();
  const url = Linking.useLinkingURL();
  const [link, setLink] = useState(() => deepLinkedPairing(url));
  const [name, setName] = useState('iPhone');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [paired, setPaired] = useState<DeviceRecord>();
  const [permission, requestPermission] = useCameraPermissions();
  const [scanning, setScanning] = useState(false);
  const scanned = useRef(false);
  const parsed = useMemo(() => parsePairingLink(link), [link]);
  useEffect(() => { const linked = deepLinkedPairing(url); if (linked) setLink(linked); }, [url]);

  const paste = async () => {
    const text = (await Clipboard.getStringAsync().catch(() => '')).trim();
    if (!text) { setError('The clipboard is empty. Copy the pairing link on your computer first.'); return; }
    setError(''); setLink(text);
  };
  const scan = async () => {
    if (!permission?.granted) { const next = await requestPermission(); if (!next.granted) { setError('Lazybox needs the camera to scan the code. You can paste its link instead.'); return; } }
    scanned.current = false; setError(''); setScanning(true);
  };
  const submit = async () => {
    if (busy) return;
    setBusy(true); setError('');
    try { setPaired(await pair(link, name.trim() || 'iPhone')); haptic.success(); }
    catch (e) { haptic.error(); setError(e instanceof Error ? e.message : String(e)); }
    finally { setBusy(false); }
  };

  if (paired) return <View style={[styles.done, { backgroundColor: theme.bg }]} testID="paired">
    <View style={[styles.check, { backgroundColor: theme.okBg }]}><Icon name="checkmark" size={30} color={theme.ok} weight="bold"/></View>
    <Text style={[styles.title, { color: theme.textStrong }]}>Paired</Text>
    <Text style={[styles.body, { color: theme.secondary }]}>Your computer shows this phone as <Text style={{ fontWeight: '700', color: theme.text }}>{paired.name}</Text> with key</Text>
    <Text style={[styles.fingerprint, { color: theme.text, backgroundColor: theme.codeBg }]} selectable>{paired.fingerprint}</Text>
    <Text style={[styles.hint, { color: theme.muted }]}>Check that both screens show the same key.</Text>
    <Button label="Open Inbox" tone="primary" onPress={() => router.dismissTo('/')} style={styles.wide} testID="open-inbox"/>
  </View>;

  if (scanning) return <View style={styles.camera}>
    <CameraView style={StyleSheet.absoluteFill} facing="back" barcodeScannerSettings={{ barcodeTypes: ['qr'] }}
      onBarcodeScanned={({ data }) => {
        if (scanned.current) return;
        if (!parsePairingLink(data)) { setError('That code is not a Lazybox pairing code.'); return; }
        scanned.current = true; haptic.success(); setLink(data); setScanning(false);
      }}/>
    <View style={styles.viewfinder} pointerEvents="none"/>
    <View style={styles.cameraBar}>
      <Text style={styles.cameraText}>Point at the code on your computer</Text>
      {error ? <Text style={[styles.cameraText, { color: theme.warn }]}>{error}</Text> : null}
      <Button label="Cancel" onPress={() => setScanning(false)}/>
    </View>
  </View>;

  return <KeyboardAvoidingView behavior={Platform.OS === 'ios' ? 'padding' : undefined} style={{ flex: 1 }}>
    <ScrollView contentContainerStyle={styles.form} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic">
      <Text style={[styles.lead, { color: theme.secondary }]}>On your computer, open Lazybox → Settings → Devices and choose <Text style={{ fontWeight: '700', color: theme.text }}>Pair a phone</Text>. The code works once, for five minutes.</Text>
      <View style={styles.row}>
        <Button label="Scan code" icon="qrcode.viewfinder" tone={parsed ? 'secondary' : 'primary'} onPress={() => void scan()} style={styles.flex} testID="scan"/>
        <Button label="Paste link" icon="doc.on.clipboard" onPress={() => void paste()} style={styles.flex} testID="paste"/>
      </View>
      <Text style={[styles.label, { color: theme.muted }]}>PAIRING LINK</Text>
      <TextInput value={link} onChangeText={value => { setLink(value.trim()); setError(''); }} placeholder="https://your-mac.tailnet.ts.net:8443/…" placeholderTextColor={theme.faint}
        autoCapitalize="none" autoCorrect={false} keyboardType="url" multiline testID="link"
        style={[styles.input, styles.linkInput, { color: theme.text, backgroundColor: theme.surface, borderColor: link && !parsed ? theme.danger : theme.line }]}/>
      {parsed && <View style={[styles.card, { backgroundColor: theme.surface, borderColor: theme.lineSubtle }]}>
        <Text style={[styles.cardTitle, { color: theme.textStrong }]}>{parsed.baseUrl.replace(/^https?:\/\//, '')}</Text>
        <Text style={[styles.hint, { color: theme.muted, textAlign: 'left' }]}>This phone will be able to</Text>
        {parsed.offer.grants.map(grant => <View key={grant} style={styles.grant}><Icon name="checkmark" size={13} color={theme.ok} weight="bold"/><Text style={{ color: theme.text, fontSize: 15 }}>{grantText[grant] || grant}</Text></View>)}
      </View>}
      <Text style={[styles.label, { color: theme.muted }]}>NAME THIS PHONE</Text>
      <TextInput value={name} onChangeText={setName} maxLength={64} autoCorrect={false} testID="name" returnKeyType="done"
        style={[styles.input, { color: theme.text, backgroundColor: theme.surface, borderColor: theme.line }]}/>
      {error ? <Text style={[styles.error, { color: theme.danger }]} accessibilityRole="alert">{error}</Text> : null}
      <Button label={busy ? 'Pairing…' : 'Pair this phone'} tone="primary" busy={busy} disabled={!parsed || !name.trim()} onPress={() => void submit()} style={styles.wide} testID="pair-submit"/>
      <Text style={[styles.hint, { color: theme.muted }]}>A key is created on this phone and kept in its Keychain; it never leaves the phone. You can revoke this phone at any time in Settings → Devices on your computer.</Text>
    </ScrollView>
  </KeyboardAvoidingView>;
}

const styles = StyleSheet.create({
  form: { padding: space.l, gap: space.m },
  lead: { fontSize: 15, lineHeight: 21 },
  row: { flexDirection: 'row', gap: space.s },
  flex: { flex: 1 },
  label: { fontSize: 12, fontWeight: '700', letterSpacing: 0.6, marginTop: space.s },
  input: { minHeight: 44, borderWidth: StyleSheet.hairlineWidth, borderRadius: radius.m, paddingHorizontal: space.m, paddingVertical: 10, fontSize: 16 },
  linkInput: { fontFamily: mono, fontSize: 13, minHeight: 64 },
  card: { borderRadius: radius.m, borderWidth: StyleSheet.hairlineWidth, padding: space.m, gap: 6 },
  cardTitle: { fontSize: 16, fontWeight: '600' },
  grant: { flexDirection: 'row', alignItems: 'center', gap: space.s },
  error: { fontSize: 14, lineHeight: 20 },
  wide: { alignSelf: 'stretch', marginTop: space.s },
  hint: { fontSize: 13, lineHeight: 18, textAlign: 'center' },
  done: { flex: 1, alignItems: 'center', justifyContent: 'center', padding: space.xl, gap: space.m },
  check: { width: 64, height: 64, borderRadius: 32, alignItems: 'center', justifyContent: 'center' },
  title: { fontSize: 24, fontWeight: '700' },
  body: { fontSize: 16, textAlign: 'center', lineHeight: 22 },
  fingerprint: { fontFamily: mono, fontSize: 17, paddingHorizontal: space.m, paddingVertical: space.s, borderRadius: radius.s, overflow: 'hidden' },
  camera: { flex: 1, backgroundColor: '#000' },
  viewfinder: { position: 'absolute', alignSelf: 'center', top: '28%', width: 240, height: 240, borderRadius: 24, borderWidth: 3, borderColor: 'rgba(255,255,255,0.85)' },
  cameraBar: { position: 'absolute', left: 0, right: 0, bottom: 0, padding: space.xl, gap: space.m, backgroundColor: 'rgba(0,0,0,0.55)' },
  cameraText: { color: '#fff', fontSize: 15, textAlign: 'center' },
});
