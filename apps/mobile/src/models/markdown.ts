/**
 * Enough Markdown for an agent's messages on a phone: paragraphs, headings,
 * lists, quotes, fenced code, and inline code, bold and italics. Pure: native
 * Text components draw the blocks (ui/Markdown.tsx). Tables, images and HTML
 * show as their text; the computer has the full renderer.
 */
export type Span = { text: string; code?: boolean; bold?: boolean; italic?: boolean };
export type Block =
  | { kind: 'paragraph'; spans: Span[] }
  | { kind: 'heading'; level: number; spans: Span[] }
  | { kind: 'item'; marker: string; depth: number; spans: Span[] }
  | { kind: 'quote'; spans: Span[] }
  | { kind: 'code'; text: string; language: string }
  | { kind: 'rule' };

export function inlineSpans(text: string): Span[] {
  const spans: Span[] = [];
  const pattern = /(`+)([\s\S]*?)\1|\*\*([^*]+)\*\*|__([^_]+)__|(?<![\w*])\*([^*\s][^*]*?)\*(?!\w)|(?<![\w_])_([^_\s][^_]*?)_(?![\w])/g;
  let last = 0;
  for (const match of text.matchAll(pattern)) {
    if (match.index > last) spans.push({ text: text.slice(last, match.index) });
    if (match[1]) spans.push({ text: match[2], code: true });
    else if (match[3] ?? match[4]) spans.push({ text: (match[3] ?? match[4])!, bold: true });
    else spans.push({ text: (match[5] ?? match[6])!, italic: true });
    last = match.index + match[0].length;
  }
  if (last < text.length) spans.push({ text: text.slice(last) });
  return spans.filter(span => span.text.length > 0);
}

export function markdownBlocks(source: string): Block[] {
  const blocks: Block[] = [];
  const lines = source.replace(/\r\n?/g, '\n').split('\n');
  let paragraph: string[] = [];
  const flush = () => { if (paragraph.length) blocks.push({ kind: 'paragraph', spans: inlineSpans(paragraph.join(' ')) }); paragraph = []; };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const fence = /^\s*(```+|~~~+)\s*([\w+-]*)/.exec(line);
    if (fence) {
      flush();
      const body: string[] = [];
      for (i++; i < lines.length && !lines[i].trimStart().startsWith(fence[1]); i++) body.push(lines[i]);
      blocks.push({ kind: 'code', text: body.join('\n'), language: fence[2] || '' });
      continue;
    }
    if (!line.trim()) { flush(); continue; }
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) { flush(); blocks.push({ kind: 'heading', level: heading[1].length, spans: inlineSpans(heading[2].replace(/\s+#+\s*$/, '')) }); continue; }
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) { flush(); blocks.push({ kind: 'rule' }); continue; }
    const item = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/.exec(line);
    if (item) {
      flush();
      const marker = /\d/.test(item[2]) ? item[2].replace(')', '.') : '•';
      blocks.push({ kind: 'item', marker, depth: Math.min(3, Math.floor(item[1].replace(/\t/g, '  ').length / 2)), spans: inlineSpans(item[3].replace(/^\[([ xX])\]\s+/, (_, done) => done === ' ' ? '☐ ' : '☑ ')) });
      continue;
    }
    const quote = /^\s*>\s?(.*)$/.exec(line);
    if (quote) { flush(); blocks.push({ kind: 'quote', spans: inlineSpans(quote[1]) }); continue; }
    paragraph.push(line.trim());
  }
  flush();
  return blocks;
}
