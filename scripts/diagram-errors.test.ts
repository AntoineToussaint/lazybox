// A diagram that does not render says why, and only failures that are not the
// text's or theme's own fault are retried. The errors are Mermaid 12's own,
// captured from WebKit (scripts/test-mermaid-robust.mjs renders them live).
import test from 'node:test';
import assert from 'node:assert/strict';
import { DiagramMeasureError, DiagramTooLargeError, diagramFailure } from '../packages/surfaces/src/diagram-errors.ts';

const jison = (message: string, hash: object) => Object.assign(new Error(message), { hash });

test('a syntax error names the line and what is there, and is not retried', () => {
  const sequence = jison("Parse error on line 3:\n...  A->>B: hi  B-->>: oops\n---------------------^\nExpecting 'link', '+', '-', '()', 'ACTOR', got 'TXT'", { text: ': oops', token: 'TXT', line: 2 });
  assert.deepEqual(diagramFailure(sequence, 'sequenceDiagram\n  A->>B: hi\n  B-->>: oops'), { kind: 'syntax', message: 'Mermaid could not read line 3 near “: oops”.', retry: false });
  const unclosed = jison("Parse error on line 5:\n...  B --> C[unclosed\n---------------------^\nExpecting 'SQE', 'PE', got '1'", { text: '', token: 1, line: 4 });
  assert.equal(diagramFailure(unclosed, '').message, 'Mermaid could not read line 5: the diagram ends too early.');
  // Langium grammars (pie, packet, architecture…) word it differently.
  const langium = new Error('Parsing failed: Lexer error on line 2, column 8: unexpected character: ->x<- at offset: 19, skipped 1 characters. Parse error on line 2, column 9: Expecting token of type \'NUMBER_PIE\' but found `\n`.');
  assert.deepEqual(diagramFailure(langium, ''), { kind: 'syntax', message: 'Mermaid could not read line 2, column 8 near “x”.', retry: false });
});

test('an unknown diagram type is named from the text', () => {
  const unknown = Object.assign(new Error('No diagram type detected matching given configuration for text: sequenceDiagrm\n A->>B: x'), { name: 'UnknownDiagramError' });
  assert.deepEqual(diagramFailure(unknown, '%% a comment\nsequenceDiagrm\n A->>B: x'), { kind: 'type', message: 'Mermaid does not recognise the diagram type “sequenceDiagrm”.', retry: false });
  assert.equal(diagramFailure(unknown, '---\ntitle: Front matter\n---\ninvalid definitely not diagram').message, 'Mermaid does not recognise the diagram type “invalid”.');
  assert.equal(diagramFailure(unknown, '  \n').message, 'The diagram is empty.');
});

test('limits and theme colours are permanent and say what to change', () => {
  assert.deepEqual(diagramFailure(new DiagramTooLargeError(50000), ''), { kind: 'limit', message: 'The diagram is too large to preview (over 50,000 characters).', retry: false });
  assert.equal(diagramFailure(new Error('Edge limit exceeded. 501 edges found, but the limit is 500.\n\nInitialize mermaid with maxEdges set to a higher number'), '').message, 'The diagram is too large to preview (501 or more connections).');
  assert.deepEqual(diagramFailure(new Error('Unsupported color format: "oklch(0.7 0.1 250)"'), ''), { kind: 'colour', message: 'Colour “oklch(0.7 0.1 250)” in your theme is not supported by diagrams.', retry: false });
  assert.equal(diagramFailure(new Error('Unsupported color format: "color-mix(in srgb, var(--accent) 40%, var(--surface-raised))"'), '').message, 'Colour “color-mix(in srgb, var(--accent) 40%, v…” in your theme is not supported by diagrams.');
});

test('a renderer that did not load, or a diagram drawn hidden, is retried', () => {
  for (const message of ['Importing a module script failed.', 'Failed to fetch dynamically imported module: http://127.0.0.1:5173/node_modules/.vite/deps/mermaid.js?v=2556c480', 'error loading dynamically imported module'])
    assert.deepEqual(diagramFailure(new TypeError(message), ''), { kind: 'load', message: 'The diagram renderer could not be loaded. Restart Lazybox if this persists.', retry: true }, message);
  assert.deepEqual(diagramFailure(new DiagramMeasureError(), ''), { kind: 'measure', message: 'The diagram could not be measured while hidden. It renders when shown.', retry: true });
  assert.equal(diagramFailure(new Error('Expected length, "NaN".'), '').kind, 'measure');
  // Anything else keeps Mermaid's words; drawn while hidden, it is taken as a measurement failure.
  assert.deepEqual(diagramFailure(new TypeError("undefined is not an object (evaluating 'a.width')"), ''), { kind: 'other', message: "Mermaid could not draw this diagram: undefined is not an object (evaluating 'a.width')", retry: true });
  assert.equal(diagramFailure(new TypeError("undefined is not an object (evaluating 'a.width')"), '', { hidden: true }).kind, 'measure');
  // Hidden does not excuse the text: a syntax error stays one.
  assert.equal(diagramFailure(jison('Parse error on line 1:\nx', { text: 'x' }), '', { hidden: true }).kind, 'syntax');
  assert.equal(diagramFailure('a thrown string', '').message, 'Mermaid could not draw this diagram: a thrown string');
});
