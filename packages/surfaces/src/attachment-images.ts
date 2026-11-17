/**
 * Images a message carries, decoded once and shown small.
 *
 * A thumbnail is a downscaled copy made off the main thread
 * (`createImageBitmap`), so a 4K screenshot is decoded once to make a 128 px
 * image, not every time a row of the conversation re-renders. The original is
 * kept as an object URL for the preview and the lightbox. Every URL made here
 * is revoked: when its composer chip goes, or when a sent image has been off
 * screen long enough to fall out of a small idle cache.
 */

export type LoadedImage = {
  /** The original bytes, for the hover preview and the lightbox. */
  url: string;
  /** A small copy for the chip; the original when it could not be made. */
  thumb: string;
  /** Natural size; 0 when the image could not be decoded ahead of display. */
  width: number;
  height: number;
  size: number;
};

/** The longest edge of a thumbnail: a 16:9 image shown 88 px tall stays sharp on a Retina screen. */
export const THUMBNAIL_EDGE = 320;

/** What the Runner serves for preview (attachments.Previewable). SVG is not an image here. */
export const PREVIEWABLE = /^image\/(png|jpeg|gif|webp)$/i;
/** A local file the browser can show: anything image/* but SVG, which is a document. */
export const isLocalImage = (file: { type: string }) => /^image\//i.test(file.type) && !/svg/i.test(file.type);

/** Decode `blob` once and make its thumbnail; never throws. */
export async function loadImage(blob: Blob): Promise<LoadedImage> {
  const url = URL.createObjectURL(blob);
  let thumb = url, width = 0, height = 0;
  try {
    const bitmap = await createImageBitmap(blob);
    width = bitmap.width; height = bitmap.height;
    const scale = Math.min(1, THUMBNAIL_EDGE / Math.max(width, height));
    if (scale < 1) {
      const canvas = document.createElement('canvas');
      canvas.width = Math.max(1, Math.round(width * scale)); canvas.height = Math.max(1, Math.round(height * scale));
      const context = canvas.getContext('2d');
      if (context) {
        context.imageSmoothingQuality = 'high';
        context.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
        const small = await new Promise<Blob | null>(resolve => canvas.toBlob(resolve, 'image/webp', 0.85));
        if (small) thumb = URL.createObjectURL(small);
      }
    }
    bitmap.close();
  } catch { /* An image the browser cannot decode ahead of time still gets an <img> try. */ }
  return { url, thumb, width, height, size: blob.size };
}

export function releaseImage(image: LoadedImage) {
  if (image.thumb !== image.url) URL.revokeObjectURL(image.thumb);
  URL.revokeObjectURL(image.url);
}

type Entry = { promise: Promise<LoadedImage>; refs: number; value?: LoadedImage; failed?: boolean };

/**
 * Sent images by Runner path, shared by every row and the lightbox of a
 * window. An image no row shows any more stays in a small idle cache, so
 * scrolling back does not fetch it again, and is revoked when it falls out.
 */
export class ImageCache {
  private entries = new Map<string, Entry>();
  private idle: string[] = [];
  private active = 0;
  private queue: (() => void)[] = [];
  constructor(private idleLimit = 24, private concurrency = 3) {}

  /** Load (once) and hold `key` until `release`. */
  acquire(key: string, fetch: (signal?: AbortSignal) => Promise<Blob>): Promise<LoadedImage> {
    let entry = this.entries.get(key);
    if (!entry || entry.failed) {
      const created: Entry = { refs: 0, promise: this.limit(() => fetch()).then(loadImage) };
      created.promise.then(value => { created.value = value; if (this.entries.get(key) !== created) releaseImage(value); }, () => { created.failed = true; });
      this.entries.set(key, created);
      entry = created;
    }
    entry.refs++;
    this.idle = this.idle.filter(value => value !== key);
    return entry.promise;
  }

  release(key: string) {
    const entry = this.entries.get(key);
    if (!entry || --entry.refs > 0) return;
    if (entry.failed) { this.entries.delete(key); return; }
    this.idle.push(key);
    while (this.idle.length > this.idleLimit) {
      const evicted = this.idle.shift()!, gone = this.entries.get(evicted);
      this.entries.delete(evicted);
      if (gone?.value) releaseImage(gone.value);
    }
  }

  /** How many images are held or cached, for tests. */
  get size() { return this.entries.size; }

  private limit<T>(task: () => Promise<T>): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const run = () => { this.active++; task().then(resolve, reject).finally(() => { this.active--; this.queue.shift()?.(); }); };
      if (this.active < this.concurrency) run(); else this.queue.push(run);
    });
  }
}

const fileIds = new WeakMap<File, number>();
let nextFileId = 0;
/** A stable React key for a selected file, so removing one chip does not reload the others. */
export function fileKey(file: File, index: number, files: readonly File[]): string {
  let id = fileIds.get(file);
  if (id === undefined) fileIds.set(file, id = ++nextFileId);
  // The same file selected twice is two chips.
  return files.indexOf(file) === index ? String(id) : `${id}-${index}`;
}

/** `next` takes `previous`'s key: an edited box stays the same box, and keeps its focus. */
export function inheritKey(previous: File, next: File) {
  const id = fileIds.get(previous);
  if (id !== undefined) fileIds.set(next, id);
}

/** The size an image is shown at to fit `box` without being enlarged past its natural size. */
export function fitWithin(width: number, height: number, box: { width: number; height: number }): { width: number; height: number } {
  if (!width || !height) return { width: 0, height: 0 };
  const scale = Math.min(1, box.width / width, box.height / height);
  return { width: Math.max(1, Math.round(width * scale)), height: Math.max(1, Math.round(height * scale)) };
}

type Box = { width: number; height: number };
type Rect = { left: number; top: number; right: number; bottom: number };
export type PreviewPlacement = { left: number; top: number; width: number; height: number; side: 'above' | 'below' | 'right' | 'left' };

/**
 * Where the enlarged preview of a thumbnail goes, and how big its image is.
 * At most `share` of the window, the image's shape kept, never enlarged, and
 * never over its thumbnail: above it when it fits there (a composer sits at
 * the bottom), else below, else beside it; when it fits nowhere whole, the
 * side with the most room, smaller. `chrome` is the card around the image.
 */
export function placePreview(anchor: Rect, image: Box, viewport: Box, { share = 0.6, chrome = { width: 14, height: 40 }, minCardWidth = 180, gap = 10, edge = 8 } = {}): PreviewPlacement {
  const most = { width: viewport.width * share - chrome.width, height: viewport.height * share - chrome.height };
  const whole = fitWithin(image.width, image.height, most);
  const sides = [
    { side: 'above' as const, box: { width: most.width, height: Math.min(most.height, anchor.top - gap - edge - chrome.height) } },
    { side: 'below' as const, box: { width: most.width, height: Math.min(most.height, viewport.height - anchor.bottom - gap - edge - chrome.height) } },
    { side: 'right' as const, box: { width: Math.min(most.width, viewport.width - anchor.right - gap - edge - chrome.width), height: most.height } },
    { side: 'left' as const, box: { width: Math.min(most.width, anchor.left - gap - edge - chrome.width), height: most.height } },
  ].filter(option => option.box.width > 0 && option.box.height > 0).map(option => ({ ...option, size: fitWithin(image.width, image.height, option.box) }));
  const chosen = sides.find(option => option.size.width === whole.width && option.size.height === whole.height)
    ?? sides.sort((a, b) => b.size.width * b.size.height - a.size.width * a.size.height)[0]
    ?? { side: 'above' as const, size: whole };
  const card = { width: Math.max(minCardWidth, chosen.size.width + chrome.width), height: chosen.size.height + chrome.height };
  const centerX = (anchor.left + anchor.right) / 2 - card.width / 2, centerY = (anchor.top + anchor.bottom) / 2 - card.height / 2;
  const at = chosen.side === 'above' ? { left: centerX, top: anchor.top - gap - card.height }
    : chosen.side === 'below' ? { left: centerX, top: anchor.bottom + gap }
    : chosen.side === 'right' ? { left: anchor.right + gap, top: centerY }
    : { left: anchor.left - gap - card.width, top: centerY };
  const clamp = (value: number, size: number, room: number) => Math.max(edge, Math.min(value, room - size - edge));
  return { left: clamp(at.left, card.width, viewport.width), top: clamp(at.top, card.height, viewport.height), width: chosen.size.width, height: chosen.size.height, side: chosen.side };
}

/** "42 KiB", "3.1 MiB". */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.ceil(bytes / 1024)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

/**
 * The files a paste carries, or none when the paste is text. A rich app (a
 * spreadsheet, a word processor) puts a picture of the copied text beside the
 * text: the text wins there. A file copied in Finder carries its name as text:
 * that is still a file paste. An image pasted from the clipboard has a generic
 * name ("image.png"); it gets a readable, distinct one.
 */
export function pastedFiles(data: Pick<DataTransfer, 'files' | 'getData'>, now = new Date()): File[] {
  const files = Array.from(data.files || []);
  if (!files.length) return [];
  const text = data.getData('text/plain').trim();
  if (text && !files.some(file => text.split(/\r?\n/).map(line => line.trim()).includes(file.name))) return [];
  const stamp = `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())} at ${pad(now.getHours())}.${pad(now.getMinutes())}.${pad(now.getSeconds())}`;
  let generic = 0;
  return files.map(file => {
    if (!/^image\.\w+$/i.test(file.name) && file.name) return file;
    const extension = file.name.split('.').at(-1) || file.type.split('/')[1] || 'png';
    generic++;
    return new File([file], `Pasted image ${stamp}${generic > 1 ? ` (${generic})` : ''}.${extension.toLowerCase()}`, { type: file.type, lastModified: file.lastModified });
  });
}
const pad = (value: number) => String(value).padStart(2, '0');
