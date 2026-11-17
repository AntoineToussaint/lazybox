import { create } from '@bufbuild/protobuf';
import { ReviewCommentSchema } from '@lazybox/protocol';
import { readFileSync, writeFileSync } from 'node:fs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { reviewMessage } from '../packages/surfaces/src/review-model.ts';

// One message, two writers: the tray (reviewMessage) and the Runner's loop
// (reviews.FindingsMessage) must put exactly the same text in front of the
// author. The Go test reads the same file. `UPDATE_GOLDEN=1` rewrites it.
const file = new URL('../services/runner/code/internal/runner/reviews/testdata/findings-message.json', import.meta.url);
type Golden = { comments: Record<string, unknown>[]; message: string };

test('the tray and the Runner send findings in the same words', () => {
  const golden = JSON.parse(readFileSync(file, 'utf8')) as Golden;
  const comments = golden.comments.map(({ createdAt, ...c }) => create(ReviewCommentSchema, { ...c, createdAtUnix: BigInt(createdAt as number) }));
  const message = reviewMessage(comments);
  if (process.env.UPDATE_GOLDEN) writeFileSync(file, JSON.stringify({ ...golden, message }, null, 2) + '\n');
  else assert.equal(message, golden.message);
});
