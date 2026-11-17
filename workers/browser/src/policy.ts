import { isIP } from 'node:net';
import type { Policy } from './protocol.js';

/**
 * The hard network rules of an agent browser (BROWSER.md, "What the browser
 * may reach"). A page may reach the public web over http(s) and ws(s), plus
 * the loopback ports the Runner names (the workspace's running previews).
 * Everything else that would let a web page reach this machine or its network
 * is refused: other loopback ports (the Runner, the person's own servers),
 * private, link-local, CGNAT and unique-local addresses, and the host:port
 * pairs the Runner lists. These rules are not the person's to waive from a
 * card; they hold whoever drives the browser.
 */

/** Schemes a page may load. data: and blob: never leave the browser. */
const allowedSchemes = new Set(['http:', 'https:', 'ws:', 'wss:', 'data:', 'blob:']);
/** Schemes a navigation may target: the web, nothing local. */
const navigableSchemes = new Set(['http:', 'https:']);

export type Verdict = { allow: true } | { allow: false; reason: string };

const allow: Verdict = { allow: true };
const refuse = (reason: string): Verdict => ({ allow: false, reason });

/** The port a URL reaches, its default included. */
export function portOf(url: URL): number {
  if (url.port) return Number(url.port);
  return url.protocol === 'https:' || url.protocol === 'wss:' ? 443 : 80;
}

/** scheme://host:port, the unit a person allows. */
export function originOf(raw: string): string {
  try {
    const url = new URL(raw);
    return navigableSchemes.has(url.protocol) ? url.origin : '';
  } catch { return ''; }
}

/** A host name without IPv6 brackets or a trailing dot, lower case. */
export function bareHost(host: string): string {
  return host.replace(/^\[|\]$/g, '').replace(/\.$/, '').toLowerCase();
}

/** Names that always mean this machine, whatever DNS says. */
export function isLoopbackName(host: string): boolean {
  const h = bareHost(host);
  return h === 'localhost' || h.endsWith('.localhost');
}

function ipv4(address: string): number[] | undefined {
  const parts = address.split('.').map(Number);
  return parts.length === 4 && parts.every(n => Number.isInteger(n) && n >= 0 && n <= 255) ? parts : undefined;
}

/** The eight 16-bit groups of an IPv6 address, a dotted IPv4 tail included. */
function ipv6Groups(address: string): number[] | undefined {
  if (isIP(address) !== 6) return undefined;
  let text = address.toLowerCase();
  const tail = /(\d+\.\d+\.\d+\.\d+)$/.exec(text);
  if (tail) {
    const v4 = ipv4(tail[1]);
    if (!v4) return undefined;
    text = text.slice(0, -tail[1].length) + `${((v4[0] << 8) | v4[1]).toString(16)}:${((v4[2] << 8) | v4[3]).toString(16)}`;
  }
  const [head, rest] = text.split('::');
  const left = head ? head.split(':') : [], right = rest ? rest.split(':') : [];
  const groups = rest === undefined ? left : [...left, ...Array(8 - left.length - right.length).fill('0'), ...right];
  return groups.length === 8 ? groups.map(g => parseInt(g, 16)) : undefined;
}

/**
 * An IPv4 address an IPv6 one carries (mapped ::ffff:a.b.c.d, the old
 * compatible ::a.b.c.d, or NAT64 64:ff9b::a.b.c.d), in whichever notation:
 * a URL writes ::ffff:192.168.0.1 as ::ffff:c0a8:1.
 */
function mappedIPv4(address: string): string | undefined {
  const g = ipv6Groups(address);
  if (!g) return undefined;
  const zeros = (n: number) => g.slice(0, n).every(x => x === 0);
  const mapped = zeros(5) && g[5] === 0xffff;
  const compatible = zeros(6) && (g[6] !== 0 || g[7] > 1);
  const nat64 = g[0] === 0x64 && g[1] === 0xff9b && g.slice(2, 6).every(x => x === 0);
  if (!mapped && !compatible && !nat64) return undefined;
  return [g[6] >> 8, g[6] & 0xff, g[7] >> 8, g[7] & 0xff].join('.');
}

/** Loopback: 127.0.0.0/8 and ::1. */
export function isLoopbackAddress(address: string): boolean {
  const a = bareHost(address), mapped = mappedIPv4(a);
  if (mapped) return isLoopbackAddress(mapped);
  const v4 = ipv4(a);
  if (v4) return v4[0] === 127;
  return a === '::1' || a === '0:0:0:0:0:0:0:1';
}

/**
 * Addresses a web page must not reach besides loopback: this host's own
 * "unspecified" address, private networks, link-local (cloud metadata lives
 * there), CGNAT (tailnets), benchmarking and multicast, and their IPv6 kin.
 */
export function isPrivateAddress(address: string): boolean {
  const a = bareHost(address), mapped = mappedIPv4(a);
  if (mapped) return isPrivateAddress(mapped);
  const v4 = ipv4(a);
  if (v4) {
    const [x, y] = v4;
    return x === 0 || x === 10 || (x === 172 && y >= 16 && y <= 31) || (x === 192 && y === 168)
      || (x === 169 && y === 254) || (x === 100 && y >= 64 && y <= 127) || (x === 198 && (y === 18 || y === 19)) || x >= 224;
  }
  if (isIP(a) !== 6) return false;
  const first = parseInt(a.split(':')[0] || '0', 16);
  return a === '::' || (first & 0xfe00) === 0xfc00 /* fc00::/7 unique local */ || (first & 0xffc0) === 0xfe80 /* fe80::/10 link local */ || (first & 0xff00) === 0xff00 /* multicast */;
}

/** Whether host:port is one the Runner listed as never reachable. */
function listedBlocked(policy: Policy, host: string, port: number): boolean {
  const key = `${bareHost(host)}:${port}`;
  return policy.blockedHosts.some(entry => {
    const at = entry.lastIndexOf(':');
    return at > 0 && `${bareHost(entry.slice(0, at))}:${entry.slice(at + 1)}` === key;
  });
}

/**
 * Judges one address a connection is about to reach: a literal IP (resolved
 * or given) or a name that always means loopback. Names that need DNS are
 * judged by their resolved addresses (see proxy.ts).
 */
export function judgeAddress(policy: Policy, host: string, port: number): Verdict {
  if (listedBlocked(policy, host, port)) return refuse(`${bareHost(host)}:${port} belongs to Lazybox`);
  if (isLoopbackName(host) || isLoopbackAddress(host)) {
    return policy.loopbackPorts.includes(port) ? allow : refuse(`this computer's port ${port} is not one of the workspace's previews`);
  }
  if (isPrivateAddress(host)) return refuse(`${bareHost(host)} is on a private or local network`);
  return allow;
}

/**
 * Judges a URL a page asks for, before any DNS: its scheme, and its host when
 * that is a literal address or a loopback name. A host name that resolves to a
 * refused address is caught when the connection is made (proxy.ts), which
 * also covers redirects and DNS rebinding.
 */
export function judgeURL(policy: Policy, raw: string): Verdict {
  let url: URL;
  try { url = new URL(raw); } catch { return refuse('it is not a valid address'); }
  if (!allowedSchemes.has(url.protocol)) return refuse(`agent browsers do not open ${url.protocol} addresses`);
  if (url.protocol === 'data:' || url.protocol === 'blob:') return allow;
  if (url.username || url.password) return refuse('addresses with a user name or password are not opened');
  return judgeAddress(policy, url.hostname, portOf(url));
}

/** Judges an address the agent asks to navigate to: the web only. */
export function judgeNavigation(policy: Policy, raw: string): Verdict {
  let url: URL;
  try { url = new URL(raw); } catch { return refuse('it is not a valid address'); }
  if (!navigableSchemes.has(url.protocol)) return refuse(`agent browsers open only http and https addresses, not ${url.protocol}`);
  return judgeURL(policy, raw);
}
