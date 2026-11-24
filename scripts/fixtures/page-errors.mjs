// Every UI gate counts browser errors through this fixture, never through
// Playwright's raw `pageerror` event (scripts/page-errors.test.ts enforces it).
//
// Why: WebKit's protocol reports a fetch that a navigation cut, and that the
// app caught, as a JavaScript console error ("Fetch API cannot load … due to
// access control checks"), and Playwright emits it as `pageerror`. A gate that
// asserts "no page errors" across a reload or goto then fails at random. The
// browser's own `error` and `unhandledrejection` events fire only for what the
// application actually failed to handle, in every engine; one the app marks
// handled with preventDefault (as CodeEditor does for Monaco's cancelled
// clipboard writes) is not counted, as the browser does not report it either.
//
// It fails closed. An error is reported the moment it is dispatched, so one
// raised while the document unloads (a pagehide handler) or just before the
// page closes still arrives; only once every listener has run, a task later,
// is it retracted if the app turned out to handle it. That task is a
// MessageChannel message, which Playwright's clock (page.clock) never fakes
// or pauses. `drainPageErrors(errors)` waits until everything the pages have
// dispatched so far has arrived (and been retracted or not): call it before
// asserting on `errors`.
//
// The collector is an init script, so it covers every document the page loads
// from then on and nothing loaded before: call it before the page's first
// navigation (it refuses otherwise) and before any other init script (one
// that throws first would go unheard), once per page, or once per context to
// cover popups and pages opened later as well.

let next = 0;
const drainers = Symbol('page-error drainers');

function installer(binding) {
  return `(() => {
    const send = window[${JSON.stringify(binding)}];
    // A popup's first, blank document hands its window to the page it then
    // loads (same origin), so the script can run twice on one window.
    const installed = ${JSON.stringify(binding + 'Installed')};
    if (typeof send !== 'function' || window[installed]) return;
    const later = [];
    const channel = new MessageChannel();
    channel.port1.onmessage = () => later.shift()?.();
    const afterListeners = run => { later.push(run); channel.port2.postMessage(0); };
    // Ids restart with each document: a random prefix keeps them apart.
    const doc = crypto.getRandomValues(new Uint32Array(2)).join('');
    let ids = 0;
    const call = (...args) => send(...args).catch(() => {});
    // Until the test has it, an error also waits in sessionStorage: a binding
    // call made while the document unloads can be dropped (Chromium), and the
    // tab's next same-origin document sends it again. The test keeps one copy.
    const key = ${JSON.stringify(binding + 'Pending')};
    const pending = () => { try { return JSON.parse(sessionStorage.getItem(key) || '[]'); } catch { return []; } };
    const save = list => { try { if (list.length) sessionStorage.setItem(key, JSON.stringify(list)); else sessionStorage.removeItem(key); } catch {} };
    const settle = id => save(pending().filter(entry => entry[0] !== id));
    const deliver = entry => send('report', ...entry).then(() => settle(entry[0]), () => {});
    const report = (event, value) => {
      const entry = [doc + '.' + (++ids), String(value), value && value.stack ? String(value.stack) : ''];
      save([...pending(), entry]);
      void deliver(entry);
      afterListeners(() => { if (event.defaultPrevented) { settle(entry[0]); void call('retract', entry[0]); } });
    };
    for (const entry of pending()) void deliver(entry);
    window[installed] = { drain: () => new Promise(resolve => afterListeners(resolve)).then(() => send('drain')) };
    window.addEventListener('error', event => report(event, event.error || event.message));
    window.addEventListener('unhandledrejection', event => report(event, event.reason));
  })();`;
}

async function install(target, errors, {label, format, onError} = {}) {
  // A binding per collector: a page may carry its own collector and its
  // context's without the two registrations colliding.
  const binding = `__lazyboxTestPageError${next++}`;
  const reported = new Map(), heard = new Set();
  await target.exposeBinding(binding, (_source, kind, key, message, stack) => {
    if (kind === 'report') {
      if (heard.has(key)) return;
      heard.add(key);
      let text = label ? `${label}: ${message}` : message;
      if (format) text = format(text);
      reported.set(key, text);
      errors.push(text);
      onError?.(text, stack);
    } else if (kind === 'retract' && reported.has(key)) {
      const at = errors.lastIndexOf(reported.get(key));
      if (at >= 0) errors.splice(at, 1);
      reported.delete(key);
    }
  });
  await target.addInitScript({content: installer(binding)});
  const pages = () => typeof target.pages === 'function' ? target.pages() : [target];
  const drain = () => Promise.all(pages().filter(page => !page.isClosed()).map(page =>
    page.evaluate(name => window[name]?.drain(), binding + 'Installed').catch(() => {})));
  if (!errors[drainers]) Object.defineProperty(errors, drainers, {value: [], enumerable: false});
  errors[drainers].push(drain);
  return errors;
}

/**
 * Collects the uncaught runtime errors and unhandled rejections of `page` into
 * `errors`, and returns it. `label` prefixes each message, for gates that pool
 * several pages into one list; `format(message)` rewrites it as it is
 * recorded; `onError(message, stack)` sees each as it comes (one the app then
 * handles is retracted from `errors` afterwards, not from what onError saw).
 */
export async function collectPageErrors(page, errors = [], options = {}) {
  if (page.url() !== 'about:blank') throw new Error(`collectPageErrors: ${page.url()} is already loaded; collect before the page's first navigation`);
  return install(page, errors, options);
}

/**
 * The same for every page of `context`, including popups and pages it opens
 * later. Call it before the context opens its first page.
 */
export async function collectContextPageErrors(context, errors = [], options = {}) {
  if (context.pages().length) throw new Error('collectContextPageErrors: the context already has pages; collect before it opens the first');
  return install(context, errors, options);
}

/**
 * Waits until every error the collected pages have dispatched so far has
 * reached `errors`, and every one the app handled has left it. A page that
 * is closed or between documents has nothing left to wait for.
 */
export async function drainPageErrors(errors) {
  await Promise.all((errors[drainers] ?? []).map(drain => drain()));
  return errors;
}
