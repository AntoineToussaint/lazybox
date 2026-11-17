import { useMemo } from 'react';
import { encode } from 'uqr';

/**
 * A QR code as one SVG path: crisp at any size, dark modules on a white
 * quiet zone so phone cameras read it in a dark theme. The encoded text is
 * on `data-value` for tests; the accessible name is the caller's.
 */
export function QrCode({ value, size = 232, label }: { value: string; size?: number; label: string }) {
  const { path, extent } = useMemo(() => {
    const qr = encode(value, { ecc: 'M', border: 3 });
    let d = '';
    qr.data.forEach((row, y) => row.forEach((dark, x) => { if (dark) d += `M${x} ${y}h1v1h-1z`; }));
    return { path: d, extent: qr.size };
  }, [value]);
  return <svg className="qr-code" role="img" aria-label={label} data-value={value} width={size} height={size} viewBox={`0 0 ${extent} ${extent}`} shapeRendering="crispEdges">
    <rect width={extent} height={extent} fill="#fff"/>
    <path d={path} fill="#111"/>
  </svg>;
}
