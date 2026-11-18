/**
 * The pairing link a `lazybox://pair?link=<encoded link>` deep link carries, read from the raw URL:
 * the router's parsed params drop the link's own `#fragment`, which holds the offer.
 */
export function deepLinkedPairing(url: string | null): string {
  const match = url && /^lazybox:\/\/pair\/?\?(?:[^#]*&)?link=([^&#]*)/i.exec(url);
  if (!match) return '';
  try { return decodeURIComponent(match[1]); } catch { return ''; }
}
