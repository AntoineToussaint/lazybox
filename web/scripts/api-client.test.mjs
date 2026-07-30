import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const html = readFileSync(
  new URL('../../crates/server/src/api_client.html', import.meta.url),
  'utf8'
);
const script = html.match(/<script>([\s\S]*?)<\/script>/)?.[1];

if (!script) {
  throw new Error('api_client.html does not contain an inline script');
}

class Element {
  constructor() {
    this.children = [];
    this.dataset = {};
    this.disabled = false;
    this.hidden = false;
    this.listeners = new Map();
    this.textContent = '';
    this.value = '';
    this.parent = null;
  }

  addEventListener(type, listener) {
    this.listeners.set(type, listener);
  }

  append(...children) {
    for (const child of children) {
      child.parent = this;
      this.children.push(child);
    }
  }

  prepend(child) {
    child.parent = this;
    this.children.unshift(child);
  }

  replaceChildren(...children) {
    this.children = [];
    this.append(...children);
  }

  get lastElementChild() {
    return this.children.at(-1) ?? null;
  }

  remove() {
    if (!this.parent) return;
    this.parent.children = this.parent.children.filter((child) => child !== this);
    this.parent = null;
  }
}

function loadClient({ eventStream = 'open' } = {}) {
  const selectors = [
    '#connect',
    '#token',
    '#connect-button',
    '#status',
    '#workspaces',
    '#workspace-empty',
    '#workspace-count',
    '#warnings',
    '#events',
    '#event-empty',
    '#event-count',
  ];
  const elements = new Map(selectors.map((selector) => [selector, new Element()]));
  const tokenInput = elements.get('#token');
  const tokenTag = html.match(/<input\b[^>]*\bid="token"[^>]*>/)?.[0] ?? '';
  tokenInput.required = /\brequired\b/.test(tokenTag);

  const requests = [];
  const fetch = async (path, options = {}) => {
    requests.push({ path, headers: { ...(options.headers ?? {}) } });
    if (path === '/v1/health') {
      return response({ ok: true, service: 'lazybox-api-gateway' });
    }
    if (path === '/v1/workspaces') {
      return response({ workspaces: [], warnings: [] });
    }
    if (path === '/v1/events') {
      return {
        ok: true,
        status: 200,
        body: {
          getReader() {
            return {
              read:
                eventStream === 'eof'
                  ? async () => ({ done: true })
                  : () => new Promise(() => {}),
            };
          },
        },
      };
    }
    throw new Error(`unexpected request: ${path}`);
  };

  vm.runInNewContext(script, {
    AbortController,
    TextDecoder,
    document: {
      createElement: () => new Element(),
      querySelector: (selector) => elements.get(selector),
    },
    fetch,
  });

  return {
    elements,
    requests,
    submit() {
      if (tokenInput.required && tokenInput.value === '') return false;
      elements.get('#connect').listeners.get('submit')({
        preventDefault() {},
      });
      return true;
    },
  };
}

function response(payload) {
  return {
    ok: true,
    status: 200,
    async json() {
      return payload;
    },
    async text() {
      return '';
    },
  };
}

async function settle() {
  for (let turn = 0; turn < 2; turn += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
}

test('connects without an authorization header when the gateway has no token', async () => {
  const client = loadClient();

  assert.equal(client.submit(), true);
  await settle();

  assert.deepEqual(
    client.requests.map(({ path }) => path),
    ['/v1/health', '/v1/workspaces', '/v1/events']
  );
  for (const request of client.requests) {
    assert.equal(request.headers.Authorization, undefined);
  }
});

test('reconnects with the token retained in page memory', async () => {
  const client = loadClient();
  client.elements.get('#token').value = 'secret';

  assert.equal(client.submit(), true);
  await settle();
  client.requests.length = 0;

  assert.equal(client.submit(), true);
  await settle();

  assert.deepEqual(
    client.requests.map(({ path }) => path),
    ['/v1/health', '/v1/workspaces', '/v1/events']
  );
  for (const request of client.requests) {
    assert.equal(request.headers.Authorization, 'Bearer secret');
  }
});

test('reports a clean event-stream EOF as disconnected', async () => {
  const client = loadClient({ eventStream: 'eof' });
  client.elements.get('#token').value = 'secret';

  assert.equal(client.submit(), true);
  await settle();

  assert.equal(
    client.elements.get('#status').textContent,
    'Live event stream disconnected. Reconnect to resume.'
  );
  assert.equal(client.elements.get('#status').dataset.tone, 'error');
});
