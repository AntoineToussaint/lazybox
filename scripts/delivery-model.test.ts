import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { DeliveryOutcome } from '@lazybox/protocol';
import { blocking, deliveryOutcomeMarkdown, evidenceLine, headline, resolved, specName, summary } from '../packages/surfaces/src/delivery-model.ts';

const criterion = (id: string, state: string, required = true, evidence: object[] = []) => ({ id, expected: `${id} works`, required, state, evidence });
const outcome = (fields: object) => ({ contract: { artifactId: 'spec-catalog', version: 3, digest: 'd', schema: 'spec.v2', criteria: [] }, attempt: 'a', conversationId: 'c', missionId: '', taskId: '', current: 'candidate:2', decision: '', candidates: [], ...fields }) as unknown as DeliveryOutcome;

test('a summary leads with the verdict on the current files, then what blocks it, worst first', () => {
  const o = outcome({ candidates: [{ candidate: 'candidate:2', current: true, verdict: 'blocked', criteria: [criterion('storage', 'passed'), criterion('api', 'unrun'), criterion('generate', 'blocked'), criterion('docs', 'failed', false)] }] });
  assert.equal(summary(o), 'Blocked · generate blocked, api not run');
  assert.deepEqual(blocking(headline(o)!).map(c => c.id), ['generate', 'api']);
});

test('acceptance and stale files are said beside the verdict, never instead of it', () => {
  const o = outcome({ decision: 'accepted', candidates: [{ candidate: 'candidate:1', current: false, verdict: 'failed', criteria: [criterion('api', 'failed')] }] });
  assert.equal(summary(o), 'Failing · api failed · on earlier files · you accepted it');
  assert.equal(summary(outcome({})), 'Nothing checked yet');
});

test('evidence says what each observation proved, and a failure a later check resolved is marked', () => {
  const failed = { id: 'run:e/1', source: 'command', reference: 'run:e/1', commandId: '1', exitCode: 1, assertion: 'failed', prerequisite: '', state: 'failed', output: 'unavailable' };
  const passed = { ...failed, id: 'run:e/2', exitCode: 0, assertion: 'passed', state: 'passed', output: 'available' };
  const c = criterion('api', 'passed', true, [failed, passed]) as never;
  assert.equal(evidenceLine(failed as never), 'failed · check · exit 1 · output no longer kept');
  assert.equal(evidenceLine({ ...failed, prerequisite: 'passed', assertion: '', state: 'unverified', exitCode: 0, output: '' } as never), 'unverified · prerequisite met · exit 0');
  assert.equal(evidenceLine({ ...passed, source: 'review', exitCode: undefined } as never), 'passed · review');
  assert.equal(evidenceLine({ ...passed, title: 'go test ./api' } as never), 'go test ./api · passed · check · exit 0');
  assert.equal(specName({ title: 'Catalog', artifactId: 'spec-catalog', version: 3 }), '“Catalog” v3');
  assert.equal(specName({ artifactId: 'spec-catalog', version: 3 }), 'spec spec-catalog v3');
  assert.equal(resolved(c, 0), true);
  assert.equal(resolved(c, 1), false);
});

test('a published verdict snapshot reads as a document per candidate', () => {
  const md = deliveryOutcomeMarkdown({ schema: 'delivery-outcome.v1', contract: { artifact: 'spec-catalog', version: 3 }, subject: { conversation: 'c', mission: 'm', task: 't' }, decision: 'accepted',
    candidates: [{ candidate: 'candidate:abcdef1234', current: true, verdict: 'blocked', criteria: [{ id: 'api', expected: 'reads | writes', required: true, state: 'unrun' }, { id: 'generate', expected: 'exists', required: false, state: 'blocked', evidence: [{ reference: 'run:e/1', source: 'command', state: 'blocked', exitCode: 1, output: 'unavailable' }] }] }] });
  const titled = deliveryOutcomeMarkdown({ schema: 'delivery-outcome.v1', contract: { artifact: 'spec-catalog', title: 'Catalog', version: 3 }, subject: { conversation: 'c' },
    candidates: [{ candidate: 'candidate:1', current: true, verdict: 'passed', criteria: [{ id: 'api', expected: 'reads', required: true, state: 'passed', evidence: [{ reference: 'run:e/2', title: 'go test ./api', source: 'command', state: 'passed', exitCode: 0 }] }] }] });
  assert.match(titled, /^Checked against “Catalog” v3/);
  assert.match(titled, /passed “go test \.\/api” \(exit 0\)/);
  assert.match(md, /^Checked against spec spec-catalog v3 · Mission task t/);
  assert.match(md, /\*\*Decision:\*\* accepted\. The technical verdict below is unchanged\./);
  assert.match(md, /## Blocked on the workspace files when published \(abcdef12\)/);
  assert.match(md, /\| \*\*api\*\*: reads \\\| writes \| not run \| — \|/);
  assert.match(md, /\| \*\*generate\*\* \(optional\): exists \| blocked \| blocked run:e\/1 \(exit 1\), output no longer kept \|/);
});
