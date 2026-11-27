import { createServer, request as httpRequest, type IncomingHttpHeaders, type Server } from 'node:http';
import { connect, isIP, type AddressInfo, type Socket } from 'node:net';
import { lookup } from 'node:dns/promises';
import type { Policy } from './protocol.js';
import { bareHost, isLoopbackName, judgeAddress, type Verdict } from './policy.js';

/**
 * Every connection an agent browser makes goes through this loopback proxy
 * (Chromium is launched with it, loopback included). It is where the address
 * rules are authoritative: it resolves a host name once, refuses it if any
 * address it resolves to is refused, and connects to the address it vetted,
 * so neither a redirect, a WebSocket, a service worker nor a DNS answer that
 * changes between check and connect (rebinding) reaches what a page may not.
 * Request interception in the page (browsers.ts) only attributes and explains.
 */

/** How long a vetted resolution is reused; the connection still goes to the vetted address. */
const resolutionTTL = 30_000;

type Resolution = { address: string; at: number };

export type Refusal = { host: string; port: number; reason: string };

export class GateProxy {
  private server: Server;
  private resolved = new Map<string, Resolution>();
  private sockets = new Set<Socket>();

  constructor(private policy: () => Policy, private refused: (refusal: Refusal) => void, private previewActive: (port: number) => Promise<boolean>) {
    this.server = createServer((req, res) => {
      // A plain-HTTP request names its absolute URL.
      let url: URL;
      try { url = new URL(req.url || ''); } catch { res.writeHead(400).end(); return; }
      if (url.protocol !== 'http:') { res.writeHead(400).end(); return; }
      const port = Number(url.port) || 80;
      void this.vet(url.hostname, port).then(target => {
        if (!target.ok) { res.writeHead(403, { 'content-type': 'text/plain' }).end(`Blocked by Lazybox: ${target.reason}`); return; }
        const upstream = httpRequest({ host: target.address, port, method: req.method, path: url.pathname + url.search, headers: forwarded(req.headers), setHost: false }, response => {
          res.writeHead(response.statusCode || 502, response.rawHeaders);
          response.pipe(res);
        });
        upstream.on('error', () => { if (!res.headersSent) res.writeHead(502); res.end(); });
        req.pipe(upstream);
      });
    });
    // https, wss and ws reach their server through a tunnel.
    this.server.on('connect', (req, client: Socket, head: Buffer) => {
      this.track(client);
      const { host, port } = splitAuthority(req.url || '');
      if (!host || !port) { client.end('HTTP/1.1 400 Bad Request\r\n\r\n'); return; }
      void this.vet(host, port).then(target => {
        if (!target.ok) { client.end('HTTP/1.1 403 Forbidden\r\n\r\n'); return; }
        const upstream = connect(port, target.address, () => {
          client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
          if (head.length) upstream.write(head);
          upstream.pipe(client); client.pipe(upstream);
        });
        this.track(upstream);
        upstream.on('error', () => client.destroy());
        client.on('error', () => upstream.destroy());
      });
    });
    this.server.on('connection', socket => this.track(socket));
  }

  private track(socket: Socket) {
    this.sockets.add(socket);
    socket.on('close', () => this.sockets.delete(socket));
  }

  /** Listens on an OS-chosen loopback port and returns the proxy's address for Chromium. */
  async listen(): Promise<string> {
    await new Promise<void>((resolve, reject) => { this.server.once('error', reject); this.server.listen(0, '127.0.0.1', () => resolve()); });
    return `http://127.0.0.1:${(this.server.address() as AddressInfo).port}`;
  }

  async close() {
    for (const socket of this.sockets) socket.destroy();
    await new Promise<void>(resolve => this.server.close(() => resolve()));
  }

  /** The address to connect to for host:port, or why not. */
  async vet(host: string, port: number): Promise<{ ok: true; address: string } | { ok: false; reason: string }> {
    const name = bareHost(host), policy = this.policy();
    const verdict = (address: string): Verdict => {
      // The Runner's own host:port entries name hosts, so check the name too.
      const named = judgeAddress(policy, name, port);
      return named.allow ? judgeAddress(policy, address, port) : named;
    };
    let address = name;
    if (isLoopbackName(name)) address = '127.0.0.1';
    else if (!isIP(name)) {
      const cached = this.resolved.get(name);
      if (cached && Date.now() - cached.at < resolutionTTL) address = cached.address;
      else {
        let all: { address: string }[];
        try { all = await lookup(name, { all: true }); } catch { return this.refuse(host, port, `${name} could not be resolved`); }
        // Every answer must pass: a name that also points inside is refused whole.
        for (const answer of all) {
          const judged = verdict(answer.address);
          if (!judged.allow) return this.refuse(host, port, `${name} points to a refused address (${judged.reason})`);
        }
        address = all[0]?.address || '';
        if (!address) return this.refuse(host, port, `${name} could not be resolved`);
        this.resolved.set(name, { address, at: Date.now() });
      }
    }
    const judged = verdict(address);
    if (!judged.allow) return this.refuse(host, port, judged.reason);
    if (isLoopbackName(address) && !await this.previewActive(port).catch(() => false)) {
      return this.refuse(host, port, 'workspace preview is no longer available');
    }
    return { ok: true, address };
  }

  private refuse(host: string, port: number, reason: string): { ok: false; reason: string } {
    this.refused({ host: bareHost(host), port, reason });
    return { ok: false, reason };
  }
}

/** host and port of a CONNECT authority: example.com:443 or [::1]:8080. */
export function splitAuthority(authority: string): { host: string; port: number } {
  const match = /^(\[[^\]]+\]|[^:]+):(\d+)$/.exec(authority);
  return match ? { host: bareHost(match[1]), port: Number(match[2]) } : { host: '', port: 0 };
}

/** Request headers without the hop-by-hop ones a proxy must not forward. */
function forwarded(headers: IncomingHttpHeaders): IncomingHttpHeaders {
  const out = { ...headers };
  for (const name of ['proxy-connection', 'proxy-authorization', 'connection', 'keep-alive', 'te', 'trailer', 'upgrade']) delete out[name];
  return out;
}
