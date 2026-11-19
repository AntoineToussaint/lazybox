import { useMemo } from 'react';

const previewElements = new Set((
  'a abbr article aside b blockquote br caption code col colgroup dd details div dl dt em figcaption figure footer ' +
  'h1 h2 h3 h4 h5 h6 header hr i img li main mark nav ol p picture pre s section small span strong style summary ' +
  'table tbody td th thead tfoot tr u ul'
).split(' '));
const previewAttributes = new Set('alt aria-label class colspan dir height id lang open role rowspan scope style title width'.split(' '));

function htmlPreview(source: string): string {
  // Template content is inert while parsing and filtering. Only the filtered
  // result reaches the sandbox, whose CSP also blocks external CSS and images.
  const template = document.createElement('template');
  template.innerHTML = source;
  for (const element of template.content.querySelectorAll('*')) {
    if (element.namespaceURI !== 'http://www.w3.org/1999/xhtml' || !previewElements.has(element.localName)) {
      element.remove();
      continue;
    }
    for (const attribute of Array.from(element.attributes)) {
      const embeddedImage = element.localName === 'img' && attribute.name === 'src' &&
        /^data:image\/(?:png|jpeg|gif|webp);base64,/i.test(attribute.value);
      if (!embeddedImage && !previewAttributes.has(attribute.name)) element.removeAttributeNode(attribute);
    }
  }
  return '<!doctype html><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" ' +
    'content="default-src \'none\'; style-src \'unsafe-inline\'; img-src data:; base-uri \'none\'; form-action \'none\'">' +
    template.innerHTML;
}

function HtmlPreview({ source }: { source: string }) {
  const preview = useMemo(() => htmlPreview(source), [source]);
  return <iframe className="artifact-html-preview" title="HTML artifact preview" sandbox="" srcDoc={preview}/>;
}

const markdown = { extension: 'md', mime: 'text/markdown', label: 'Markdown', Preview: undefined };
const formats = new Map<string, Omit<typeof markdown, 'Preview'> & { Preview?: typeof HtmlPreview }>([
  ['markdown', markdown],
  ['html', { extension: 'html', mime: 'text/html', label: 'HTML source', Preview: HtmlPreview }],
]);

export function artifactFormat(format: string) {
  return formats.get(format) ?? markdown;
}
