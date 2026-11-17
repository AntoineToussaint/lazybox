/**
 * A tooltip only for text the reader cannot see whole: one that repeats a
 * visible title adds nothing and covers the row beneath it. Use as the
 * element's `onPointerEnter`; it decides when the pointer arrives, so a
 * width change since the last render is taken into account.
 */
export const titleWhenTruncated = (text: string) => (event: { currentTarget: HTMLElement }) => {
  const node = event.currentTarget;
  if (node.scrollWidth > node.clientWidth + 1 || node.scrollHeight > node.clientHeight + 1) node.title = text;
  else node.removeAttribute('title');
};
