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
// The collector is an init script, so it covers every document the page loads
// from then on and nothing loaded before: call it before the page's first
// navigation (it refuses otherwise), once per page, or once per context to
// cover popups and pages opened later as well.

let next = 0;

function installer(binding) {
  return `(() => {
    const send = window[${JSON.stringify(binding)}];
    // A popup's first, blank document hands its window to the page it then
    // loads (same origin), so the script can run twice on one window.
    const installed = ${JSON.stringify(binding + 'Installed')};
    if (typeof send !== 'function' || window[installed]) return;
    window[installed] = true;
    const report = value => { void send(String(value), value && value.stack ? String(value.stack) : '').catch(() => {}); };
    // What the app handles itself (preventDefault, as the browser's own
    // reporting honours) is not an error. This listener runs first, so it
    // reads the verdict once every listener has run: a task later.
    const unless = (event, value) => setTimeout(() => { if (!event.defaultPrevented) report(value); }, 0);
    window.addEventListener('error', event => unless(event, event.error || event.message));
    window.addEventListener('unhandledrejection', event => unless(event, event.reason));
  })();`;
}

async function install(target, errors, {label, onError} = {}) {
  // A binding per collector: a page may carry its own collector and its
  // context's without the two registrations colliding.
  const binding = `__lazyboxTestPageError${next++}`;
  await target.exposeBinding(binding, (_source, message, stack) => {
    const text = label ? `${label}: ${message}` : message;
    errors.push(text);
    onError?.(text, stack);
  });
  await target.addInitScript({content: installer(binding)});
  return errors;
}

/**
 * Collects the uncaught runtime errors and unhandled rejections of `page` into
 * `errors`, and returns it. `label` prefixes each message, for gates that pool
 * several pages into one list; `onError(message, stack)` sees each as it comes.
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
