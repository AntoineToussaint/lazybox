import { join } from 'node:path';

// Both terminal and native-service launch paths use the same Codefly contract.
// Values are resolved filesystem locators, never credentials or allocated ports.
export function runnerArguments({ paths, source, snippetAdapter, snippetDirectory, subscriptionState }) {
  const args = ['run', 'service', 'runner', '--runtime-context', 'native', '--temporary-ports', '--headless'];
  const settings = {
    LAZYBOX_V2_STATE: paths.state,
    LAZYBOX_V2_WORKTREE_ROOT: join(paths.directory, 'collections'),
    LAZYBOX_V2_CONFIG: paths.config,
    LAZYBOX_V2_WORKER: join(source, 'workers/claude-sdk/dist/main.js'),
    // Persistent environments keep agents running across Runner restarts.
    LAZYBOX_V2_KEEP_AGENTS: '1',
    ...(snippetAdapter ? { LAZYBOX_V2_SNIPPET_COMMAND: snippetAdapter, LAZYBOX_V2_SNIPPET_DIRECTORY: snippetDirectory } : {}),
    ...(subscriptionState ? { LAZYBOX_V2_SUBSCRIPTION_STATE: subscriptionState } : {}),
  };
  for (const [key, value] of Object.entries(settings)) args.push('--set', `runner:${key}=${value}`);
  return args;
}
