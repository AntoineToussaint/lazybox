/** Plain panel text belongs to its document, including when the dock moves it to a new window. */
export function selectPanelText(event: KeyboardEvent, panel: HTMLElement): void {
  if (event.defaultPrevented || event.isComposing || event.ctrlKey === event.metaKey ||
      event.shiftKey || event.altKey || event.key.toLowerCase() !== 'a') return;
  const target = event.composedPath()[0] as HTMLElement | undefined;
  if (target?.isContentEditable || target?.closest?.('input, textarea, select, .monaco-editor, .xterm, [role="dialog"]')) return;
  const doc = panel.ownerDocument;
  const selection = doc.getSelection();
  if (!selection) return;
  event.preventDefault();
  const range = doc.createRange();
  range.selectNodeContents(panel);
  selection.removeAllRanges();
  selection.addRange(range);
}
