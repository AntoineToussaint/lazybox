// Lazybox's extension shell (EXTENSIONS.md). The app frames this page,
// sandboxed, from the extension content origin; it frames the extension's
// entry page, sandboxed again, under a policy whose frame-src is this
// session's own address: the extension can load its own pages and nothing
// else, so it cannot carry what it read out in a navigation.
//
// It relays messages and nothing more. What the extension posts goes to the
// app wrapped as {from: "extension"}, so the extension can never pass for the
// shell; what the app sends to the extension goes to the inner frame only.
(function () {
  'use strict';
  var entry = new URLSearchParams(location.search).get('entry') || 'index.html';
  // The Runner checked the entry; checked again here, it stays inside the base.
  if (!/^[^/:?#\\][^:?#\\]*\.html?$/i.test(entry) || entry.split('/').some(function (part) { return part === '..' || part.charAt(0) === '.'; })) entry = 'index.html';
  var frame = document.createElement('iframe');
  frame.setAttribute('sandbox', 'allow-scripts');
  frame.setAttribute('referrerpolicy', 'no-referrer');
  frame.setAttribute('title', 'Extension');
  frame.src = new URL('../' + entry, location.href).href;
  var loads = 0;
  var toApp = function (message) { parent.postMessage(Object.assign({ lazybox: 1, from: 'shell' }, message), '*'); };
  frame.addEventListener('load', function () { loads += 1; toApp({ event: 'loaded', data: { count: loads } }); });
  // The extension tried to leave its page (frame-src): say so; it stays here.
  // Only that: a style a browser tool injects into this page is no news.
  document.addEventListener('securitypolicyviolation', function (event) {
    var directive = String(event.effectiveDirective || event.violatedDirective || '');
    if (directive !== 'frame-src' && directive !== 'child-src') return;
    toApp({ event: 'blocked', data: { directive: directive, uri: String(event.blockedURI || '').slice(0, 300) } });
  });
  window.addEventListener('message', function (event) {
    var data = event.data;
    if (event.source === frame.contentWindow) {
      parent.postMessage({ lazybox: 1, from: 'extension', message: data }, '*');
      return;
    }
    if (event.source !== parent || !data || data.lazybox !== 1) return;
    if (data.to === 'extension' && frame.contentWindow) frame.contentWindow.postMessage(data.message, '*');
    else if (data.to === 'shell' && data.command === 'reload') frame.src = new URL('../' + entry, location.href).href;
  });
  document.body.appendChild(frame);
  toApp({ event: 'ready', data: { entry: entry } });
})();
