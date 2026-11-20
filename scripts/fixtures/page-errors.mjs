// WebKit's protocol reports caught fetch failures during navigation as
// JavaScript console errors, which Playwright emits as `pageerror`. Collect
// the browser's actual runtime-error and unhandled-rejection events instead.
export async function collectPageErrors(page, errors = []) {
  await page.exposeBinding('__lazyboxTestPageError', (_source, message) => errors.push(message));
  await page.addInitScript(() => {
    const report = value => { void window.__lazyboxTestPageError(String(value)).catch(() => {}); };
    window.addEventListener('error', event => report(event.error || event.message));
    window.addEventListener('unhandledrejection', event => report(event.reason));
  });
  return errors;
}
