import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const html = await readFile(new URL('../dist/index.html', import.meta.url), 'utf8');

const brewCommand = 'brew install AntoineToussaint/lazybox/lazybox';
const installerCommand =
  "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/AntoineToussaint/lazybox/releases/latest/download/lazybox-tui-installer.sh | sh";
const sourceCommand =
  'cargo install --git https://github.com/AntoineToussaint/lazybox --locked lazybox-tui-boot';

test('the homepage prioritizes prebuilt installs over the source build', () => {
  const detailsIndex = html.indexOf('<details class="install-more">');
  const brewIndex = html.indexOf(brewCommand);
  const installerIndex = html.indexOf('Installer script <span>Prebuilt</span>');
  const sourceIndex = html.indexOf('Advanced / from source');

  assert.ok(detailsIndex > 0, 'expected alternate install methods to be disclosed');
  assert.ok(brewIndex > 0 && brewIndex < detailsIndex, 'expected Homebrew before alternate methods');
  assert.ok(installerIndex > detailsIndex, 'expected the prebuilt installer in alternate methods');
  assert.ok(sourceIndex > installerIndex, 'expected the source build to be the last method');
  assert.equal(
    html.slice(0, detailsIndex).includes(sourceCommand),
    false,
    'source install must not appear as the primary command',
  );
  assert.ok(
    html.includes('Compiles the current main branch (HEAD) locally. Requires Rust 1.88+'),
    'expected the source build requirements',
  );
});

test('every install method has a copy control for its exact command', () => {
  for (const command of [brewCommand, installerCommand, sourceCommand]) {
    assert.ok(html.includes(`data-copy="${command}"`), `missing copy control for: ${command}`);
  }
});

test('the hero presents lazybox as a terminal TUI before any install choice', () => {
  const heroStart = html.indexOf('<header class="hero">');
  const heroEnd = html.indexOf('<div class="strip">');
  const hero = html.slice(heroStart, heroEnd);
  const declaration = 'A terminal TUI — reactive GitHub inbox + agent fleet, keyboard-driven.';

  assert.ok(heroStart > 0 && heroEnd > heroStart, 'expected the homepage hero');
  assert.ok(hero.includes(declaration), 'expected the explicit terminal-TUI description');
  assert.ok(hero.indexOf(declaration) < hero.indexOf(brewCommand), 'expected the terminal description before install');
  assert.ok(hero.includes('Runs in your terminal. No Electron. No tab farm.'));
  assert.ok(hero.includes('src="/demo/lazybox.mp4"'), 'expected the real TUI recording');
  assert.ok(hero.includes('real TUI capture · 14s loop'));
  assert.equal(hero.includes('<span class="dot r">'), false, 'hero must not use macOS window chrome');
});
