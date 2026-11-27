import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { uiWatcher } from '../../scripts/ui-watch.mjs';
import { drawingAssets } from '../../scripts/drawing-assets.mjs';
import { monacoVimCompat } from '../../scripts/monaco-vim-compat.mjs';

export default defineConfig({ plugins: [react(), monacoVimCompat(), drawingAssets()], clearScreen: false, server: { watch: { ...uiWatcher().options, ignored: ['**/src-tauri/**'] } } });
