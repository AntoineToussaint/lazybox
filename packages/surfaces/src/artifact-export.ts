import { createContext } from 'react';

/** The native app saves through a system dialog. The browser downloads a Blob. */
export type SaveArtifactExport = (filename: string, content: string) => Promise<boolean>;
export const ArtifactExportContext = createContext<SaveArtifactExport | undefined>(undefined);

export type DocumentHeading = { title: string; eyebrow: string; meta: string[] };

export function documentSnapshot({ title, eyebrow, meta }: DocumentHeading, rendered: HTMLElement) {
  const heading = document.createElement('header');
  const kind = document.createElement('p');
  kind.className = 'print-eyebrow';
  kind.textContent = eyebrow;
  const h1 = document.createElement('h1');
  h1.textContent = title;
  const details = document.createElement('p');
  details.className = 'print-meta';
  details.textContent = meta.join('  ·  ');
  heading.append(kind, h1, details);

  const body = rendered.cloneNode(true) as HTMLElement;
  body.querySelectorAll('button, [role="toolbar"], .doc-anchor, .diagram-zoom, .diagram-help').forEach(node => node.remove());
  body.querySelectorAll<HTMLInputElement>('input[type="checkbox"]').forEach(input => {
    input.toggleAttribute('checked', input.checked);
    input.disabled = true;
  });
  return { heading, body };
}

const exportStyles = `
:root {
  color-scheme: light;
  --paper: #fff; --canvas: #edf1f6; --ink: #19253a; --muted: #52627a;
  --line: #d9e1eb; --accent: #274cb4; --tint: #edf3ff;
  font: 16px/1.65 system-ui, sans-serif; color: var(--ink); background: var(--canvas);
}
* { box-sizing: border-box }
body { margin: 0; padding: clamp(0px, 4vw, 64px) clamp(0px, 3vw, 36px) }
main { max-width: 1180px; margin: auto; border: 1px solid var(--line); border-radius: 20px; background: var(--paper); overflow: clip }
main > header {
  padding: clamp(24px, 6vw, 68px);
  background: linear-gradient(115deg, #e7efff, #f2f0ff 78%, #f8f6ff);
  border-bottom: 1px solid var(--line);
}
main > header h1 { font-size: clamp(2rem, 5vw, 3.8rem); line-height: 1.06; letter-spacing: -.045em; margin: .2em 0 .35em }
.print-eyebrow { margin: 0; color: var(--accent); font-size: .78rem; font-weight: 750; letter-spacing: .09em; text-transform: uppercase }
.print-meta { color: var(--muted); font-size: .85rem; margin: 0 }
.export-layout { display: flex; align-items: flex-start; gap: clamp(24px, 4vw, 56px); padding: clamp(24px, 5vw, 64px) }
.artifact-document { flex: 1; min-width: 0; overflow-wrap: anywhere }
.artifact-document > :first-child, .artifact-document > :first-child > :first-child { margin-top: 0 }
.export-outline { flex: 0 0 190px; position: sticky; top: 20px; max-height: calc(100vh - 40px); overflow: auto; font-size: .84rem; border-left: 2px solid var(--line); padding-left: 14px }
.export-outline strong { font-size: .75rem; text-transform: uppercase; letter-spacing: .06em; color: var(--muted) }
.export-outline ol { padding: 0; margin: .55em 0 0; list-style: none }
.export-outline li { margin: .35em 0; overflow-wrap: anywhere }
.export-outline a { color: var(--muted); text-decoration: none }
.export-outline a:hover, .export-outline a:focus-visible { color: var(--accent); text-decoration: underline }
.export-outline li.h3 { padding-left: .9em }
h1, h2, h3, h4 { line-height: 1.2; scroll-margin-top: 20px }
.artifact-document h2 { margin: 1.8em 0 .6em; padding-bottom: .35em; border-bottom: 1px solid var(--line); letter-spacing: -.025em }
.artifact-document h3 { margin: 1.5em 0 .45em }
.artifact-document :is(p, li) { max-width: 76ch }
a { color: var(--accent); text-underline-offset: .16em }
:target { outline: 2px solid var(--accent); outline-offset: 4px }
.doc-table { overflow-x: auto; margin: 1.2rem 0; border: 1px solid var(--line); border-radius: 9px }
table { border-collapse: collapse; width: 100%; font-size: .9rem }
th, td { padding: .65rem .8rem; border-bottom: 1px solid var(--line); text-align: left }
thead { background: var(--tint); color: #253b68 }
tbody tr:nth-child(even) { background: #f6f8fc }
tbody tr:last-child td { border-bottom: 0 }
.code-block { border: 1px solid var(--line); border-radius: 9px; overflow: hidden; margin: 1.25rem 0 }
.code-block > header { background: #edf1f7; color: var(--muted); padding: .45rem .85rem; font-size: .77rem; font-weight: 650 }
pre { padding: .9rem 1rem; margin: 0; overflow-x: auto; color: #e3eafa; background: #152239; font-size: .85rem; line-height: 1.55; tab-size: 4 }
pre code { color: inherit }
.hljs-comment, .hljs-quote { color: #a7b7ce }
.hljs-string, .hljs-addition, .hljs-regexp { color: #99e1b3 }
.hljs-keyword, .hljs-selector-tag, .hljs-literal { color: #d3b3ff }
.hljs-number, .hljs-built_in, .hljs-attr, .hljs-variable { color: #ffd085 }
.hljs-title, .hljs-type, .hljs-name { color: #90cdfd }
.hljs-deletion { color: #ffabb1 }
:not(pre) > code { padding: .1em .32em; border-radius: 4px; background: var(--tint); color: #263e7c }
blockquote, .doc-callout { margin: 1.15rem 0; padding: .65rem 1rem; border-left: 4px solid var(--accent); border-radius: 0 8px 8px 0; background: var(--tint) }
.doc-callout.is-tip { background: #e9f7ef; border-color: #24814e }
.doc-callout.is-warning { background: #fff4dc; border-color: #a56506 }
.doc-callout.is-caution { background: #fff0ef; border-color: #ba414a }
.doc-callout.is-important, .doc-callout.is-decision { background: #f3eeff; border-color: #7952b3 }
.doc-callout-head { font-weight: 700 }
.doc-callout-icon { display: none }
.doc-callout-title { margin-left: .5rem }
.doc-callout-body > :first-child { margin-top: .25em }
.doc-callout-body > :last-child { margin-bottom: 0 }
.doc-footnotes { font-size: .9rem; border-top: 1px solid var(--line) }
.task-list-item { list-style: none }
input[type=checkbox] { accent-color: var(--accent) }
img { max-width: 100%; height: auto }
.mermaid-block { margin: 1.25rem 0; padding: 1rem; border: 1px solid var(--line); border-radius: 10px; background: #f8faff }
.diagram-viewport { height: auto !important }
.diagram-viewport img { display: block; width: auto; transform: none !important; margin: auto }
[data-diagram-scheme="dark"] .diagram-viewport img { filter: invert(1) hue-rotate(180deg) }
@media (max-width: 850px) {
  main { border-radius: 0 }
  .export-layout { flex-direction: column }
  .export-outline { position: static; flex: auto; max-height: none }
  .artifact-document { width: 100% }
}
@media print {
  body, .export-layout { padding: 0 }
  main { border: 0; border-radius: 0; overflow: visible }
  main > header { padding: 0 0 1rem; margin-bottom: 1rem; background: none }
  .export-outline { display: none }
  pre { color: var(--ink); background: #f2f4f8; white-space: pre-wrap }
  pre code * { color: inherit }
  h2, h3 { break-after: avoid }
}
`;

export function exportDocumentHTML(sheet: DocumentHeading, rendered: HTMLElement, dark: boolean): string {
  const { heading, body } = documentSnapshot(sheet, rendered);
  for (const viewport of body.querySelectorAll('.diagram-viewport')) {
    for (const attribute of ['tabindex', 'title', 'aria-roledescription', 'aria-describedby']) viewport.removeAttribute(attribute);
  }
  const doc = document.implementation.createHTMLDocument(sheet.title);
  const charset = doc.createElement('meta');
  charset.setAttribute('charset', 'utf-8');
  const policy = doc.createElement('meta');
  policy.httpEquiv = 'Content-Security-Policy';
  policy.content = "default-src 'none'; img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'";
  const viewport = doc.createElement('meta');
  viewport.name = 'viewport';
  viewport.content = 'width=device-width, initial-scale=1';
  const style = doc.createElement('style');
  style.textContent = exportStyles;
  doc.documentElement.lang = document.documentElement.lang || 'en';
  doc.head.prepend(charset, policy, viewport);
  doc.head.append(style);
  const main = doc.createElement('main');
  if (dark) main.dataset.diagramScheme = 'dark';
  const layout = doc.createElement('div');
  layout.className = 'export-layout';
  const sections = [...body.querySelectorAll<HTMLElement>('h2[id], h3[id]')];
  if (sections.length > 1) {
    const outline = doc.createElement('nav');
    outline.className = 'export-outline';
    outline.setAttribute('aria-label', 'On this page');
    const label = doc.createElement('strong');
    label.textContent = 'On this page';
    const list = doc.createElement('ol');
    for (const section of sections) {
      const item = doc.createElement('li');
      item.className = section.localName;
      const link = doc.createElement('a');
      link.href = '#' + encodeURIComponent(section.id);
      link.textContent = section.textContent?.trim() || section.id;
      item.append(link);
      list.append(item);
    }
    outline.append(label, list);
    layout.append(outline);
  }
  layout.append(body);
  main.append(heading, layout);
  doc.body.append(main);
  return '<!doctype html>\n' + doc.documentElement.outerHTML;
}
