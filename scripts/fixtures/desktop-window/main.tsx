// A desktop window without the native shell, for browser tests: it follows the
// Runner locator exactly as apps/desktop does (the shared useRunnerLocator),
// except that the locator is read over HTTP from the test server instead of
// through the native host. The window calls the Runner's own origin.
import React from 'react';
import { createRoot } from 'react-dom/client';
import { Workspace, useRunnerLocator } from '@lazybox/surfaces';

const params = new URLSearchParams(location.search);
const intervalMs = Number(params.get('interval') || 3000);
const read = async () => {
  const response = await fetch('/fixture-locator', { cache: 'no-store' });
  if (!response.ok) throw new Error(await response.text());
  return await response.json() as { url: string; token: string };
};
const viewState = { key: 'fixture:main', scope: 'fixture', root: 'fixture', getItem: (key: string) => localStorage.getItem(key), setItem: (key: string, value: string) => localStorage.setItem(key, value) };

function DesktopWindow() {
  const { value, error, renew, follow } = useRunnerLocator(read, { intervalMs });
  if (!value) return <p role={error ? 'alert' : 'status'}>{error ? `Unable to restore this window: ${error}` : 'Restoring your windows…'}</p>;
  return <Workspace connection={{ baseUrl: value.url, token: value.token, renew }} viewState={viewState} host={{ expectRunnerRestart: async () => follow() }}/>;
}
createRoot(document.getElementById('root')!).render(<React.StrictMode><DesktopWindow/></React.StrictMode>);
