import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const html = await readFile(new URL('../dist/index.html', import.meta.url), 'utf8');
const stylesheetPath = html.match(/href="(\/_astro\/[^"]+\.css)"/)?.[1];
assert.ok(stylesheetPath, 'expected the built homepage stylesheet');
const css = await readFile(new URL(`../dist${stylesheetPath}`, import.meta.url), 'utf8');
const socialPreview = await readFile(new URL('../public/og.png', import.meta.url));

// `&&` is HTML-escaped to `&amp;&amp;` in the built markup; the raw HTML we read
// here contains the escaped form, so these constants match what ships.
const brewCommand =
  'brew tap AntoineToussaint/lazybox &amp;&amp; brew trust AntoineToussaint/lazybox &amp;&amp; brew install lazybox';
const sourceCommand =
  'git clone https://github.com/AntoineToussaint/lazybox &amp;&amp; cd lazybox &amp;&amp; make setup &amp;&amp; make run';

test('the homepage leads with the brew-only install and buries the source build', () => {
  const detailsIndex = html.indexOf('<details class="install-more">');
  const brewIndex = html.indexOf(brewCommand);
  const sourceIndex = html.indexOf('From source <span>Contributors</span>');

  assert.ok(detailsIndex > 0, 'expected the contributor build to be disclosed behind a details toggle');
  assert.ok(brewIndex > 0 && brewIndex < detailsIndex, 'expected the brew command before the disclosed build');
  assert.ok(sourceIndex > detailsIndex, 'expected the source build inside the details disclosure');
  assert.equal(
    html.slice(0, detailsIndex).includes(sourceCommand),
    false,
    'source build must not appear as a primary command',
  );
  // No deprecated user-facing installers on the homepage.
  assert.equal(html.includes('lazybox-tui-installer.sh'), false, 'the curl installer must not be advertised');
  assert.equal(html.includes('cargo install --git'), false, 'the broken cargo-install path must not be advertised');
  assert.ok(html.includes('Requires Rust 1.88+'), 'expected the source build requirements');
});

test('every install method has a copy control for its exact command', () => {
  for (const command of [brewCommand, sourceCommand]) {
    assert.ok(html.includes(`data-copy="${command}"`), `missing copy control for: ${command}`);
  }
});

test('the hero presents lazybox as a terminal TUI before any install choice', () => {
  const heroStart = html.indexOf('<header class="hero">');
  const heroEnd = html.indexOf('<section id="demos"');
  const hero = html.slice(heroStart, heroEnd);
  const declaration = 'A terminal TUI — reactive GitHub inbox + agent fleet, keyboard-driven.';

  assert.ok(heroStart > 0 && heroEnd > heroStart, 'expected the homepage hero');
  assert.ok(hero.includes(declaration), 'expected the explicit terminal-TUI description');
  assert.ok(hero.indexOf(declaration) < hero.indexOf(brewCommand), 'expected the terminal description before install');
  assert.ok(hero.includes('Runs in your terminal. No Electron. No tab farm.'));
  const demoIndex = hero.indexOf('src="/demo/01-inbox.mp4"');
  assert.ok(demoIndex > 0, 'expected the real TUI recording');
  assert.ok(demoIndex < hero.indexOf(brewCommand), 'expected the TUI recording before install');
  assert.ok(hero.includes('real TUI capture · inbox at scale'));
  assert.equal(hero.includes('<span class="dot r">'), false, 'hero must not use macOS window chrome');
});

test('the hero recording stays full width at desktop and tablet sizes', () => {
  const heroGrid = css.match(/\.hero-grid\s*\{([^}]*)\}/)?.[1] ?? '';
  const terminal = css.match(/\.term\s*\{([^}]*)\}/)?.[1] ?? '';

  assert.equal(
    heroGrid.includes('grid-template-columns'),
    false,
    'hero media must not be squeezed into a desktop side column',
  );
  assert.match(terminal, /width:\s*100%/, 'expected the terminal capture to use the full hero width');
});

test('hero alignment does not change the centered footer call to action', () => {
  const sharedCta = css.match(/\.cta-row\s*\{([^}]*)\}/)?.[1] ?? '';
  const heroCta = css.match(/\.hero \.cta-row\s*\{([^}]*)\}/)?.[1] ?? '';

  assert.match(sharedCta, /justify-content:\s*center/);
  assert.match(heroCta, /justify-content:\s*flex-start/);
});

test('the homepage presents the five recorded workflow demos', () => {
  const demosStart = html.indexOf('<section id="demos"');
  const demos = html.slice(demosStart, html.indexOf('</section>', demosStart));
  const clips = [
    { file: '01-inbox', label: 'inbox at scale' }, // hero, above the strip
    { file: '02-snippets', label: 'snippets' },
    { file: '03-policies', label: 'github controls' },
    { file: '04-spawn', label: 'worktree + agent' },
    { file: '05-autowork', label: 'github auto-work' },
  ];
  // The hero clip lives above the strip; the other four are in the grid.
  assert.ok(html.includes('src="/demo/01-inbox.mp4"'), 'expected the inbox hero clip');
  for (const { file, label } of clips.slice(1)) {
    assert.ok(demos.includes(`src="/demo/${file}.mp4"`), `missing demo video: ${file}`);
    assert.ok(demos.includes(`<span class="title">${label}</span>`), `missing demo label: ${label}`);
  }
});

test('every referenced demo asset ships as a non-empty file', async () => {
  const referenced = new Set(html.match(/\/demo\/[\w.-]+\.(?:mp4|jpg)/g));
  assert.ok(referenced.size > 0, 'expected the homepage to reference demo assets');
  for (const ref of referenced) {
    const bytes = await readFile(new URL(`../public${ref}`, import.meta.url));
    assert.ok(bytes.length > 0, `empty or missing demo asset: ${ref}`);
  }
});

test('demo captions render key hints in the terminal mono font', () => {
  const rule = css.match(/\.demo-item figcaption code\{([^}]*)\}/)?.[1] ?? '';
  assert.match(rule, /font-family:\s*var\(--mono\)/, 'expected the g key hint to use the mono font');
});

const demoClips = ['01-inbox', '02-snippets', '03-policies', '04-spawn', '05-autowork'];

test('every demo video carries a timestamped WebVTT captions track', () => {
  for (const clip of demoClips) {
    assert.ok(
      html.includes(`<track kind="captions" src="/demo/${clip}.vtt"`),
      `missing captions track for demo: ${clip}`,
    );
  }
});

test('each captions file ships with parseable cues inside its clip', async () => {
  const timestamp = /^(?:\d{2}:)?[0-5]\d:[0-5]\d\.\d{3}$/;
  for (const clip of demoClips) {
    const vtt = await readFile(new URL(`../public/demo/${clip}.vtt`, import.meta.url), 'utf8');
    assert.ok(vtt.startsWith('WEBVTT'), `missing WEBVTT header: ${clip}`);
    const cues = [...vtt.matchAll(/^(\S+)\s+-->\s+(\S+)/gm)];
    assert.ok(cues.length >= 3, `expected at least three cues in ${clip}, got ${cues.length}`);
    for (const [, start, end] of cues) {
      assert.match(start, timestamp, `bad cue start in ${clip}: ${start}`);
      assert.match(end, timestamp, `bad cue end in ${clip}: ${end}`);
    }
  }
});

// "MM:SS.mmm" or "HH:MM:SS.mmm" → seconds.
function cueSeconds(stamp) {
  const parts = stamp.split(':').map(Number);
  return parts.reduce((acc, n) => acc * 60 + n, 0);
}

// The playable length of an MP4's video track, read from its sample-table
// (stts) — the only duration that matches what the browser loops a <video>
// on. We walk the box tree (moov→trak→mdia→…) rather than byte-scanning for
// box names, which false-matches inside the compressed mdat; and we sum stts
// deltas rather than trusting mvhd/mdhd, whose headers can be padded past the
// real samples (VHS clips 02–05 declare ~3s more than they play).
function mp4Boxes(buf, start, end) {
  const out = [];
  let p = start;
  while (p + 8 <= end) {
    const size = buf.readUInt32BE(p);
    const type = buf.toString('latin1', p + 4, p + 8);
    let headerSize = 8;
    let boxEnd;
    if (size === 1) {
      boxEnd = p + Number(buf.readBigUInt64BE(p + 8));
      headerSize = 16;
    } else if (size === 0) {
      boxEnd = end;
    } else {
      boxEnd = p + size;
    }
    if (boxEnd <= p || boxEnd > end) break;
    out.push({ type, dataStart: p + headerSize, end: boxEnd });
    p = boxEnd;
  }
  return out;
}
function mp4Child(buf, box, type) {
  return mp4Boxes(buf, box.dataStart, box.end).find((b) => b.type === type);
}
function videoTrackSeconds(buf) {
  const moov = mp4Boxes(buf, 0, buf.length).find((b) => b.type === 'moov');
  assert.ok(moov, 'mp4 has no moov box');
  for (const trak of mp4Boxes(buf, moov.dataStart, moov.end).filter((b) => b.type === 'trak')) {
    const mdia = mp4Child(buf, trak, 'mdia');
    if (!mdia) continue;
    const hdlr = mp4Child(buf, mdia, 'hdlr');
    const mdhd = mp4Child(buf, mdia, 'mdhd');
    if (!hdlr || !mdhd) continue;
    if (buf.toString('latin1', hdlr.dataStart + 8, hdlr.dataStart + 12) !== 'vide') continue;
    const version = buf[mdhd.dataStart];
    const timescale = buf.readUInt32BE(mdhd.dataStart + 4 + (version === 1 ? 16 : 8));
    const stbl = mp4Child(buf, mp4Child(buf, mdia, 'minf') ?? mdia, 'stbl');
    const stts = stbl && mp4Child(buf, stbl, 'stts');
    assert.ok(stts, 'video track has no stts box');
    let p = stts.dataStart + 4; // version + flags
    const entries = buf.readUInt32BE(p);
    p += 4;
    let units = 0;
    for (let i = 0; i < entries; i += 1) {
      units += buf.readUInt32BE(p) * buf.readUInt32BE(p + 4);
      p += 8;
    }
    return units / timescale;
  }
  throw new Error('mp4 has no video track');
}

// The regression guard for the high-res hero swap (#1347): the video content
// was replaced but the captions weren't, leaving the 01-inbox track with a cue
// at 00:11.2 over an 8.56s clip — invisible, and narrating a keystroke the clip
// never performs. Cue text can't be checked automatically, but a cue that ends
// past the clip is an unambiguous, catchable desync.
test('no caption cue outlives its demo clip', async () => {
  for (const clip of demoClips) {
    const mp4 = await readFile(new URL(`../public/demo/${clip}.mp4`, import.meta.url));
    const duration = videoTrackSeconds(mp4);
    const vtt = await readFile(new URL(`../public/demo/${clip}.vtt`, import.meta.url), 'utf8');
    const cues = [...vtt.matchAll(/^(\S+)\s+-->\s+(\S+)/gm)];
    for (const [, , end] of cues) {
      assert.ok(
        cueSeconds(end) <= duration,
        `${clip}: caption ends at ${end} (${cueSeconds(end)}s) past the ${duration.toFixed(3)}s clip`,
      );
    }
  }
});

test('the styled caption overlay reads its cues from the same track', () => {
  for (const clip of demoClips) {
    assert.ok(html.includes(`src="/demo/${clip}.mp4"`), `missing demo video: ${clip}`);
  }
  // One overlay host per clip, plus the keycap-chip look sourced from the cue text.
  const overlays = html.match(/class="demo-overlay"/g) ?? [];
  assert.equal(overlays.length, demoClips.length, 'expected one overlay per demo clip');
  assert.ok(html.includes('cuechange'), 'overlay must be driven by the VTT track cuechange event');
  const keyRule = css.match(/\.demo-overlay-key\{([^}]*)\}/)?.[1] ?? '';
  assert.match(keyRule, /font-family:\s*var\(--mono\)/, 'expected the keycap chip in the mono font');
});

test('the caption pill clears the control bar and reads without blur', () => {
  const overlayRule = css.match(/\.demo-overlay\{([^}]*)\}/)?.[1] ?? '';
  assert.match(
    overlayRule,
    /padding:[^;}]*2\.75rem/,
    'expected extra bottom padding so the pill sits above the native control bar',
  );
  const lineRule = css.match(/\.demo-overlay-line\{([^}]*)\}/)?.[1] ?? '';
  assert.match(lineRule, /backdrop-filter:\s*blur/, 'expected the backdrop blur enhancement');
  // The fallback for browsers without backdrop-filter: a near-opaque fill keeps
  // the caption legible, so the missing blur is purely cosmetic.
  assert.match(
    lineRule,
    /background:#090c12[0-9a-f]{2}/,
    'expected an opaque fill so the caption reads without blur support',
  );
});

test('the homepage publishes the established public-safe social preview', () => {
  assert.ok(html.includes('<meta property="og:image" content="https://lazybox.ai/og.png">'));
  assert.ok(html.includes('<meta name="twitter:image" content="https://lazybox.ai/og.png">'));
  assert.ok(html.includes('<meta name="twitter:card" content="summary_large_image">'));
  assert.equal(
    createHash('sha256').update(socialPreview).digest('hex'),
    '2caa482e5167d921530c6d3e6dfa7304585713165b73aa9a64308f0bba2c5a29',
  );
});
