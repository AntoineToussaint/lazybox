/** Browser Back on Windows/Linux, and Command+[ on macOS. */
export function isNavigationBack(event: KeyboardEvent) {
  return !event.defaultPrevented && !event.isComposing && !event.shiftKey && !event.ctrlKey &&
    ((event.altKey && !event.metaKey && event.key === 'ArrowLeft') ||
     (event.metaKey && !event.altKey && event.key === '['));
}
