import assert from 'node:assert/strict';
import { test } from 'node:test';
import { commandCore, doneMessage, explicitHomeTarget, globalLogin, homeOffers, inHome, isLogin, matchRepository, needsTerminal, placeFor, placeLabel, reviewTerminalCommand, type HomeRepository } from '../packages/surfaces/src/home-run';

const repositories: HomeRepository[] = [
  { repositoryId: 'github-1', name: 'api', identity: 'github:acme/api', home: '/u/lazybox/repos/acme/api' },
  { repositoryId: 'github-2', name: 'web', identity: 'github:acme/web', home: '' },
];

test('the briefing’s words name the place for certain', () => {
  const offers = homeOffers([
    'Done. To try it, run in the home checkout of api: `make playground`, and `make stop` when done.',
    'Then run in your home folder: `gcloud auth application-default login`',
  ].join('\n'));
  assert.deepEqual(offers, [
    { command: 'make playground', target: { kind: 'repository', repository: 'api' }, explicit: true },
    { command: 'gcloud auth application-default login', target: { kind: 'user' }, explicit: true },
  ]);
  assert.deepEqual(homeOffers('Run in the **acme/web** home: `npm run dev`')[0].target, { kind: 'repository', repository: 'acme/web' });
});

test('a fenced command carries its named destination and a compound login needs review', () => {
  const command = 'git fetch origin main && git switch main && env -u TOKEN bin/tool login prod';
  const message = ['Run in the home checkout of `api`:', '', '```sh', command, '```'].join('\n');
  assert.deepEqual(explicitHomeTarget(message, command), { kind: 'repository', repository: 'api' });
  assert.equal(explicitHomeTarget(message, 'git status'), undefined);
  assert.ok(reviewTerminalCommand(command));
  assert.ok(!reviewTerminalCommand('env -u TOKEN bin/tool login prod'));
  assert.ok(!reviewTerminalCommand('git fetch origin main && git status'));
});

test('a command handed to the person is offered; one the agent ran is not, nor one for later', () => {
  const offers = homeOffers([
    'I ran `go test ./...` and it passes.',
    'Run `acme login` first — I cannot sign in for you.',
    'To run it yourself, use `make playground` from the parser module and `make stop` when done.',
  ].join('\n'));
  assert.deepEqual(offers.map(o => [o.command, o.target.kind, o.explicit]), [
    ['acme login', 'any', false],
    ['make playground', 'any', false],
  ]);
  assert.deepEqual(homeOffers('The tests pass: `npm test` is green, and `make lint` too.'), [], 'nothing handed back, nothing offered');
});

test('the exact command is kept: environment changes, the whole line, a fenced sh block', () => {
  const text = [
    'The sign-in opens a browser and needs a real terminal (TTY), which my commands do not have. Run this yourself:',
    '',
    '```sh',
    'cd /tmp/api-main && env -u ACME_TOKEN ./bin/acme login staging --no-launch-browser',
    '```',
  ].join('\n');
  const [offer] = homeOffers(text);
  assert.equal(offer.command, 'cd /tmp/api-main && env -u ACME_TOKEN ./bin/acme login staging --no-launch-browser');
  assert.equal(offer.hint, 'api', 'the temporary checkout names its repository');
  assert.equal(placeLabel(placeFor(offer.target, repositories, offer.hint)), 'Run in api home');
  // In the home, only the cd into the temporary checkout goes; env -u and every argument stay.
  assert.deepEqual(inHome(offer.command), { command: 'env -u ACME_TOKEN ./bin/acme login staging --no-launch-browser', dropped: '/tmp/api-main' });
  assert.deepEqual(inHome('cd parser && make playground'), { command: 'cd parser && make playground', dropped: '' }, 'a folder of the checkout stays');
  // Inline, too.
  assert.equal(homeOffers('You will need to run `env -u ACME_TOKEN ./bin/acme login staging` yourself.')[0].command, 'env -u ACME_TOKEN ./bin/acme login staging');
  assert.equal(commandCore('cd x && env -u A -i B=1 C=2 ./bin/acme login'), './bin/acme login');
});

test('a fenced block after a hand-off sentence is one offer', () => {
  const text = 'You will need to run this on your machine:\n\n```sh\ngcloud auth login\ngcloud config set project demo\n```\n';
  const [offer] = homeOffers(text);
  assert.equal(offer.command, 'gcloud auth login\ngcloud config set project demo');
  assert.equal(offer.target.kind, 'user', 'a global login goes to the home folder');
  assert.deepEqual(homeOffers('```sh\nmake build\n```'), [], 'a block nobody handed over');
});

test('logins and prompts need a terminal; which belong to no repository', () => {
  assert.ok(isLogin('acme login'));
  assert.ok(isLogin('gcloud auth application-default login'));
  assert.ok(isLogin('aws sso login --profile dev'));
  assert.ok(isLogin('env -u TOKEN ./bin/acme login prod'));
  assert.ok(!isLogin('make loginpage'));
  assert.ok(globalLogin('gh auth login') && globalLogin('gcloud auth login') && globalLogin('env -u X gcloud auth login'));
  assert.ok(!globalLogin('acme login'), 'an unknown tool’s login may be the repository’s');
  assert.deepEqual(homeOffers('Please sign in: `gh auth login`')[0].target, { kind: 'user' });
  for (const script of ['acme login', 'gcloud auth print-access-token', 'acme connect --no-launch-browser', 'acme sign-in', 'make test\nacme login --profile dev'])
    assert.ok(needsTerminal(script), script);
  for (const script of ['make test', 'npm run dev', 'go test ./...', 'echo author'])
    assert.ok(!needsTerminal(script), script);
});

test('at most three, no repeats', () => {
  const text = ['Run `acme login` yourself.', 'Run `acme login` yourself again.', 'You need to run `make a`.', 'You need to run `make b`.', 'You need to run `make c`.'].join('\n');
  assert.deepEqual(homeOffers(text).map(o => o.command), ['acme login', 'make a', 'make b']);
});

test('where an offer runs', () => {
  assert.equal(matchRepository(repositories, 'ACME/API')?.repositoryId, 'github-1');
  assert.equal(matchRepository(repositories, 'web')?.repositoryId, 'github-2');
  assert.equal(matchRepository(repositories, 'billing'), undefined);
  assert.equal(placeLabel(placeFor({ kind: 'repository', repository: 'api' }, repositories)), 'Run in api home');
  assert.equal(placeLabel(placeFor({ kind: 'user' }, repositories)), 'Run in your home folder');
  assert.deepEqual(placeFor({ kind: 'repository', repository: 'billing' }, repositories), { kind: 'unknown', name: 'billing' });
  // Wherever fits: a repository the command's cd names, the first with a home, the first, else the home folder.
  assert.equal(placeLabel(placeFor({ kind: 'any' }, repositories, 'web')), 'Run in web home');
  assert.equal(placeLabel(placeFor({ kind: 'any' }, repositories)), 'Run in api home');
  assert.equal(placeLabel(placeFor({ kind: 'any' }, [repositories[1]])), 'Run in web home');
  assert.equal(placeLabel(placeFor({ kind: 'any' }, [])), 'Run in your home folder');
});

test('telling the agent it is done', () => {
  assert.equal(doneMessage('acme login staging', 0), '`acme login staging` completed successfully.');
  assert.equal(doneMessage('acme login staging'), '`acme login staging` finished.');
  assert.equal(doneMessage('make a\nmake b', 2), 'The command failed (exit 2).\n\n```sh\nmake a\nmake b\n```');
});

test('repeating the destination gives each command its own home handoff', () => {
  const commands = ['env -u ACME_TOKEN MODE=check bin/devbox status', 'bin/devbox validate'];
  const text = ['First update the home checkout to main.', ...commands.map(command => `Run in the home checkout of api: \`${command}\``)].join('\n\n');
  assert.deepEqual(homeOffers(text).map(({ command, target }) => ({ command, target })),
    commands.map(command => ({ command, target: { kind: 'repository', repository: 'api' } })));
});

test('an explicit home handoff preserves a long shell expression exactly', () => {
  const line = "bash -o pipefail -c 'for root in alpha beta; do " +
    'printf "%s" "$root" | jq -e --arg label "keep  both  spaces" ' +
    '"[.resources[] | select((.instances | length) > 0)]" || exit 1; ' +
    ': "' + 'long command '.repeat(20) + '"; done' + "'";
  assert.deepEqual(homeOffers('Run in the home checkout of infra-base: `' + line + '`'),
    [{ command: line, target: { kind: 'repository', repository: 'infra-base' }, explicit: true }]);
  assert.deepEqual(homeOffers('I ran `' + line + '` yesterday.'), []);
});
