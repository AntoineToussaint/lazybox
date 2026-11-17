// Renders scenarios to video, or runs them as tests.
//
//   npm run scenario -- start-from-issue            # MP4 + WebM + contact sheet + report
//   npm run scenario -- all --gif                   # every scenario, with a GIF each
//   npm run scenario -- inbox-on-phone --light      # another theme (or --dark)
//   npm run scenario -- pr-checks-loop --phone      # the phone layout
//   npm run scenario -- all --no-video              # fast: the scenarios as end-to-end tests
//   npm run scenario -- --list                      # names, titles and stories
//
// Output: .cache/scenarios/<name>[-phone][-light].{mp4,webm,gif,contact.png,report.json}.
// Video needs ffmpeg on PATH (or FFMPEG=…); without it, Playwright's own 1×
// WebM is written instead. Options: --scale=N (device pixels, default 2),
// --no-webm, --timeout=ms (per check, default 10000).
import { Stage, listScenarios, loadScenario, runScenario, OUTPUT_DIR, type RunOptions } from './lib/runner.ts';
import { findFfmpeg } from './lib/recorder.ts';

const args = process.argv.slice(2);
const flag = (name: string) => args.includes(`--${name}`);
const value = (name: string) => args.find(a => a.startsWith(`--${name}=`))?.slice(name.length + 3);
const known = new Set(['list', 'phone', 'light', 'dark', 'gif', 'no-video', 'no-webm']);
const unknown = args.filter(a => a.startsWith('--') && !known.has(a.slice(2)) && !['scale', 'timeout'].includes(a.slice(2).split('=')[0]));
if (unknown.length) { console.error(`Unknown option: ${unknown.join(', ')}`); process.exit(2); }

const available = await listScenarios();
if (flag('list')) {
  for (const [name, file] of available) { const s = await loadScenario(file); console.log(`${name.padEnd(26)} ${s.title}\n${' '.repeat(27)}As ${s.story.as}, I want ${s.story.want}, so that ${s.story.soThat}.`); }
  process.exit(0);
}
const wanted = args.filter(a => !a.startsWith('--'));
if (!wanted.length) { console.error(`Name a scenario, or all. Known: ${[...available.keys()].join(', ')}`); process.exit(2); }
const names = wanted.includes('all') ? [...available.keys()] : wanted;
const missing = names.filter(n => !available.has(n));
if (missing.length) { console.error(`Unknown scenario: ${missing.join(', ')}. Known: ${[...available.keys()].join(', ')}`); process.exit(2); }

const options: RunOptions = {
  video: !flag('no-video'), gif: flag('gif'), webm: !flag('no-webm'), phone: flag('phone'),
  theme: flag('light') ? 'light' : flag('dark') ? 'dark' : undefined,
  scale: Number(value('scale') || 2), timeout: Number(value('timeout') || 10_000),
};
if (options.video && !findFfmpeg()) console.warn('ffmpeg is not on PATH: writing Playwright\'s own WebM at 1× (install ffmpeg for crisp MP4, WebM and GIF).');

const stage = new Stage();
let failures = 0;
try {
  await stage.start();
  for (const name of names) {
    const scenario = await loadScenario(available.get(name)!);
    const report = await runScenario(stage, name, scenario, options);
    const passed = report.status === 'passed';
    if (!passed) failures++;
    const outputs = report.outputs.map(o => `${o.file.split('/').at(-1)} ${(o.bytes / 1048576).toFixed(1)} MB${o.seconds ? ` ${o.seconds}s` : ''}`).join(', ');
    console.log(`${passed ? 'PASS' : 'FAIL'} ${report.name.padEnd(26)} ${report.steps.filter(s => s.status === 'passed').length}/${report.steps.length} steps, ${report.checks} checks, ${(report.durationMs / 1000).toFixed(1)}s${outputs ? `  → ${outputs}` : ''}`);
    if (!passed) {
      console.log(`     ${report.error}`);
      const step = report.steps.find(s => s.status === 'failed');
      if (step) console.log(`     at step ${step.index + 1}: ${step.label}`);
      console.log(`     screenshot: ${OUTPUT_DIR}${report.name}-failed.png (accessibility tree: ${report.name}-failed.aria.yaml)`);
    }
  }
} finally { await stage.stop(); }
console.log(`${names.length - failures} of ${names.length} scenarios passed${options.video ? `; output in ${OUTPUT_DIR}` : ''}`);
process.exit(failures ? 1 : 0);
