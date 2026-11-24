import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';

// publish-cask_test.sh (copied from V1 with the publisher) is one sequential
// test: its cases share the taps the first one creates. node --test runs it so
// it has the 30 s deadline every unit test has; when the deadline cancels
// the test, its signal stops the script.
test('publish-cask.sh: first publish, idempotent re-publish, version bump, rejected push retried, formula alongside', async t => {
  const script = fileURLToPath(new URL('publish-cask_test.sh', import.meta.url));
  const { stdout } = await promisify(execFile)('bash', [script], { signal: t.signal, maxBuffer: 16 * 1024 * 1024 });
  assert.match(stdout, /OK: publish-cask\.sh regression tests passed/);
});
