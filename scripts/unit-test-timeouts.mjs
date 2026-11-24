// Every unit test finishes within 30 seconds (AGENTS.md, RELEASE_GATES.md), and
// each suite enforces it per test:
//
//   node --test         --test-timeout=30000 on every invocation
//   Go                  scripts/go-test-guard.mjs, never a bare `go test`
//   Rust                cargo nextest with .config/nextest.toml (slow-timeout
//                       30 s, terminate-after 1), never `cargo test`
//
// unitTestTimeoutViolations() reads the places tests are started from (npm
// scripts, the Makefile, the CI workflows, the scripts under scripts/, the
// nextest config) and says what
// breaks the rule. The release gate catalog refuses to load while anything
// does (release-gates.mjs), so the rule cannot erode without CI and staging
// failing first.
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

/** The longest any unit test may run, in every suite. */
export const UNIT_TEST_TIMEOUT_MS = 30_000;
export const NODE_TEST_TIMEOUT_FLAG = `--test-timeout=${UNIT_TEST_TIMEOUT_MS}`;
export const NEXTEST_CONFIG = '.config/nextest.toml';

/**
 * Make targets that run Go tests that are not unit tests, so they keep a bare
 * `go test` with their own package deadline. Anything else goes through the guard.
 */
export const NOT_UNIT_GO_TARGETS = {
  'test-real': 'real providers: needs a provider login',
  'test-personality': 'real providers: needs a provider login',
  'test-real-dogfood': 'real providers: needs a provider login',
  'test-real-outlive': 'real providers: needs a provider login',
  'release-smoke': 'smoke test of a built release app (RELEASING.md), not a unit test',
};

/** Scripts that run Go tests that are not unit tests, the same way. */
export const NOT_UNIT_GO_SCRIPTS = {
  'scripts/release/build.mjs': 'the release smoke test of the built app (TestRelease, as make release-smoke)',
  'scripts/test-mobile-simulator.mjs': 'serves the iOS Simulator gateway for minutes; test:mobile-simulator is external in the catalog',
};

// One shell command at a time: `a && b | c; d` is four.
const commands = line => line.split(/&&|\|\||[;|]/).map(part => part.trim()).filter(Boolean);
const words = command => command.split(/\s+/);
const runsNodeTest = command => {
  const all = words(command), node = all.findIndex(word => /(^|\/)node$/.test(word));
  return node >= 0 && all.slice(node + 1).includes('--test');
};
// The program, however it is named (`go`, `/usr/local/go/bin/go`, `$GO`,
// `$$GO`, `$(GO)`, `"${GO}"`), then its global flags, then the subcommand.
const program = name => new RegExp(`^["']?((\\S*/)?${name}|\\$\\$?\\{?${name.toUpperCase()}\\}?|\\$\\(${name.toUpperCase()}\\))["']?$`);
const runs = (name, subcommand, skip) => command => {
  const all = words(command);
  for (let at = all.findIndex(word => program(name).test(word)); at >= 0 && at < all.length; ) {
    let next = at + 1;
    while (next < all.length && skip(all[next])) next += all[next] === '-C' ? 2 : 1;
    if (all[next] === subcommand) return true;
    const again = all.slice(at + 1).findIndex(word => program(name).test(word));
    at = again < 0 ? -1 : at + 1 + again;
  }
  return false;
};
// go's one global flag is -C <dir> (or -C=<dir>); cargo's are +toolchain and --flags.
const runsGoTest = runs('go', 'test', word => word === '-C' || word.startsWith('-C='));
const runsCargoTest = runs('cargo', 'test', word => word.startsWith('+') || word.startsWith('--'));
// A test file run directly rather than by a runner with a per-test deadline.
const testFile = word => /(_test\.sh|\.test\.(mjs|cjs|js|ts))["']?$/.test(word);
const runsNextest = command => /(^|\s)cargo\s+nextest\s+run(\s|$)/.test(command);
const usesGuard = command => /go-test-guard\.mjs\b/.test(command);

const clip = command => command.length > 100 ? `${command.slice(0, 100)}…` : command;

/** What is wrong with one shell command line, `where` naming it in the message. */
function commandViolations(line, where) {
  const found = [];
  for (const full of commands(line)) {
    const command = clip(full);
    if (runsNodeTest(full) && !words(full).includes(NODE_TEST_TIMEOUT_FLAG)) found.push(`${where}: node --test without ${NODE_TEST_TIMEOUT_FLAG}: ${command}`);
    if (runsCargoTest(full)) found.push(`${where}: cargo test has no per-test deadline; use cargo nextest run --config-file ${NEXTEST_CONFIG}: ${command}`);
    if (runsNextest(full) && !/--config-file\s+(\$\(abspath\s+)?\S*\.config\/nextest\.toml\)?(\s|$)/.test(full)) found.push(`${where}: cargo nextest run without --config-file ${NEXTEST_CONFIG}: ${command}`);
    if (!runsNodeTest(full) && words(full).some(testFile)) found.push(`${where}: a test file run outside node --test ${NODE_TEST_TIMEOUT_FLAG}, so its tests have no per-test deadline: ${command}`);
    if (usesGuard(full) && /--limit-ms/.test(full)) found.push(`${where}: the Go guard's deadline is ${UNIT_TEST_TIMEOUT_MS / 1000} s; --limit-ms is for its own tests: ${command}`);
  }
  return found;
}

/** npm scripts: every node --test carries the deadline; no bare go test or cargo test. */
export function scriptViolations(scripts) {
  const found = [];
  for (const [name, body] of Object.entries(scripts || {})) {
    found.push(...commandViolations(body, `package.json ${name}`));
    if (commands(body).some(runsGoTest)) found.push(`package.json ${name}: go test without scripts/go-test-guard.mjs`);
  }
  return found;
}

/**
 * The Makefile: the same, and a `go test` recipe line belongs to a target in
 * NOT_UNIT_GO_TARGETS; unit tests run through the guard. Variables a recipe
 * uses (`$(NEXTEST)`) are expanded first.
 */
export function makefileViolations(text) {
  const found = [], variables = {};
  const lines = text.replace(/\\\n/g, ' ').split('\n');
  for (const line of lines) {
    const assignment = /^([A-Z_]+)\s*[:?]?=\s*(.*)$/.exec(line);
    if (assignment) variables[assignment[1]] = assignment[2];
  }
  const expand = value => value.replace(/\$\(([A-Z_]+)\)/g, (whole, name) => name in variables ? expand(variables[name]) : whole);
  let targets = [];
  for (const line of lines) {
    if (!line.startsWith('\t')) {
      const rule = /^([^\s:#=][^:=]*?)\s*:(?!=)/.exec(line);
      if (rule && !line.startsWith('.')) targets = rule[1].split(/\s+/);
      continue;
    }
    const recipe = expand(line.trim().replace(/^[@-]+/, ''));
    const where = `Makefile ${targets.join(' ')}`;
    found.push(...commandViolations(recipe, where));
    if (commands(recipe).some(runsGoTest) && !targets.every(target => target in NOT_UNIT_GO_TARGETS)) found.push(`${where}: go test without scripts/go-test-guard.mjs (a unit test target), or list the target in NOT_UNIT_GO_TARGETS with why it is not one`);
  }
  return found;
}

/** A workflow: its `run:` lines start no test outside make and npm, which carry the rule. */
export function workflowViolations(text, name) {
  const found = [];
  for (const raw of text.split('\n')) {
    const line = raw.replace(/^\s*(-\s*)?(run:\s*)?/, '');
    if (!line || line.startsWith('#')) continue;
    found.push(...commandViolations(line, `${name}`));
    if (commands(line).some(runsGoTest)) found.push(`${name}: go test without scripts/go-test-guard.mjs: ${line}`);
  }
  return found;
}

/**
 * The scripts under scripts/ that start tests themselves (stress.mjs, a shell
 * harness): JavaScript that spawns `go test`, `cargo test` or `node --test`
 * without the deadline, and shell lines, as in the Makefile. Test files are
 * left out (their fixtures quote commands) and so is the guard, which is the
 * one place `go test` runs.
 */
export function scriptFileViolations(files) {
  const found = [];
  const quoted = word => `['"\`]${word}['"\`]`;
  const spawnsGoTest = new RegExp(`${quoted('go')}\\s*,\\s*\\[\\s*(${quoted('-C')}\\s*,\\s*[^,\\]]+,\\s*)?${quoted('test')}`);
  const spawnsCargoTest = new RegExp(`${quoted('cargo')}\\s*,\\s*\\[\\s*${quoted('test')}`);
  for (const [path, text] of Object.entries(files)) {
    if (/\.test\.[cm]?[jt]s$|_test\.(sh|go)$/.test(path) || path === 'scripts/go-test-guard.mjs') continue;
    if (path.endsWith('.sh')) {
      for (const line of text.split('\n')) {
        if (/^\s*#/.test(line)) continue;
        found.push(...commandViolations(line, path));
        if (commands(line).some(runsGoTest)) found.push(`${path}: go test without scripts/go-test-guard.mjs: ${clip(line.trim())}`);
      }
      continue;
    }
    if (spawnsGoTest.test(text) && !(path in NOT_UNIT_GO_SCRIPTS)) found.push(`${path}: spawns go test without scripts/go-test-guard.mjs`);
    if (spawnsCargoTest.test(text)) found.push(`${path}: spawns cargo test, which has no per-test deadline; use cargo nextest run --config-file ${NEXTEST_CONFIG}`);
    if (new RegExp(quoted('--test')).test(text) && !text.includes(NODE_TEST_TIMEOUT_FLAG)) found.push(`${path}: spawns node --test without ${NODE_TEST_TIMEOUT_FLAG}`);
  }
  return found;
}

/** Every script file under `dir`, by path relative to `root`. */
function scriptFiles(root, dir = 'scripts', files = {}) {
  if (!existsSync(join(root, dir))) return files;
  for (const entry of readdirSync(join(root, dir), { withFileTypes: true })) {
    const path = `${dir}/${entry.name}`;
    if (entry.isDirectory()) { if (entry.name !== 'node_modules') scriptFiles(root, path, files); }
    else if (/\.(mjs|cjs|js|ts|sh)$/.test(entry.name)) files[path] = readFileSync(join(root, path), 'utf8');
  }
  return files;
}

/** The nextest config: the default profile ends a test at 30 s; the version CI installs is the one it requires. */
export function nextestViolations(toml, workflows = {}) {
  if (toml === undefined) return [`${NEXTEST_CONFIG} is missing`];
  const found = [];
  const sections = toml.split(/^(?=\[)/m);
  const profile = sections.find(section => section.startsWith('[profile.default]')) || '';
  const slow = /^slow-timeout\s*=\s*\{\s*period\s*=\s*"(\d+)s"\s*,\s*terminate-after\s*=\s*(\d+)\s*\}/m.exec(profile);
  if (!slow || Number(slow[1]) * 1000 !== UNIT_TEST_TIMEOUT_MS || slow[2] !== '1') found.push(`${NEXTEST_CONFIG}: [profile.default] needs slow-timeout = { period = "${UNIT_TEST_TIMEOUT_MS / 1000}s", terminate-after = 1 }`);
  const version = nextestVersion(toml);
  if (!version) found.push(`${NEXTEST_CONFIG}: nextest-version = { required = "<x.y.z>" } pins the version CI installs`);
  for (const [name, text] of Object.entries(workflows)) {
    const installed = /^\s*NEXTEST_VERSION:\s*"?([\d.]+)"?\s*$/m.exec(text)?.[1];
    if (installed && installed !== version) found.push(`${name}: installs cargo-nextest ${installed}, ${NEXTEST_CONFIG} requires ${version}`);
  }
  return found;
}

/** The cargo-nextest version .config/nextest.toml requires. */
export function nextestVersion(toml) {
  return /^nextest-version\s*=\s*\{\s*required\s*=\s*"([\d.]+)"/m.exec(toml || '')?.[1];
}

/** Everything in the tree at `root` that breaks the rule; empty when it holds. */
export function unitTestTimeoutViolations(root) {
  const read = path => existsSync(join(root, path)) ? readFileSync(join(root, path), 'utf8') : undefined;
  const found = scriptViolations(JSON.parse(read('package.json') || '{}').scripts);
  const makefile = read('Makefile') || '';
  found.push(...makefileViolations(makefile));
  const workflows = {};
  const dir = join(root, '.github/workflows');
  if (existsSync(dir)) for (const file of readdirSync(dir).filter(file => /\.ya?ml$/.test(file)).sort()) workflows[`.github/workflows/${file}`] = read(`.github/workflows/${file}`);
  for (const [name, text] of Object.entries(workflows)) found.push(...workflowViolations(text, name));
  found.push(...scriptFileViolations(scriptFiles(root)));
  if (/cargo\s+nextest/.test(makefile) || existsSync(join(root, NEXTEST_CONFIG))) found.push(...nextestViolations(read(NEXTEST_CONFIG), workflows));
  return found;
}
