import { join } from 'node:path';

// A Runner launched from an agent shell owns its own account selection.
// Keep the person's normal terminal overrides, but drop the parent agent's.
export function runnerLaunchEnvironment(inherited) {
  if (!inherited.LAZYBOX_V2_SESSION_ID) return inherited;
  const env = { ...inherited };
  for (const name of ['CODEX_HOME', 'OPENAI_API_KEY', 'CODEX_API_KEY', 'CLAUDE_CONFIG_DIR', 'ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'CLAUDE_CODE_OAUTH_TOKEN']) {
    delete env[name];
  }
  return env;
}

// Both terminal and native-service launch paths use the same Codefly contract.
// Values are resolved filesystem locators, never credentials or allocated ports.
export function runnerArguments({ paths, source, snippetAdapter, snippetDirectory, subscriptionState, cliDir, restartRequest, actionsFile }) {
  const args = ['run', 'service', 'runner', '--runtime-context', 'native', '--temporary-ports', '--headless'];
  const settings = {
    LAZYBOX_V2_STATE: paths.state,
    LAZYBOX_V2_WORKTREE_ROOT: join(paths.directory, 'collections'),
    LAZYBOX_V2_CONFIG: paths.config,
    LAZYBOX_V2_WORKER: join(source, 'workers/claude-sdk/dist/main.js'),
    // The agent browser service (BROWSER.md); `make build-worker` builds it too.
    LAZYBOX_V2_BROWSER_WORKER: join(source, 'workers/browser/dist/main.js'),
    // The phone app the device gateway serves (REMOTE.md); `make build-web` builds it.
    LAZYBOX_V2_WEB_DIR: join(source, 'apps/web/dist'),
    // The documentation Ask Lazybox reads (Help → Documentation shows the same pages).
    LAZYBOX_V2_DOCS_DIR: join(source, 'docs'),
    // Persistent environments keep agents running across Runner restarts.
    LAZYBOX_V2_KEEP_AGENTS: '1',
    ...(snippetAdapter ? { LAZYBOX_V2_SNIPPET_COMMAND: snippetAdapter, LAZYBOX_V2_SNIPPET_DIRECTORY: snippetDirectory } : {}),
    ...(subscriptionState ? { LAZYBOX_V2_SUBSCRIPTION_STATE: subscriptionState } : {}),
    // Agents find `lazybox run` here; the Runner puts it first on their PATH.
    ...(cliDir ? { LAZYBOX_V2_CLI_DIR: cliDir } : {}),
    // Only a launcher that starts the Runner again after it exits names this
    // (internal/restart); the Runner then offers Restart Runner in the app.
    ...(restartRequest ? { LAZYBOX_V2_RESTART_REQUEST: restartRequest } : {}),
    // The person's actions and hooks file (POWER.md), shared by Dev and Working.
    ...(actionsFile ? { LAZYBOX_V2_ACTIONS_FILE: actionsFile } : {}),
  };
  for (const [key, value] of Object.entries(settings)) args.push('--set', `runner:${key}=${value}`);
  return args;
}
