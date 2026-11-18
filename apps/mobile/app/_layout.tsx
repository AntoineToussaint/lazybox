import { DarkTheme, DefaultTheme, Stack, ThemeProvider } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import * as SystemUI from 'expo-system-ui';
import { useEffect } from 'react';
import { LogBox } from 'react-native';
import { RunnerProvider } from '../src/runner/RunnerProvider';
import { useTheme } from '../src/ui/theme';

// A pairing link can open the app straight onto the pairing sheet: the Inbox stays beneath it.
export const unstable_settings = { anchor: 'index' };

// Development only: React Native's native animated driver reports an event
// for a value nothing listens to (the native header's large-title collapse).
// It is React Native's own bookkeeping, not this app's; its toast would cover
// the composer. Exactly that message, nothing else.
LogBox.ignoreLogs(['Sending `onAnimatedValueUpdate` with no listeners registered.']);

export default function Layout() {
  const theme = useTheme();
  useEffect(() => { void SystemUI.setBackgroundColorAsync(theme.bg); }, [theme.bg]);
  const base = theme.scheme === 'dark' ? DarkTheme : DefaultTheme;
  const navigation = { ...base, colors: { ...base.colors, primary: theme.accent, background: theme.bg, card: theme.bg, text: theme.textStrong, border: theme.lineSubtle, notification: theme.warn } };
  return <RunnerProvider>
    <ThemeProvider value={navigation}>
      <StatusBar style={theme.scheme === 'dark' ? 'light' : 'dark'}/>
      <Stack screenOptions={{ headerTintColor: theme.accent, headerTitleStyle: { color: theme.textStrong }, headerShadowVisible: false, contentStyle: { backgroundColor: theme.bg }, headerBackButtonDisplayMode: 'minimal' }}>
        <Stack.Screen name="index" options={{ title: 'Inbox', headerLargeTitle: true, headerLargeTitleShadowVisible: false }}/>
        <Stack.Screen name="pair" options={{ presentation: 'modal', title: 'Pair with your computer' }}/>
        <Stack.Screen name="conversation/[id]" options={{ title: '', headerTransparent: false, headerStyle: { backgroundColor: theme.bg } }}/>
        <Stack.Screen name="settings" options={{ presentation: 'modal', title: 'This phone' }}/>
      </Stack>
    </ThemeProvider>
  </RunnerProvider>;
}
