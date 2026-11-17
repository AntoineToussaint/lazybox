// The browsers the UI suites launch: Playwright's, with every page reporting
// macOS when the suites run on another host (CI runs them on Linux).
//
// Lazybox V2 is a macOS app and the suites test its macOS behaviour: they press
// `Meta` for ⌘ and read ⌘ and ⌥ in hints. The app reads the platform from
// `navigator.platform` (Monaco from `navigator.userAgent`), as it must in a real
// non-Mac browser, so on Linux the same page would switch ⌘ to Ctrl. Off macOS,
// every context these browsers open runs an init script, before the page's own,
// that makes those report a Mac. On a Mac nothing changes. The browser's own
// clipboard and undo keys still follow the host: press `ControlOrMeta+V` there.
//
//   import { webkit, chromium, expect } from './fixtures/playwright.mjs';
import { webkit as playwrightWebkit, chromium as playwrightChromium } from '@playwright/test';

export { devices, expect } from '@playwright/test';

/** Runs in the page before its scripts: the platform and user agent say macOS. */
function reportMac() {
  const mac = value => value.replace(/\((?:X11; )?Linux [^)]*\)/, '(Macintosh; Intel Mac OS X 10_15_7)');
  const define = (name, value) => Object.defineProperty(Navigator.prototype, name, { configurable: true, enumerable: true, get: () => value });
  define('platform', 'MacIntel');
  define('userAgent', mac(navigator.userAgent));
  define('appVersion', mac(navigator.appVersion));
}

function macBrowserType(type) {
  if (process.platform === 'darwin') return type;
  return new Proxy(type, {
    get(target, name) {
      if (name !== 'launch') { const value = Reflect.get(target, name, target); return typeof value === 'function' ? value.bind(target) : value; }
      return async (...args) => {
        const browser = await target.launch(...args), newContext = browser.newContext.bind(browser);
        // Browser.newPage creates its context through newContext, so both get the script.
        browser.newContext = async (...options) => { const context = await newContext(...options); await context.addInitScript(reportMac); return context; };
        return browser;
      };
    },
  });
}

export const webkit = macBrowserType(playwrightWebkit);
export const chromium = macBrowserType(playwrightChromium);
