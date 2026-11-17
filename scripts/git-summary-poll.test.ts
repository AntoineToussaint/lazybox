// The sidebar's git poll (git-summary-poll.ts): a sidebar that goes away and
// comes back while a request is in flight (a remount when the dock settles)
// gets the git state, instead of waiting forever on the request it aborted.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { GitSummaryPoll } from '../packages/surfaces/src/git-summary-poll.ts';

/** A Runner that answers when told to; an aborted request rejects, as fetch does. */
function runner() {
  const calls: { signal: AbortSignal; answer(): void }[] = [];
  return {
    calls,
    listWorkspaceGitSummaries(_request: unknown, { signal }: { signal: AbortSignal }) {
      return new Promise<{ workspaces: { collectionId: string }[] }>((resolve, reject) => {
        signal.addEventListener('abort', () => reject(new Error('aborted')));
        calls.push({ signal, answer: () => resolve({ workspaces: [{ collectionId: 'alpha' }] }) });
      });
    },
  };
}
const tick = () => new Promise(resolve => setImmediate(resolve));

test('a sidebar remounted while the first request is in flight still gets the git state', async () => {
  const api = runner();
  const poll = new GitSummaryPoll(api as never);
  const leave = poll.subscribe(() => {});
  assert.equal(api.calls.length, 1, 'the first sidebar asks');
  leave(); // the dock settles and remounts the sidebar
  const again = poll.subscribe(() => {});
  await tick();
  assert.equal(api.calls[0].signal.aborted, true, 'the old request was aborted');
  assert.equal(api.calls.length, 2, 'the remounted sidebar asks again at once');
  api.calls[1].answer();
  await tick();
  assert.equal(poll.snapshot.loaded, true);
  assert.deepEqual([...poll.snapshot.byWorkspace.keys()], ['alpha']);
  again();
});

test('one request at a time while a sidebar stays', async () => {
  const api = runner();
  const poll = new GitSummaryPoll(api as never);
  const leave = poll.subscribe(() => {});
  const second = poll.subscribe(() => {});
  await poll.load();
  assert.equal(api.calls.length, 1, 'a load while one is in flight does not ask twice');
  api.calls[0].answer();
  await tick();
  assert.equal(poll.snapshot.loaded, true);
  second(); leave();
});
