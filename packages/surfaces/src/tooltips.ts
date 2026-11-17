import { placeTooltip } from './context-menu-model';
import './tooltips.css';

/**
 * One tooltip for the whole window, in place of the system's.
 *
 * Every control says what it does through its `title`. The system draws that
 * text itself, wherever it likes: in the desktop web view it is cut by the
 * window's edge ("Track as artifact… (or right-click"), cannot wrap, and
 * ignores the theme. This takes over the drawing and nothing else: authors
 * keep writing `title`, and while the pointer rests on an element (or the
 * keyboard focuses it) its title is shown in a tooltip that is placed like a
 * menu: under the element, above it when there is no room, and always whole
 * inside the window.
 *
 * The `title` attribute is only set aside while the tooltip is up (so the
 * system does not draw a second one) and is put back when it goes; a title
 * that changes meanwhile (a ticking clock) updates the tooltip.
 */
const DELAY_MS = 450;
const KEPT = 'data-tooltip-title';

let tip: HTMLDivElement | undefined, anchor: HTMLElement | undefined, timer: ReturnType<typeof setTimeout> | undefined, watcher: MutationObserver | undefined;

function element(): HTMLDivElement {
  if (!tip) {
    tip = document.createElement('div');
    tip.className = 'lazybox-tooltip'; tip.setAttribute('role', 'tooltip'); tip.setAttribute('popover', 'manual');
    document.body.append(tip);
  }
  return tip;
}

function hide() {
  clearTimeout(timer); timer = undefined;
  watcher?.disconnect(); watcher = undefined;
  if (anchor) {
    // Unless the page gave it a new title meanwhile, the element gets its own back.
    const kept = anchor.getAttribute(KEPT);
    if (kept !== null && !anchor.hasAttribute('title')) anchor.setAttribute('title', kept);
    anchor.removeAttribute(KEPT);
    anchor = undefined;
  }
  if (tip?.matches(':popover-open')) tip.hidePopover();
}

/** Takes the element's title for the tooltip, so the system shows none of its own. */
function take(node: HTMLElement): string {
  const title = node.getAttribute('title');
  if (title !== null) { node.setAttribute(KEPT, title); node.removeAttribute('title'); }
  return node.getAttribute(KEPT) || '';
}

function show(node: HTMLElement) {
  if (!node.isConnected) return;
  const text = take(node).trim();
  if (!text) { hide(); return; }
  const box = element();
  box.textContent = text;
  if (!box.matches(':popover-open')) { try { box.showPopover(); } catch { return; } }
  const rect = node.getBoundingClientRect(), size = box.getBoundingClientRect();
  const { left, top } = placeTooltip(rect, { width: size.width, height: size.height }, { width: window.innerWidth, height: window.innerHeight });
  box.style.left = `${left}px`; box.style.top = `${top}px`;
}

function arm(node: HTMLElement, delay: number) {
  if (node === anchor) return;
  hide();
  anchor = node;
  // From now on the title is ours: the system's own tooltip must not appear beside it.
  take(node);
  // Inside an element with a hover card of its own (a workspace row), the card says it all: no tooltip over it.
  const quiet = !!node.closest('[data-hover-card]');
  watcher = new MutationObserver(() => { if (anchor === node && node.hasAttribute('title')) { take(node); if (!quiet && tip?.matches(':popover-open')) show(node); } });
  watcher.observe(node, { attributes: true, attributeFilter: ['title'] });
  if (!quiet) timer = setTimeout(() => { if (anchor === node) show(node); }, delay);
}

const titled = (target: EventTarget | null) => target instanceof Element ? target.closest<HTMLElement>(`[title]:not([title=""]),[${KEPT}]`) : null;

/** Installs the window's tooltip once. Safe to call again. */
export function installTooltips() {
  if (typeof document === 'undefined' || (window as { __lazyboxTooltips?: boolean }).__lazyboxTooltips) return;
  (window as { __lazyboxTooltips?: boolean }).__lazyboxTooltips = true;
  document.addEventListener('pointerover', event => {
    if (event.pointerType === 'touch') return;
    const node = titled(event.target);
    if (node) arm(node, DELAY_MS); else hide();
  }, true);
  document.addEventListener('pointerout', event => { if (anchor && !(event.relatedTarget instanceof Node && anchor.contains(event.relatedTarget))) hide(); }, true);
  // The keyboard reaches the same words: a focused control shows its title too.
  document.addEventListener('focusin', event => { const node = titled(event.target); if (node && node === event.target && node.matches(':focus-visible')) arm(node, DELAY_MS); }, true);
  document.addEventListener('focusout', hide, true);
  for (const type of ['pointerdown', 'keydown', 'wheel', 'scroll'] as const) document.addEventListener(type, hide, { capture: true, passive: true });
  window.addEventListener('blur', hide);
}

installTooltips();
