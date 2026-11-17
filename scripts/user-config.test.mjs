import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, stat, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { lazyboxConfigDir, registerRunner, userActionsFile } from './user-config.mjs';
import { runnerArguments } from './runner-command.mjs';

test('the config directory follows XDG, else ~/.config, never a relative path', () => {
  assert.equal(lazyboxConfigDir({ HOME: '/home/me' }), '/home/me/.config/lazybox');
  assert.equal(lazyboxConfigDir({ HOME: '/home/me', XDG_CONFIG_HOME: '/xdg' }), '/xdg/lazybox');
  assert.equal(lazyboxConfigDir({ HOME: '/home/me', XDG_CONFIG_HOME: 'relative' }), '/home/me/.config/lazybox');
  assert.equal(userActionsFile({ HOME: '/home/me' }), '/home/me/.config/lazybox/actions.yaml');
});

test('a launcher passes the actions file to its Runner only when it has one', () => {
  const paths = { state: '/s', directory: '/d', config: '/c' };
  assert.ok(runnerArguments({ paths, source: '/src', actionsFile: '/home/me/.config/lazybox/actions.yaml' }).includes('runner:LAZYBOX_V2_ACTIONS_FILE=/home/me/.config/lazybox/actions.yaml'));
  assert.ok(!runnerArguments({ paths, source: '/src' }).some(value => value.includes('ACTIONS_FILE')));
});

test('a registered Runner names its private endpoint file and holds no credential', async t => {
  const home = await mkdtemp(join(tmpdir(), 'lazybox-config-'));
  t.after(() => rm(home, { recursive: true, force: true }));
  const paths = { productName: 'Lazybox Dev', endpoint: '/env/dev/state/endpoint.json', state: '/env/dev/state' };
  const file = await registerRunner('dev', paths, { HOME: home });
  assert.equal(file, join(home, '.config/lazybox/runners/dev.json'));
  assert.deepEqual(JSON.parse(await readFile(file, 'utf8')), { name: 'dev', product: 'Lazybox Dev', endpoint: paths.endpoint, state: paths.state });
  assert.equal((await stat(file)).mode & 0o777, 0o600);
  assert.equal((await stat(join(home, '.config/lazybox/runners'))).mode & 0o777, 0o700);
  await assert.rejects(registerRunner('../evil', paths, { HOME: home }), /dev or working/);
});
