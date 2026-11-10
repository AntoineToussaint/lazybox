import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { readFileSync } from 'node:fs';
import { uiWatcher } from '../../scripts/ui-watch.mjs';

export default defineConfig(({ command }) => {
  if (command === 'build') return { plugins: [react()] };
  const path = process.env.LAZYBOX_V2_ENDPOINT;
  if (!path) throw new Error('Set LAZYBOX_V2_ENDPOINT to the endpoint file printed by the Runner');
  const endpoint = JSON.parse(readFileSync(path, 'utf8')) as { url: string };
  return { plugins: [react()], server: { watch: uiWatcher().options, proxy: { '/lazybox.bench.v1.RunnerService/': { target: endpoint.url, changeOrigin: true } } } };
});
