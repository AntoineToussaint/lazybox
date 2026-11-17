// Pure helpers for rendered Mermaid SVG; the viewer lives in MermaidDiagram.tsx.
export type Size = { width: number; height: number };
export type Rendered = Size & { text: string; svg: string; src: string };

/**
 * Mermaid draws `width="100%"` with a max-width style, which leaves an image
 * without a natural size. Pin the root to its viewBox so zoom has a real 100%.
 */
export function sizedSvg(svg: string): Rendered | undefined {
  const root = /<svg\b[^>]*>/.exec(svg)?.[0];
  if (!root) return;
  const box = /viewBox="\s*[-\d.e]+[\s,]+[-\d.e]+[\s,]+([\d.e]+)[\s,]+([\d.e]+)\s*"/.exec(root);
  // Without a viewBox, fixed pixel dimensions still give a natural size; it is added so the image scales.
  const fixed = !box && [/\swidth="([\d.]+)(?:px)?"/.exec(root)?.[1], /\sheight="([\d.]+)(?:px)?"/.exec(root)?.[1]];
  const width = Number(box ? box[1] : fixed && fixed[0]), height = Number(box ? box[2] : fixed && fixed[1]);
  if (!(width > 0 && height > 0)) return;
  const pinned = root.replace(/\s(width|height)="[^"]*"/g, '').replace(/max-width:\s*[\d.]+px;?/, '').replace(/^<svg/, `<svg width="${width}" height="${height}"${box ? '' : ` viewBox="0 0 ${width} ${height}"`}`);
  const out = svg.replace(root, pinned);
  return { text: '', svg: out, src: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(out)}`, width, height };
}

