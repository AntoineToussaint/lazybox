// Why a Mermaid diagram did not render, in words a reader can act on. Pure:
// the viewer is MermaidDiagram.tsx, the rendering mermaid-render.ts.

/**
 * - syntax, type, limit: the text itself; permanent until it changes.
 * - colour: the theme; permanent until the theme changes.
 * - load, measure, other: the moment; worth another attempt.
 */
export type DiagramFailureKind = 'syntax' | 'type' | 'limit' | 'colour' | 'load' | 'measure' | 'other';
export type DiagramFailure = { kind: DiagramFailureKind; message: string; retry: boolean };

/** Thrown for a diagram that rendered without a usable size, or was drawn where nothing can be measured. */
export class DiagramMeasureError extends Error {
  constructor(message = 'The rendered diagram has no size.') { super(message); this.name = 'DiagramMeasureError'; }
}
/** Thrown before Mermaid is asked: the text is past what a preview draws. */
export class DiagramTooLargeError extends Error {
  readonly limit: number;
  constructor(limit: number) { super(`Diagram is over ${limit} characters.`); this.name = 'DiagramTooLargeError'; this.limit = limit; }
}

const MEASURE = 'The diagram could not be measured while hidden. It renders when shown.';
const PERMANENT = new Set<DiagramFailureKind>(['syntax', 'type', 'limit', 'colour']);
const quote = (text: string, max = 40) => { const one = text.replace(/\s+/g, ' ').trim(); return `“${one.length > max ? `${one.slice(0, max - 1)}…` : one}”`; };
const firstLine = (text: string) => text.split('\n').map(line => line.trim()).find(Boolean) ?? '';

function message(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === 'string') return error;
  if (error && typeof error === 'object' && 'message' in error && typeof error.message === 'string') return error.message;
  return String(error);
}

function describe(error: unknown, text: string): { kind: DiagramFailureKind; message: string } {
  const raw = message(error);
  if (error instanceof DiagramTooLargeError) return { kind: 'limit', message: `The diagram is too large to preview (over ${error.limit.toLocaleString('en-US')} characters).` };
  if (error instanceof DiagramMeasureError) return { kind: 'measure', message: MEASURE };
  // khroma, Mermaid's colour parser: `Unsupported color format: "oklch(…)"`.
  const colour = /Unsupported color format:\s*"([^"]*)"/i.exec(raw);
  if (colour) return { kind: 'colour', message: `Colour ${quote(colour[1])} in your theme is not supported by diagrams.` };
  // Mermaid's Langium grammars (pie, packet, architecture…): "Parsing failed: Lexer error on line 2, column 8: unexpected character: ->x<- …".
  const langium = /^Parsing failed:[^]*?on line (\d+)(?:, column (\d+))?/i.exec(raw);
  if (langium) {
    const character = /unexpected character: ->(.+?)<-/.exec(raw)?.[1];
    return { kind: 'syntax', message: `Mermaid could not read line ${langium[1]}${langium[2] ? `, column ${langium[2]}` : ''}${character ? ` near ${quote(character)}` : ''}.` };
  }
  // Mermaid's jison grammars: "Parse error on line 3:\n…\nExpecting 'A', 'B', got 'TXT'".
  const jison = /Parse error on line (\d+)/i.exec(raw);
  const hash = error && typeof error === 'object' && 'hash' in error ? (error as { hash?: { text?: unknown } }).hash : undefined;
  if (jison) {
    const near = typeof hash?.text === 'string' && hash.text.trim() ? ` near ${quote(hash.text)}` : /got 'EOF'|got '1'$/m.test(raw) ? ': the diagram ends too early' : '';
    return { kind: 'syntax', message: `Mermaid could not read line ${jison[1]}${near}.` };
  }
  if ((error instanceof Error && error.name === 'UnknownDiagramError') || /No diagram type detected/i.test(raw)) {
    const head = firstLine(text.replace(/^---[^]*?---/, '').replace(/%%.*$/gm, ''));
    return { kind: 'type', message: head ? `Mermaid does not recognise the diagram type ${quote(head.split(/\s/)[0], 30)}.` : 'The diagram is empty.' };
  }
  if (/(Edge|node) limit exceeded|Maximum text size/i.test(raw)) {
    const edges = /(\d+) edges found/i.exec(raw);
    return { kind: 'limit', message: edges ? `The diagram is too large to preview (${edges[1]} or more connections).` : 'The diagram is too large to preview.' };
  }
  // A lazily loaded part of the renderer did not arrive: the app's files are
  // unreachable or replaced (WebKit, Chromium and Firefox say it differently).
  if (/Importing a module script failed|dynamically imported module|Loading (CSS )?chunk|module script|Outdated Optimize Dep/i.test(raw)) {
    return { kind: 'load', message: 'The diagram renderer could not be loaded. Restart Lazybox if this persists.' };
  }
  if (/getBBox|getComputedTextLength|NaN|Infinity|has no size|could not be measured/i.test(raw)) return { kind: 'measure', message: MEASURE };
  return { kind: 'other', message: `Mermaid could not draw this diagram: ${firstLine(raw).slice(0, 160) || 'unknown error'}` };
}

/**
 * What went wrong rendering `text`, for the reader. `hidden`: the figure had
 * no size or the window was not visible when it failed, so a failure that is
 * not the text's own fault is taken as a measurement one and retried when shown.
 */
export function diagramFailure(error: unknown, text: string, { hidden = false }: { hidden?: boolean } = {}): DiagramFailure {
  let { kind, message } = describe(error, text);
  if (hidden && kind === 'other') { kind = 'measure'; message = MEASURE; }
  return { kind, message, retry: !PERMANENT.has(kind) };
}
