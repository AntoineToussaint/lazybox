import assert from 'node:assert/strict';
import test from 'node:test';
import { messageContent } from './content.ts';

test('parts become content blocks in the Runner’s order, after an instruction update', () => {
  const content = messageContent({ context: 'Updated instructions', parts: [
    { type: 'text', text: 'Look at these' },
    { type: 'image', mediaType: 'image/png', data: 'cG5n' },
    { type: 'text', text: 'Attached file "Pasted text.txt" (2 lines):\n```\na\nb\n```' },
    { type: 'text', text: 'Attached file "report.pdf" (application/pdf, 1 KiB), retained on this Runner at "/r/report.pdf": read it as needed.' },
  ] });
  assert.deepEqual(content.map(block => block.type), ['text', 'text', 'image', 'text', 'text']);
  assert.equal(content[0].text, 'Updated instructions');
  assert.equal(content[1].text, 'Look at these');
  assert.deepEqual(content[2].source, { type: 'base64', media_type: 'image/png', data: 'cG5n' });
  assert.match(content[3].text, /```\na\nb\n```/);
  assert.match(content[4].text, /report\.pdf/);
});

test('a Runner from before parts sends text and images', () => {
  const content = messageContent({ text: 'hello', images: [{ mediaType: 'image/jpeg', data: 'anBn' }] });
  assert.deepEqual(content, [{ type: 'text', text: 'hello' }, { type: 'image', source: { type: 'base64', media_type: 'image/jpeg', data: 'anBn' } }]);
});

test('an unknown part fails the message rather than dropping it', () => {
  assert.throws(() => messageContent({ parts: [{ type: 'document' } as never] }), /unknown message part/);
});
