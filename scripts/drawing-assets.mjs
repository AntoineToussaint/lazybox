// Excalidraw resolves font subsets at runtime, relative to EXCALIDRAW_ASSET_PATH.
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { viteStaticCopy } from 'vite-plugin-static-copy';

export function drawingAssets() {
  const dist = dirname(fileURLToPath(import.meta.resolve('@excalidraw/excalidraw')));
  return viteStaticCopy({ targets: [{ src: join(dist, 'fonts/**/*'), dest: 'drawing-assets', rename: (_, __, path) => ({ stripBase: true, name: relative(dist, path) }) }] });
}
