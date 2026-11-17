import assert from 'node:assert/strict';
import test from 'node:test';
import { LogModel, SourceColours, logRows, readLine, sliceSegments, tailLines, type LogLine } from '../packages/surfaces/src/log-lines.ts';
import { stripAnsi } from '../packages/surfaces/src/ansi.tsx';

const E = '\x1b';
const part = (line: LogLine, span?: { start: number; end: number }) => span ? line.text.slice(span.start, span.end) : undefined;
const message = (line: LogLine) => line.text.slice(line.message);

// The console log the person pasted, from a multi-service Codefly run (with its own elisions).
const CODEFLY = [
  '21:28:14 fte-tracker/web     > - Experiments (use with caution):',
  '21:28:15 fte-tracker/web     > Next.js development server running on port 57771',
  '>> fte-tracker/web: Running on :57771',
  '21:28:15 authority/edge      | (INFO) Building go binary',
  '(WARN) (Flow.checkLoopback) a native service answers off loopback; … answers=[100.77.139.126:57771 192.168.1.21:57771 … [2a01:cb0d:…]:57771 …]',
  '[codefly] still starting authority/edge… 10s',
  '>> authority/edge: Running in 15.2s',
].join('\n');

test('the Codefly sample: timestamps, sources, levels and milestones', () => {
  const model = new LogModel(); model.append(CODEFLY + '\n');
  const [experiments, next, running, building, warn, starting, ready] = model.lines;
  assert.equal(experiments.time?.label, '21:28:14');
  assert.equal(experiments.source?.name, 'fte-tracker/web');
  assert.equal(experiments.source?.sep, '>');
  assert.equal(message(experiments), '- Experiments (use with caution):');
  assert.equal(experiments.milestone, false);

  assert.equal(next.milestone, true, '"running on port" announces the server');
  assert.deepEqual(next.links.map(link => [link.kind, part(next, link)]), [['port', '57771']]);
  assert.equal(next.links[0].kind === 'port' && next.links[0].url, 'http://localhost:57771/');

  assert.equal(running.milestone, true);
  assert.equal(part(running, running.marker), '>>');
  assert.equal(running.source?.name, 'fte-tracker/web');
  assert.equal(message(running), 'Running on :57771');
  assert.deepEqual(running.links.map(link => part(running, link)), [':57771']);

  assert.equal(building.source?.name, 'authority/edge');
  assert.equal(building.source?.sep, '|');
  assert.equal(building.level, 'info');
  assert.equal(part(building, building.levelSpan), '(INFO)');
  assert.equal(building.levelSpan?.label, 'INFO');

  assert.equal(warn.level, 'warn');
  assert.equal(warn.source, undefined);
  assert.equal(warn.continuation, false, 'a line of its own, not part of the one above');
  assert.equal(warn.lists.length, 1);
  assert.equal(part(warn, warn.lists[0]), '[100.77.139.126:57771 192.168.1.21:57771 … [2a01:cb0d:…]:57771 …]');
  assert.equal(warn.lists[0].label, '3 addresses');
  assert.deepEqual(warn.links, [], 'addresses off loopback are not local ports');

  assert.equal(starting.source?.name, 'codefly');
  assert.equal(starting.source?.open, '[');
  assert.equal(message(starting), 'still starting authority/edge… 10s');
  assert.equal(ready.milestone, true);
  assert.equal(ready.source?.name, 'authority/edge');

  assert.deepEqual(model.sources, ['fte-tracker/web', 'authority/edge', 'codefly']);
  assert.equal(model.hasTime, true);
  assert.equal(model.sourceWidth, 'fte-tracker/web'.length);
  // Never lossy: Copy reads the raw output, exactly.
  assert.equal(stripAnsi(model.raw), CODEFLY + '\n');
});

test('timestamps in their common forms', () => {
  for (const [line, label] of [
    ['21:28:14 plain', '21:28:14'], ['[21:28:14] bracketed', '21:28:14'], ['21:28:14.532 millis', '21:28:14'],
    ['2026-11-16T21:28:14.123Z iso', '21:28:14'], ['2026-11-16 21:28:14,123 python', '21:28:14'], ['2026/11/16 21:28:14 go log', '21:28:14'],
  ]) assert.equal(readLine(line).time?.label, label, line);
  assert.equal(readLine('took 21:28:14 to run').time, undefined, 'only at the start of a line');
  assert.equal(readLine('21:28:14 fte-tracker/web     > hi').key, 'fte-tracker/web     > hi', 'the fold key leaves the time out');
});

test('sources: docker compose, pnpm, turbo, brackets and runners', () => {
  const cases: [string, string, string][] = [
    ['web-1  | ready on 3000', 'web-1', 'ready on 3000'],
    ['api_1   | GET /health 200', 'api_1', 'GET /health 200'],
    ['web.1  | started', 'web.1', 'started'],
    ['db-1  | 2026-11-16T21:28:14Z LOG:  database system is ready', 'db-1', 'LOG:  database system is ready'],
    ['packages/web dev: ready - started server on 0.0.0.0:3000', 'packages/web dev', 'ready - started server on 0.0.0.0:3000'],
    ['apps/api build$ tsc -p .', 'apps/api build', 'tsc -p .'],
    ['@acme/ui:build: dist/index.js 12kb', '@acme/ui:build', 'dist/index.js 12kb'],
    ['web:dev: compiled', 'web:dev', 'compiled'],
    ['[codefly] waiting', 'codefly', 'waiting'],
    ['[0] vite v5 ready', '0', 'vite v5 ready'],
    ['worker    > tick', 'worker', 'tick'],
  ];
  for (const [text, name, rest] of cases) {
    const line = readLine(text);
    assert.equal(line.source?.name, name, text);
    assert.equal(message(line), rest, text);
  }
  assert.equal(readLine('db-1  | 2026-11-16T21:28:14Z ready').time?.label, '21:28:14', 'compose with --timestamps: the time after the service');
  for (const text of ['[INFO] building', '[1/3] Resolving packages', 'src/app.ts error: nope', '12 |     let x = 1;', 'INFO  | message', 'Error: boom', 'http://localhost:3000: ok'])
    assert.equal(readLine(text).source, undefined, text);
});

test('levels: wrapped, bare, compilers, npm, go test and key=value', () => {
  const cases: [string, string, string][] = [
    ['(INFO) Building', 'info', '(INFO)'], ['(ERROR) failed', 'error', '(ERROR)'], ['[WARN] slow', 'warn', '[WARN]'], ['[warning] slow', 'warn', '[warning]'],
    ['WARN something', 'warn', 'WARN'], ['ERROR: it broke', 'error', 'ERROR:'], ['INFO[0000] logrus', 'info', 'INFO[0000]'],
    ['error[E0308]: mismatched types', 'error', 'error[E0308]:'], ['warning: unused variable', 'warn', 'warning:'],
    ['npm ERR! code ELIFECYCLE', 'error', 'ERR!'], ['npm warn deprecated x', 'warn', 'warn'],
    ['--- FAIL: TestParse (0.00s)', 'error', 'FAIL'], ['FAIL\texample.com/parser', 'error', 'FAIL'], ['panic: runtime error', 'error', 'panic:'],
    ['time=2026-11-16 level=warn msg="disk"', 'warn', 'level=warn'], ['{"level":"error","msg":"x"}', 'error', '"level":"error"'],
  ];
  for (const [text, level, span] of cases) {
    const line = readLine(text);
    assert.equal(line.level, level, text);
    assert.equal(part(line, line.levelSpan), span, text);
  }
  for (const text of ['0 ERRORS found', 'no errors', 'information: none', 'Errorless run']) assert.equal(readLine(text).level, undefined, text);
  assert.equal(readLine('21:28:15 authority/edge      | (ERROR) listening on :8080 failed').milestone, false, 'an error is never a milestone');
});

test('milestones', () => {
  for (const text of ['>> api: Running on :8080', 'Server listening on http://127.0.0.1:4000', '  VITE v5.4.0  ready in 312 ms', '  ➜  Local:   http://localhost:5173/', 'api listening at :9000', 'Started server on 0.0.0.0:3000'])
    assert.equal(readLine(text).milestone, true, text);
  for (const text of ['Building go binary', 'compiling…', 'listening for changes']) assert.equal(readLine(text).milestone, false, text);
});

test('links: web addresses, local ports and files with a line', () => {
  const kinds = (text: string) => readLine(text).links.map(link => [link.kind, text.slice(link.start, link.end)]);
  assert.deepEqual(kinds('Local: http://localhost:5173/, then (see https://nextjs.org/docs).'), [['url', 'http://localhost:5173/'], ['url', 'https://nextjs.org/docs']]);
  assert.deepEqual(kinds('Listening on localhost:3000 and 0.0.0.0:8080 and [::1]:9000'), [['port', 'localhost:3000'], ['port', '0.0.0.0:8080'], ['port', '[::1]:9000']]);
  assert.deepEqual(kinds('PORT=4000 port: 4001'), [['port', '4000'], ['port', '4001']]);
  assert.deepEqual(kinds('at 21:28:14 and 192.168.1.21:57771 and example.com:443'), [], 'times, LAN addresses and hosts are not local ports or files');
  assert.deepEqual(kinds('    parser_test.go:42: want 3, got 4'), [['file', 'parser_test.go:42']]);
  assert.deepEqual(kinds('/Users/me/proj/internal/run.go:118 +0x1d'), [['file', '/Users/me/proj/internal/run.go:118']]);
  assert.deepEqual(kinds('    at render (./src/App.tsx:12:5)'), [['file', './src/App.tsx:12:5']]);
  assert.deepEqual(kinds('see services/api/Dockerfile:3'), [['file', 'services/api/Dockerfile:3']]);
  const file = readLine('src/App.tsx:12:5 broke').links[0];
  assert.ok(file.kind === 'file' && file.path === 'src/App.tsx' && file.line === 12 && file.column === 5);
  assert.deepEqual(kinds('http://localhost:3000/app.js:12'), [['url', 'http://localhost:3000/app.js:12']], 'a file inside an address is the address');
});

test('long lists fold; short ones do not', () => {
  const addresses = Array.from({ length: 9 }, (_, i) => `192.168.1.${i + 10}:57771`);
  const line = readLine(`(WARN) answers off loopback; answers=[${addresses.join(' ')}] first=1`);
  assert.equal(line.lists.length, 1);
  assert.equal(line.lists[0].label, '9 addresses');
  assert.equal(line.text.slice(line.lists[0].end), ' first=1');
  const v6 = readLine(`answers=[${['[2a01:cb0d:1::1]:80', '[fe80::1]:80', '127.0.0.1:80', '10.0.0.1:80'].join(', ')}]`);
  assert.equal(v6.lists[0]?.label, '4 addresses');
  assert.equal(readLine(`tags=[${['alpha', 'beta', 'gamma', 'delta', 'epsilon', 'zeta'].join(', ')}]`).lists[0]?.label, '6 items');
  assert.equal(readLine('[a b c d]').lists.length, 0, 'short');
  assert.equal(readLine('[codefly] x').lists.length, 0);
  const bare = readLine(`peers 10.0.0.1, 10.0.0.2, 10.0.0.3, 10.0.0.4, 10.0.0.5 ok`);
  assert.equal(bare.lists[0]?.label, '5 addresses');
});

test('repeats fold into ×N; noise hides repeated warnings and folds progress', () => {
  const model = new LogModel();
  model.append([
    '21:00:01 api  | (WARN) (Flow.checkLoopback) answers off loopback answers=[1.1.1.1:1 2.2.2.2:2 3.3.3.3:3 4.4.4.4:4]',
    '21:00:02 api  | GET /health 200',
    '21:00:03 api  | GET /health 200',
    '21:00:04 api  | GET /health 200',
    '21:00:05 api  | (WARN) (Flow.checkLoopback) answers off loopback answers=[5.5.5.5:5 6.6.6.6:6 7.7.7.7:7 8.8.8.8:8]',
    '[codefly] still starting api… 5s',
    '[codefly] still starting api… 10s',
    '[codefly] still starting api… 15s',
    '',
    '',
  ].join('\n'));
  const lines = model.all();
  const plain = logRows(lines);
  assert.deepEqual(plain.rows.map(row => [row.line.text.slice(0, 40), row.count]), [
    ['21:00:01 api  | (WARN) (Flow.checkLoopba', 1], ['21:00:04 api  | GET /health 200', 3], ['21:00:05 api  | (WARN) (Flow.checkLoopba', 1],
    ['[codefly] still starting api… 5s', 1], ['[codefly] still starting api… 10s', 1], ['[codefly] still starting api… 15s', 1], ['', 1],
  ]);
  assert.equal(plain.rows[1].first.time?.label, '21:00:02', 'a fold remembers its first line');
  assert.equal(plain.noise, 1, 'the repeated warning is counted even while shown');
  const quiet = logRows(lines, { hideNoise: true });
  assert.equal(quiet.noise, 1);
  assert.deepEqual(quiet.rows.map(row => [row.line.id, row.count]), [[lines[0].id, 1], [lines[3].id, 3], [lines[7].id, 3], [lines[8].id, 1]]);
  assert.equal(quiet.rows[2].line.text, '[codefly] still starting api… 15s', 'progress folds to its latest line');
  // Expanded, a fold shows each line, and the last carries the count to fold it again.
  const expanded = logRows(lines, { expanded: new Set([lines[1].id]) });
  assert.deepEqual(expanded.rows.slice(1, 4).map(row => [row.line.time?.label, row.count, !!row.expanded]), [['21:00:02', 1, false], ['21:00:03', 1, false], ['21:00:04', 3, true]]);
});

test('source filters and search', () => {
  const model = new LogModel();
  model.append('web-1  | ready\napi-1  | listening on :8080\n    at continuation\nplain line\nweb-1  | GET /\n');
  const lines = model.all();
  assert.equal(lines[2].owner, 'api-1', 'an indented line belongs to the source above');
  const rows = logRows(lines, { hidden: new Set(['web-1']) });
  assert.deepEqual(rows.rows.map(row => row.line.text), ['api-1  | listening on :8080', '    at continuation', 'plain line']);
  assert.equal(rows.counts.get('web-1'), 2);
  assert.equal(rows.counts.get(''), 1);
  assert.deepEqual(logRows(lines, { hidden: new Set(['']) }).rows.map(row => row.line.text).includes('plain line'), false, '"Other" hides lines with no source');
  assert.deepEqual(logRows(lines, { search: 'GET' }).rows.map(row => row.line.text), ['web-1  | GET /']);
});

test('streaming: lines are read once, the partial line is re-read, ANSI state carries, the tail is kept', () => {
  const model = new LogModel(200);
  model.append(`${E}[32mweb-1  | gre`);
  assert.equal(model.lines.length, 0);
  assert.equal(model.partial?.text, 'web-1  | gre');
  const partialId = model.partial!.id;
  model.append(`en\nweb-1  | still green${E}[0m\nplain\n`);
  assert.equal(model.lines[0].id, partialId, 'the partial line keeps its id once complete');
  assert.equal(model.lines[0].text, 'web-1  | green');
  assert.equal(model.lines[1].segments[0].style.fg, 'var(--ansi-2)', 'colour carries to the next line');
  assert.equal(model.partial, undefined);
  const before = model.lines[0];
  model.append('x\n');
  assert.equal(model.lines[0], before, 'complete lines are not re-read');
  for (let i = 0; i < 40; i++) model.append(`line ${i}\n`);
  assert.ok(model.raw.length <= 200);
  assert.equal(stripAnsi(model.raw), model.lines.map(line => line.text).join('\n') + '\n', 'raw and lines agree after trimming');
  assert.equal(model.lines.at(-1)!.text, 'line 39');
  model.append('progress 10%\rprogress 50%');
  assert.equal(model.partial?.text, 'progress 50%', 'a carriage return redraws the line');
});

test('source colours are stable and distinct within a log', () => {
  const a = new SourceColours(), b = new SourceColours();
  const names = ['fte-tracker/web', 'authority/edge', 'codefly'];
  const first = names.map(name => a.colour(name));
  assert.equal(new Set(first).size, 3);
  assert.deepEqual(names.map(name => b.colour(name)), first, 'the same sources get the same colours in another run');
  assert.ok(first.every(colour => colour >= 1 && colour <= 6));
});

test('tails and segment slices', () => {
  assert.deepEqual(tailLines('a\nb\nc\n\n\x1b[0m\n', 2), ['b', 'c']);
  const line = readLine(`${E}[31mred${E}[0m plain`);
  assert.deepEqual(sliceSegments(line.segments, 1, 6).map(s => s.text), ['ed', ' pl']);
});
