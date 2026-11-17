// Rendering Mermaid text to a sized SVG image, one diagram at a time. The
// viewer is MermaidDiagram.tsx; the failure messages are diagram-errors.ts.
import type { MermaidConfig } from 'mermaid';
import { sizedSvg, type Rendered } from './diagram-svg';
import { DiagramMeasureError, DiagramTooLargeError } from './diagram-errors';

export const MAX_DIAGRAM_TEXT = 50000;
type Mermaid = typeof import('mermaid')['default'];

let loading: Promise<Mermaid> | undefined;
/** Mermaid, loaded once it is first needed. A failed load is not kept: the next diagram tries again. */
function loadMermaid(): Promise<Mermaid> {
  return loading ??= import('mermaid').then(({ default: mermaid }) => mermaid, error => { loading = undefined; throw error; });
}

let host: HTMLDivElement | undefined;
/**
 * Where Mermaid draws before the SVG is serialised: attached and laid out,
 * with a real width, but off screen and invisible. Sequence diagrams measure
 * their text as they draw (getBBox, getComputedTextLength), and that needs a
 * box that is rendered even when the conversation holding the diagram is in a
 * background tab, a folded group or a window not yet shown.
 */
function measuringHost(): HTMLDivElement {
  if (host?.isConnected) return host;
  host = document.createElement('div');
  host.className = 'mermaid-measure';
  host.setAttribute('aria-hidden', 'true');
  Object.assign(host.style, { position: 'fixed', left: '-10000px', top: '0', width: '1600px', visibility: 'hidden', pointerEvents: 'none', overflow: 'hidden' });
  document.body.appendChild(host);
  return host;
}

let queue: Promise<unknown> = Promise.resolve();
let sequence = 0;
/**
 * Mermaid has one configuration for the whole window, and initialize() is how
 * a diagram gets the window's theme: an initialize/render pair must never
 * interleave with another, so every render waits for the one before it.
 */
function serialized<T>(job: () => Promise<T>): Promise<T> {
  const run = queue.then(job, job);
  queue = run.catch(() => undefined);
  return run;
}

/** The diagram `text` drawn with `themeVariables`, sized for zooming; rejects with why it could not be. */
export function renderMermaid(text: string, themeVariables: Record<string, unknown>): Promise<Rendered> {
  if (text.length > MAX_DIAGRAM_TEXT) return Promise.reject(new DiagramTooLargeError(MAX_DIAGRAM_TEXT));
  return serialized(async () => {
    const mermaid = await loadMermaid();
    const config: MermaidConfig = { startOnLoad: false, securityLevel: 'strict', secure: ['securityLevel', 'startOnLoad', 'maxTextSize', 'maxEdges', 'suppressErrorRendering', 'htmlLabels', 'flowchart'], theme: 'base', themeVariables, htmlLabels: false, flowchart: { htmlLabels: false }, maxTextSize: MAX_DIAGRAM_TEXT, maxEdges: 500, suppressErrorRendering: true };
    mermaid.initialize(config);
    const box = measuringHost();
    try {
      // A fresh id per render: Mermaid removes any element that already has it.
      const { svg } = await mermaid.render(`lazybox-mermaid-${++sequence}`, text, box);
      const sized = sizedSvg(svg);
      if (!sized) throw new DiagramMeasureError();
      return { ...sized, text };
    } finally { box.replaceChildren(); }
  });
}
