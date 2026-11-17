import { orderPages, sectionOf, titleOf, type DocPage } from './docs-model';

/**
 * The documentation bundled with the app: docs, read at build time. Pages
 * and images load on demand, so they cost nothing until Help → Documentation
 * opens. docs/internal is for people working on Lazybox, not in the app.
 */
const pages = import.meta.glob(['../../../docs/**/*.md', '!../../../docs/internal/**'], { query: '?raw', import: 'default' }) as Record<string, () => Promise<string>>;
const images = import.meta.glob('../../../docs/**/*.{png,jpg,jpeg,gif,webp,svg}', { query: '?url', import: 'default' }) as Record<string, () => Promise<string>>;
const PREFIX = '../../../docs/';
/** A page's path relative to docs/ ("guides/chat.md", "install.md"). */
const docPath = (key: string) => key.startsWith(PREFIX) ? key.slice(PREFIX.length) : '../' + key.slice('../../../'.length);

let loaded: Promise<DocPage[]> | undefined;
/** Every page with its text, in reading order. */
export function loadDocs(): Promise<DocPage[]> {
  loaded ??= Promise.all(Object.entries(pages).map(async ([key, load]) => {
    const path = docPath(key), text = await load();
    return { path, text, title: titleOf(path, text), section: sectionOf(path) };
  })).then(orderPages);
  return loaded;
}

const imageURLs = new Map<string, string>();
/** Loads every image URL once (they are small strings; the files load when shown). */
export async function loadDocImages() {
  await Promise.all(Object.entries(images).map(async ([key, load]) => { imageURLs.set(key.slice(PREFIX.length), await load()); }));
  return imageURLs;
}
