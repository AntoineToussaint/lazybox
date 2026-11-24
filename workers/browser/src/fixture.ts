import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';

/**
 * Local sites for the browser service's tests: `home` (the site the agent
 * works on), `other` (a second origin) and `forbidden` (a loopback port no
 * policy allows). Nothing leaves the machine.
 */
export type Sites = { home: string; other: string; forbidden: string; ports: { home: number; other: number; forbidden: number }; close(): Promise<void> };

const page = (title: string, body: string) => `<!doctype html><html><head><meta charset="utf-8"><title>${title}</title></head><body>${body}</body></html>`;

/** `delay`: the reply comes this many ms late (a slow server). */
type Reply = { status?: number; headers?: Record<string, string>; body: string; delay?: number };

async function serve(handler: (path: string, query: URLSearchParams) => Reply): Promise<{ server: Server; port: number }> {
  const server = createServer((req, res) => {
    const url = new URL(req.url || '/', 'http://fixture');
    const reply = handler(url.pathname, url.searchParams);
    setTimeout(() => {
      res.writeHead(reply.status || 200, { 'content-type': 'text/html; charset=utf-8', ...reply.headers });
      res.end(reply.body);
    }, reply.delay || 0);
  });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', () => resolve()));
  return { server, port: (server.address() as AddressInfo).port };
}

export async function startSites(): Promise<Sites> {
  const forbidden = await serve(() => ({ body: page('Forbidden', '<h1>Private service</h1>') }));
  const other = await serve(() => ({ body: page('Other site', '<h1>Other origin page</h1>') }));
  const otherURL = `http://127.0.0.1:${other.port}`, forbiddenURL = `http://127.0.0.1:${forbidden.port}`;
  const home = await serve((path, query): Reply => {
    switch (path) {
      case '/': return { body: page('Home', `<h1>Fixture home</h1><a href="/form">Form</a> <a href="${otherURL}/other">Other site</a>`) };
      case '/form': return { body: page('Form', `<form action="/done"><label>Name <input name="name"></label><label>Color <select name="color"><option>red</option><option>blue</option></select></label><button>Send</button></form>`) };
      case '/done': return { body: page('Done', `<p>Thanks ${escape(query.get('name') || '')}, ${escape(query.get('color') || '')}</p>`) };
      case '/slow': return { body: page('Slow', `<p id="state">Loading</p><script>setTimeout(() => { document.getElementById('state').textContent = 'Ready now'; }, 400)</script>`) };
      case '/signin': return { body: page('Sign in', `<form action="/welcome"><label>Email <input name="email"></label><label>Password <input type="password" name="password"></label><button>Sign in</button></form>`) };
      // A page slow to leave (a busy machine, or its own beforeunload): the navigation is requested at
      // once, but its request leaves only after this handler, well past a fixed short look.
      case '/signin-slow': return { body: page('Sign in', `<form action="/welcome"><label>Email <input name="email"></label><button>Sign in</button></form><script>addEventListener('beforeunload', () => { const end = Date.now() + 400; while (Date.now() < end); });</script>`) };
      case '/welcome': return { body: page('Welcome', '<h1>Signed in</h1>') };
      // The usual sign-in: the form posts, the server redirects, and the next page is slow to answer.
      case '/signin-redirect': return { body: page('Sign in', '<form method="post" action="/session"><label>Email <input name="email"></label><button>Sign in</button></form>') };
      case '/session': return { status: 302, headers: { location: '/dashboard' }, body: '' };
      case '/dashboard': return { delay: 800, body: page('Dashboard', '<h1>Your dashboard</h1>') };
      // A sign-in by script: it posts with fetch, and goes on only once that answers.
      case '/signin-script': return { body: page('Sign in', `<button onclick="fetch('/api/session', { method: 'POST' }).then(() => { location.href = '/dashboard'; })">Sign in</button>`) };
      case '/api/session': return { delay: 400, headers: { 'content-type': 'application/json' }, body: '{}' };
      // A choice that submits its form at once.
      case '/choose': return { body: page('Choose', '<form action="/done"><select name="color" onchange="this.form.submit()"><option>red</option><option>blue</option></select></form>') };
      case '/download': return { body: page('Download', '<a href="/file">Get the report</a>') };
      // A form that comes back to the same address (a search, a post-back with errors).
      case '/same': return { body: page('Same', '<form method="post" action="/same"><button>Search again</button></form>') };
      // A link answered with no content: the browser stays on this page.
      case '/stay': return { body: page('Stay', '<a href="/nothing">Check again</a>') };
      case '/nothing': return { status: 204, body: '' };
      case '/file': return { headers: { 'content-type': 'text/plain', 'content-disposition': 'attachment; filename="report.txt"' }, body: 'report body\n' };
      case '/popup': return { body: page('Popup', '<a href="/form" target="_blank">Open in a new window</a>') };
      case '/fetch-forbidden': return { body: page('Fetch', `<p id="out">pending</p><script>fetch('${forbiddenURL}/').then(() => out.textContent = 'reached', () => out.textContent = 'refused')</script>`) };
      case '/redirect-forbidden': return { status: 302, headers: { location: `${forbiddenURL}/` }, body: '' };
      case '/long': return { body: page('Long', `<div style="height:3000px">Top</div><p>Bottom</p>`) };
      case '/alert': return { body: page('Alert', `<script>alert('hello')</script><p>After the alert</p>`) };
      default: return { status: 404, body: page('Not found', 'Not found') };
    }
  });
  return {
    home: `http://127.0.0.1:${home.port}`, other: otherURL, forbidden: forbiddenURL,
    ports: { home: home.port, other: other.port, forbidden: forbidden.port },
    close: async () => { for (const s of [home, other, forbidden]) { s.server.closeAllConnections(); await new Promise(resolve => s.server.close(resolve)); } },
  };
}

function escape(text: string) {
  return text.replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]!));
}

/** A ref from a snapshot, by the line that names it: refIn(text, 'button "Send"'). */
export function refIn(snapshot: string, label: string): string {
  const line = snapshot.split('\n').find(l => l.includes(`${label} [`) || l.includes(`${label}:`) || l.trimEnd().endsWith(label));
  const ref = line && /\[ref=(\w+)\]/.exec(line)?.[1];
  if (!ref) throw new Error(`no ${label} in snapshot:\n${snapshot}`);
  return ref;
}
