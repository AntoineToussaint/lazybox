/** One piece of a message as the Runner sends it, in order (claude.go wirePart). */
export type Part = { type: 'text'; text: string } | { type: 'image'; mediaType: string; data: string };
export type MessageParams = { context?: string; text?: string; images?: { mediaType: string; data: string }[]; parts?: Part[] };

/**
 * A message's Claude content blocks. An instruction update from Lazybox goes
 * first, as its own block. Then the parts, in the Runner's order: the
 * person's text, then each attachment (an image block, or text: a file's
 * fenced text or the path of a file Claude reads itself). A Runner from
 * before parts sends text and images instead.
 */
export function messageContent(p: MessageParams): any[] {
  const content: any[] = [];
  if (p.context) content.push({ type: 'text', text: p.context });
  if (Array.isArray(p.parts)) {
    for (const part of p.parts) {
      if (part?.type === 'text' && typeof part.text === 'string') { if (part.text) content.push({ type: 'text', text: part.text }); }
      else if (part?.type === 'image' && typeof part.data === 'string' && typeof part.mediaType === 'string') content.push(image(part));
      else throw new Error(`unknown message part: ${JSON.stringify((part as { type?: unknown } | undefined)?.type)}`);
    }
    return content;
  }
  if (p.text) content.push({ type: 'text', text: p.text });
  for (const img of p.images || []) content.push(image(img));
  return content;
}

const image = (img: { mediaType: string; data: string }) => ({ type: 'image', source: { type: 'base64', media_type: img.mediaType, data: img.data } });
