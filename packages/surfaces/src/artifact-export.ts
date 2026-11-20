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
:root { color-scheme: light; font: 16px/1.65 system-ui, sans-serif; color: #202127; background: #fff }
body { margin: 0; padding: 3rem 1.5rem }
main { max-width: 76ch; margin: auto; overflow-wrap: anywhere }
main > header { border-bottom: 1px solid #dddde5; padding-bottom: 1.25rem; margin-bottom: 2rem }
h1, h2, h3, h4 { line-height: 1.2 }
main > header h1 { font: bold clamp(2rem, 6vw, 3rem)/1.1 Georgia, serif; margin: .25em 0 }
.print-eyebrow, .print-meta, .code-block > header { color: #585866; font-size: .85rem; margin: 0 }
a { color: #4b3aa0 }
.doc-table { overflow-x: auto; margin: 1rem 0 }
table { border-collapse: collapse; width: 100%; font-size: .95rem }
th, td { padding: .45rem .65rem; border: 1px solid #dcdce5; text-align: left }
th, pre, .doc-callout, :not(pre) > code { background: #f5f5f9 }
pre { padding: .85rem 1rem; overflow-x: auto; border-radius: .4rem; margin: .35rem 0 1rem }
pre code, pre code * { color: inherit !important }
:not(pre) > code { padding: .12em .25em; border-radius: .2em }
blockquote, .doc-callout { margin: 1rem 0; padding: .5rem 1rem; border-left: 3px solid #8174b5 }
.doc-callout-icon { display: none }
.doc-callout-title { margin-left: .5rem }
.doc-footnotes { font-size: .9rem; border-top: 1px solid #dddde5 }
.task-list-item { list-style: none }
img { max-width: 100%; height: auto }
.diagram-viewport { height: auto !important }
.diagram-viewport img { display: block; width: auto; transform: none !important }
[data-diagram-scheme="dark"] .diagram-viewport img { filter: invert(1) hue-rotate(180deg) }
@media print { body { padding: 0 } }
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
  main.append(heading, body);
  doc.body.append(main);
  return '<!doctype html>\n' + doc.documentElement.outerHTML;
}
