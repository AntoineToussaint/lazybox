import { useRef, useState, type CSSProperties, type ReactNode } from 'react';

export type SwipeAction = { id: string; label: string; glyph: ReactNode; tone: 'allow' | 'pin' | 'read' | 'stop'; run(): void };

const decide = 10, longPress = 550;

/** A light tick where the platform has one (Android); iOS Safari has no vibration API. */
function tick() { try { navigator.vibrate?.(8); } catch { /* no haptics here */ } }

/**
 * A row that answers a horizontal swipe, as Mail does: right commits the
 * leading action (past the line, the action's label grows and the phone
 * ticks); left reveals the trailing actions, and a long swipe commits the
 * first. A long press opens every action. Vertical scrolling stays native
 * (touch-action: pan-y), and a drag never also opens the row.
 */
export function SwipeRow({ leading, trailing, onLongPress, children, className, ...rest }: { leading?: SwipeAction; trailing: SwipeAction[]; onLongPress(): void; children: ReactNode; className?: string; 'data-state'?: string; 'data-unread'?: true; 'data-pinned'?: true; 'aria-label'?: string }) {
  const [offset, setOffset] = useState(0);
  const [dragging, setDragging] = useState(false);
  const [armed, setArmed] = useState(false);
  const gesture = useRef<{ id: number; x: number; y: number; base: number; horizontal: boolean; cancelled: boolean; timer?: ReturnType<typeof setTimeout>; width: number } | null>(null);
  const suppress = useRef(false);
  const pressedAt = useRef(0);
  const trayWidth = trailing.length * 76;
  const limits = (width: number) => ({ lead: Math.min(132, width * 0.36), full: width * 0.62 });
  const end = () => { const g = gesture.current; if (g?.timer) clearTimeout(g.timer); gesture.current = null; setDragging(false); setArmed(false); };
  const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.pointerType === 'mouse' && event.button !== 0) return;
    suppress.current = false;
    const width = event.currentTarget.getBoundingClientRect().width || 320;
    const g = { id: event.pointerId, x: event.clientX, y: event.clientY, base: offset, horizontal: false, cancelled: false, width, timer: undefined as ReturnType<typeof setTimeout> | undefined };
    if (event.pointerType !== 'mouse') g.timer = setTimeout(() => { if (gesture.current === g && !g.horizontal) { g.cancelled = true; suppress.current = true; pressedAt.current = Date.now(); tick(); onLongPress(); } }, longPress);
    gesture.current = g;
  };
  const onPointerMove = (event: React.PointerEvent<HTMLDivElement>) => {
    const g = gesture.current;
    if (!g || g.id !== event.pointerId || g.cancelled) return;
    const dx = event.clientX - g.x, dy = event.clientY - g.y;
    if (!g.horizontal) {
      if (Math.abs(dy) > decide && Math.abs(dy) >= Math.abs(dx)) { if (g.timer) clearTimeout(g.timer); g.cancelled = true; return; }
      if (Math.abs(dx) < decide || Math.abs(dx) < Math.abs(dy) * 1.4) return;
      g.horizontal = true; if (g.timer) clearTimeout(g.timer); setDragging(true);
      try { event.currentTarget.setPointerCapture(event.pointerId); } catch { /* synthetic pointers have nothing to capture */ }
    }
    let next = g.base + dx;
    if (next > 0 && !leading) next = Math.min(next, 12) / 3;
    if (next < 0 && !trailing.length) next = Math.max(next, -12) / 3;
    const { lead, full } = limits(g.width);
    // Past the end, the row resists.
    if (next > lead) next = lead + (next - lead) * 0.35;
    if (next < -full) next = -full + (next + full) * 0.35;
    const nowArmed = (next >= lead && !!leading) || (next <= -full && trailing.length > 0);
    if (nowArmed !== armed) { if (nowArmed) tick(); setArmed(nowArmed); }
    setOffset(next);
  };
  const onPointerUp = (event: React.PointerEvent<HTMLDivElement>) => {
    const g = gesture.current;
    if (!g || g.id !== event.pointerId) return;
    const { lead, full } = limits(g.width);
    if (g.horizontal) {
      suppress.current = true;
      if (offset >= lead && leading) { setOffset(0); leading.run(); }
      else if (offset <= -full && trailing.length) { setOffset(0); trailing[0].run(); }
      else setOffset(offset < -trayWidth / 2 ? -trayWidth : 0);
    }
    end();
  };
  const close = () => setOffset(0);
  const style = { '--swipe': `${offset}px` } as CSSProperties;
  return <li className={['remote-row', className].filter(Boolean).join(' ')} {...rest} data-dragging={dragging || undefined} data-armed={armed || undefined} data-open={offset < 0 && !dragging || undefined} style={style}>
    {leading && <div className="remote-swipe-lead" data-tone={leading.tone} aria-hidden="true"><span>{leading.glyph}{leading.label}</span></div>}
    {trailing.length > 0 && <div className="remote-swipe-tray" style={{ width: Math.max(trayWidth, -offset) }}>
      {trailing.map((action, i) => <button key={action.id} type="button" tabIndex={offset < 0 ? 0 : -1} aria-hidden={offset < 0 ? undefined : true} data-tone={action.tone} style={i === 0 && armed && offset < 0 ? { flexGrow: 6 } : undefined} onClick={() => { close(); action.run(); }}>{action.glyph}<span>{action.label}</span></button>)}
    </div>}
    <div className="remote-row-surface" onPointerDown={onPointerDown} onPointerMove={onPointerMove} onPointerUp={onPointerUp} onPointerCancel={() => { setOffset(gesture.current?.horizontal ? 0 : offset); end(); }}
      onClickCapture={event => { if (suppress.current) { suppress.current = false; event.preventDefault(); event.stopPropagation(); } else if (offset < 0) { event.preventDefault(); event.stopPropagation(); close(); } }}
      onContextMenu={event => { event.preventDefault(); if (Date.now() - pressedAt.current > 1000) onLongPress(); }}>
      {children}
    </div>
  </li>;
}
