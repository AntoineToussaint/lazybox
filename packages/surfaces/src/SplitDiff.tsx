import type { MouseEvent as ReactMouseEvent, ReactNode } from 'react';
import type { SplitRow } from './diff-layout';

/** What a diff renderer needs from ReviewDiff for one line: its cells on a
 * side, and the review state for it. Both views share these, so a line looks
 * and comments the same unified or side by side. */
export type LineCells = {
  /** The line number, the comment button (when this side can be commented on) and the code. */
  cells(index: number, side: 'old' | 'new'): ReactNode;
  /** Classes for one line on one side: its change type, selected, commented. */
  className(index: number): string;
  selected(index: number): boolean;
  extend(index: number): void;
  /** The line's context menu handler (the 'diff-line' scope). */
  menu?(index: number): ((event: ReactMouseEvent) => void) | undefined;
};

/** One row of the side-by-side view: the old line on the left, the new one on
 * the right. A side with no line (an added line has no old one) is blank. */
export function SplitRowView({ row, line }: { row: Extract<SplitRow, { kind: 'pair' }>; line: LineCells }) {
  const selected = [row.old, row.new].some(index => index !== undefined && line.selected(index));
  const side = (index: number | undefined, which: 'old' | 'new') => {
    if (index === undefined) return <div role="presentation" className="review-split-side is-empty"><span role="gridcell" className="review-number"/><span role="gridcell" className="review-gutter"/><span role="gridcell" className="review-code"/></div>;
    // A context line is the same on both sides; it is commented on from the new side.
    return <div role="presentation" className={`review-split-side ${line.className(index)}`} onPointerEnter={() => line.extend(index)} onContextMenu={line.menu?.(index)}>
      {line.cells(index, which)}
    </div>;
  };
  return <div role="row" aria-selected={selected} className="review-split-row">
    {side(row.old, 'old')}
    {side(row.new, 'new')}
  </div>;
}
