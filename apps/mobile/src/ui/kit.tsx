import type { ReactNode } from 'react';
import { ActivityIndicator, Pressable, StyleSheet, Text, View, type StyleProp, type ViewStyle } from 'react-native';
import { SymbolView, type SFSymbol } from 'expo-symbols';
import * as Haptics from 'expo-haptics';
import type { AttentionState } from '@lazybox/surfaces/models';
import { radius, space, touch, useTheme, type Theme } from './theme';

/** Haptics: a light tick on a tap, a success or warning on what was sent. Never on scroll or typing. */
export const haptic = {
  tap: () => void Haptics.impactAsync(Haptics.ImpactFeedbackStyle.Light).catch(() => {}),
  select: () => void Haptics.selectionAsync().catch(() => {}),
  success: () => void Haptics.notificationAsync(Haptics.NotificationFeedbackType.Success).catch(() => {}),
  warning: () => void Haptics.notificationAsync(Haptics.NotificationFeedbackType.Warning).catch(() => {}),
  error: () => void Haptics.notificationAsync(Haptics.NotificationFeedbackType.Error).catch(() => {}),
};

export function Icon({ name, size = 18, color, weight = 'medium' }: { name: SFSymbol; size?: number; color: string; weight?: 'regular' | 'medium' | 'semibold' | 'bold' }) {
  return <SymbolView name={name} size={size} tintColor={color} weight={weight} resizeMode="scaleAspectFit" style={{ width: size, height: size }}/>;
}

export function stateStyle(state: AttentionState | undefined, theme: Theme): { icon: SFSymbol; color: string } {
  switch (state) {
    case 'needs-input': return { icon: 'hand.raised.fill', color: theme.warn };
    case 'failed': return { icon: 'exclamationmark.octagon.fill', color: theme.danger };
    case 'review': return { icon: 'checkmark.circle.fill', color: theme.info };
    case 'working': return { icon: 'circle.dotted.circle', color: theme.running };
    case 'idle': return { icon: 'checkmark', color: theme.faint };
    default: return { icon: 'pin.fill', color: theme.accent };
  }
}

type Tone = 'primary' | 'secondary' | 'danger' | 'plain';
export function Button({ label, onPress, tone = 'secondary', icon, disabled, busy, compact, style, testID, accessibilityLabel }: {
  label: string; onPress(): void; tone?: Tone; icon?: SFSymbol; disabled?: boolean; busy?: boolean; compact?: boolean; style?: StyleProp<ViewStyle>; testID?: string; accessibilityLabel?: string;
}) {
  const theme = useTheme();
  const colors = {
    primary: { bg: theme.accentSolid, fg: theme.onAccentSolid, border: theme.accentSolid },
    secondary: { bg: theme.raised, fg: theme.text, border: theme.line },
    danger: { bg: theme.dangerBg, fg: theme.danger, border: theme.dangerLine },
    plain: { bg: 'transparent', fg: theme.accent, border: 'transparent' },
  }[tone];
  const off = disabled || busy;
  return <Pressable testID={testID} accessibilityRole="button" accessibilityLabel={accessibilityLabel || label} accessibilityState={{ disabled: !!off, busy: !!busy }} disabled={off}
    onPress={() => { haptic.tap(); onPress(); }}
    style={({ pressed }) => [styles.button, compact && styles.compact, { backgroundColor: colors.bg, borderColor: colors.border, opacity: off ? 0.5 : pressed ? 0.75 : 1 }, style]}>
    {busy ? <ActivityIndicator size="small" color={colors.fg}/> : icon ? <Icon name={icon} size={15} color={colors.fg} weight="semibold"/> : null}
    <Text style={[styles.buttonText, { color: colors.fg }]} numberOfLines={1}>{label}</Text>
  </Pressable>;
}

export function IconButton({ icon, label, onPress, color, size = 20, testID, disabled }: { icon: SFSymbol; label: string; onPress(): void; color?: string; size?: number; testID?: string; disabled?: boolean }) {
  const theme = useTheme();
  return <Pressable testID={testID} accessibilityRole="button" accessibilityLabel={label} hitSlop={8} disabled={disabled} onPress={() => { haptic.tap(); onPress(); }}
    style={({ pressed }) => [styles.iconButton, { opacity: disabled ? 0.4 : pressed ? 0.5 : 1 }]}>
    <Icon name={icon} size={size} color={color || theme.accent}/>
  </Pressable>;
}

/** The one quiet line that says the computer cannot be reached, with a way to try again. */
export function ConnectionBanner({ label, detail, onRetry }: { label: string; detail?: string; onRetry?(): void }) {
  const theme = useTheme();
  return <View accessibilityRole="alert" style={[styles.banner, { backgroundColor: theme.warnBg, borderColor: theme.warnLine }]} testID="connection-banner">
    <ActivityIndicator size="small" color={theme.warn}/>
    <View style={{ flex: 1 }}>
      <Text style={[styles.bannerLabel, { color: theme.text }]}>{label}</Text>
      {detail ? <Text style={[styles.bannerDetail, { color: theme.muted }]} numberOfLines={2}>{detail}</Text> : null}
    </View>
    {onRetry && <Pressable accessibilityRole="button" onPress={() => { haptic.tap(); onRetry(); }} hitSlop={10}><Text style={{ color: theme.accent, fontWeight: '600' }}>Retry</Text></Pressable>}
  </View>;
}

export function Pill({ children, color, bg }: { children: ReactNode; color: string; bg: string }) {
  return <View style={[styles.pill, { backgroundColor: bg }]}><Text style={[styles.pillText, { color }]}>{children}</Text></View>;
}

/** A small live/connecting dot with its word, for a header. */
export function LiveDot({ tone, label }: { tone: 'live' | 'connecting' | 'reconnecting' | 'unreachable' | 'ended'; label: string }) {
  const theme = useTheme();
  const color = tone === 'live' ? theme.ok : tone === 'unreachable' ? theme.danger : tone === 'ended' ? theme.faint : theme.warn;
  return <View style={styles.live} accessibilityLabel={label} testID={`live-${tone}`}>
    <View style={[styles.dot, { backgroundColor: color }]}/>
    <Text style={[styles.liveText, { color: theme.muted }]}>{label}</Text>
  </View>;
}

const styles = StyleSheet.create({
  button: { minHeight: touch, paddingHorizontal: space.l, borderRadius: radius.m, borderWidth: StyleSheet.hairlineWidth, flexDirection: 'row', alignItems: 'center', justifyContent: 'center', gap: space.s },
  compact: { minHeight: 40, paddingHorizontal: space.m },
  buttonText: { fontSize: 16, fontWeight: '600' },
  iconButton: { minWidth: touch, minHeight: touch, alignItems: 'center', justifyContent: 'center' },
  banner: { flexDirection: 'row', alignItems: 'center', gap: space.m, marginHorizontal: space.l, marginVertical: space.s, padding: space.m, borderRadius: radius.m, borderWidth: StyleSheet.hairlineWidth },
  bannerLabel: { fontSize: 14, fontWeight: '600' },
  bannerDetail: { fontSize: 12, marginTop: 2 },
  pill: { paddingHorizontal: space.s, paddingVertical: 2, borderRadius: radius.pill },
  pillText: { fontSize: 12, fontWeight: '600' },
  live: { flexDirection: 'row', alignItems: 'center', gap: 6 },
  dot: { width: 7, height: 7, borderRadius: 4 },
  liveText: { fontSize: 12, fontWeight: '500' },
});
