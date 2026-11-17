/**
 * Focuses `element`, or, when it cannot take focus yet (a panel the dock has
 * just placed or rebuilt is drawn on its next frame), on each of the next few
 * frames until it can. Gives up as soon as focus has gone somewhere else
 * meanwhile (a dialog, a click): that choice wins.
 */
export function focusWhenShown(element: HTMLElement | null | undefined, options: FocusOptions = {}, frames = 12) {
  if (!element) return;
  const owner = element.ownerDocument, before = owner.activeElement;
  const attempt = (left: number) => {
    const now = owner.activeElement;
    if (!element.isConnected || now === element || (now !== before && now !== owner.body)) return;
    element.focus(options);
    if (owner.activeElement !== element && left > 0) requestAnimationFrame(() => attempt(left - 1));
  };
  attempt(frames);
}
