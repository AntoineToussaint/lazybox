import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { uiWatcher } from '../../scripts/ui-watch.mjs';

export default defineConfig({ plugins: [react()], clearScreen: false, server: { watch: { ...uiWatcher().options, ignored: ['**/src-tauri/**'] } } });
