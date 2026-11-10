// Read-only with respect to the project: exercise Vite in a disposable folder.
// This diagnoses backend delivery; it does not switch the application's watcher.
import { createServer } from 'vite';
import { watch } from 'node:fs';
import { mkdtemp, writeFile, rm, realpath, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, basename } from 'node:path';

const directory = await realpath(await mkdtemp(join(tmpdir(), 'lazybox-ui-watch-')));
const file = join(directory, 'fixture.css');
const observed = [];
try {
  for (const [backend, options] of [['default', {}], ['native', { useFsEvents: false, usePolling: false }], ['polling', { useFsEvents: false, usePolling: true }]]) {
    await writeFile(file, 'body { color: red; }');
    const events = [];
    const native = watch(file, event => events.push({ source: 'node-file-watch', event }));
    let server;
    try {
      server = await createServer({ root: directory, configFile: false, logLevel: 'error', server: { middlewareMode: true, watch: options } });
      server.watcher.on('all', (event, path) => { if (path === file) events.push({ source: 'vite', event }); });
      const readyDeadline = Date.now() + 3000;
      while (!Object.values(server.watcher.getWatched()).some(files => files.includes(basename(file)))) {
        if (Date.now() > readyDeadline) throw new Error('Vite did not register the diagnostic file');
        await new Promise(resolve => setTimeout(resolve, 25));
      }
      await writeFile(file, 'body { color: blue; }');
      await new Promise(resolve => setTimeout(resolve, 2000));
      observed.push({ backend, options, delivered: events.some(item => item.source === 'vite' && item.event === 'change'), events });
    } finally { native.close(); await server?.close(); }
  }
  const vite = JSON.parse(await readFile(new URL('../node_modules/vite/package.json', import.meta.url), 'utf8')).version;
  console.log(JSON.stringify({ version: 1, platform: process.platform, node: process.version, vite, observed }, null, 2));
  if (!observed[0].delivered) process.exitCode = 1;
} finally { await rm(directory, { recursive: true, force: true }); }
