// The person's own Lazybox configuration directory (POWER.md): the actions and
// hooks file both Dev and Working read, and the locators the `lazybox` CLI
// uses to find a running Runner from any terminal.
//
// It follows the XDG convention power users expect on every platform:
// $XDG_CONFIG_HOME/lazybox, else ~/.config/lazybox. Nothing here holds a
// credential: a locator names the private endpoint file, which stays in the
// environment's state directory (0600).
import { mkdir, rename, writeFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import { isAbsolute, join } from 'node:path';

export function lazyboxConfigDir(env = process.env) {
  const base = env.XDG_CONFIG_HOME && isAbsolute(env.XDG_CONFIG_HOME) ? env.XDG_CONFIG_HOME : join(env.HOME || homedir(), '.config');
  return join(base, 'lazybox');
}

export function userActionsFile(env = process.env) {
  return join(lazyboxConfigDir(env), 'actions.yaml');
}

/** Records where this environment's Runner writes its endpoint, so `lazybox` finds it. */
export async function registerRunner(channel, paths, env = process.env) {
  if (!['dev', 'working'].includes(channel)) throw new Error('Choose dev or working');
  const dir = join(lazyboxConfigDir(env), 'runners');
  await mkdir(dir, { recursive: true, mode: 0o700 });
  const locator = { name: channel, product: paths.productName, endpoint: paths.endpoint, state: paths.state };
  const target = join(dir, `${channel}.json`), temporary = `${target}.${process.pid}.tmp`;
  await writeFile(temporary, JSON.stringify(locator, null, 2) + '\n', { mode: 0o600 });
  await rename(temporary, target);
  return target;
}
