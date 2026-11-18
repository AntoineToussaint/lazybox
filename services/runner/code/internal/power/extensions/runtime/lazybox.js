// The Lazybox extension bridge client, protocol 1 (EXTENSIONS.md; types in
// lazybox.d.ts). Lazybox serves it to every extension at
// _lazybox/lazybox.js; load it before your own script:
//
//   <link rel="stylesheet" href="_lazybox/lazybox.css">
//   <script src="_lazybox/lazybox.js"></script>
//   <script src="app.js"></script>
//
// An extension runs in a sandboxed frame with no access to Lazybox, the
// network (beyond the hosts the person approved) or storage. It asks
// Lazybox for data and actions here, and gets only what its grants allow;
// anything else is refused with a LazyboxError whose code says why.
(function () {
  'use strict';
  var PROTOCOL = 1;
  var pending = new Map();
  var listeners = new Map();
  var next = 1;
  var theme = null;

  function LazyboxError(error) {
    var e = new Error((error && error.message) || 'Lazybox refused the request');
    e.name = 'LazyboxError';
    e.code = (error && error.code) || 'failed';
    if (error && error.grant) e.grant = error.grant;
    return e;
  }

  function post(message) {
    message.lazybox = PROTOCOL;
    parent.postMessage(message, '*');
  }

  function emit(event, data) {
    (listeners.get(event) || []).slice().forEach(function (listener) {
      try { listener(data); } catch (error) { setTimeout(function () { throw error; }); }
    });
  }

  function applyTheme(next) {
    if (!next || typeof next !== 'object') return;
    theme = next;
    var root = document.documentElement;
    var tokens = next.tokens || {};
    Object.keys(tokens).forEach(function (name) {
      if (/^[a-z0-9-]+$/.test(name) && typeof tokens[name] === 'string') root.style.setProperty('--' + name, tokens[name]);
    });
    if (next.fonts) {
      if (next.fonts.ui) root.style.setProperty('--font-ui', next.fonts.ui);
      if (next.fonts.code) root.style.setProperty('--font-mono', next.fonts.code);
    }
    if (typeof next.textScale === 'number') root.style.setProperty('--text-scale', String(next.textScale));
    root.style.colorScheme = next.scheme === 'light' ? 'light' : 'dark';
    root.setAttribute('data-scheme', next.scheme === 'light' ? 'light' : 'dark');
    if (next.density) root.setAttribute('data-density', next.density);
    emit('theme', next);
  }

  window.addEventListener('message', function (event) {
    if (event.source !== parent) return;
    var message = event.data;
    if (!message || message.lazybox !== PROTOCOL) return;
    if (typeof message.id === 'string' && pending.has(message.id)) {
      var waiting = pending.get(message.id);
      pending.delete(message.id);
      clearTimeout(waiting.timer);
      if (message.error) waiting.reject(LazyboxError(message.error));
      else waiting.resolve(message.result);
      return;
    }
    if (typeof message.event === 'string') {
      if (message.event === 'theme') applyTheme(message.data);
      else emit(message.event, message.data);
    }
  });

  /** Calls one bridge method; rejects with a LazyboxError. */
  function call(method, params) {
    return new Promise(function (resolve, reject) {
      var id = 'r' + next++;
      var timer = setTimeout(function () {
        pending.delete(id);
        reject(LazyboxError({ code: 'timeout', message: 'Lazybox did not answer ' + method + ' in time' }));
      }, 30000);
      pending.set(id, { resolve: resolve, reject: reject, timer: timer });
      post({ id: id, method: method, params: params || {} });
    });
  }

  // What the browser refused (a host that is not approved, an inline
  // script), and uncaught errors: Lazybox shows them quietly in the panel.
  // (An inline style injected by a browser tool is not the extension's doing: not reported.)
  document.addEventListener('securitypolicyviolation', function (event) {
    var directive = String(event.effectiveDirective || event.violatedDirective || '');
    if (/^style-src/.test(directive) && event.blockedURI === 'inline') return;
    post({ event: 'blocked', data: { directive: directive, uri: String(event.blockedURI || '').slice(0, 300) } });
  });
  window.addEventListener('error', function (event) {
    post({ event: 'error', data: { message: String(event.message || 'Script error').slice(0, 300) } });
  });
  window.addEventListener('unhandledrejection', function (event) {
    var reason = event.reason;
    if (reason && reason.name === 'LazyboxError') return; // refusals are shown already
    post({ event: 'error', data: { message: String((reason && reason.message) || reason || 'Unhandled rejection').slice(0, 300) } });
  });

  var ready = call('hello', { protocol: PROTOCOL }).then(function (info) {
    applyTheme(info.theme);
    return info;
  });

  /** Runs fn now and every ms while the panel is visible; returns a stop function. */
  function every(ms, fn) {
    var interval = Math.max(1000, ms || 5000);
    var timer = 0;
    var stopped = false;
    function tick() {
      if (stopped) return;
      Promise.resolve().then(fn).catch(function () {}).then(function () {
        if (!stopped) timer = setTimeout(tick, document.visibilityState === 'hidden' ? interval * 4 : interval);
      });
    }
    ready.then(tick, tick);
    return function () { stopped = true; clearTimeout(timer); };
  }

  window.lazybox = Object.freeze({
    protocol: PROTOCOL,
    ready: ready,
    call: call,
    every: every,
    get theme() { return theme; },
    on: function (event, listener) {
      if (!listeners.has(event)) listeners.set(event, []);
      listeners.get(event).push(listener);
      return function () { listeners.set(event, (listeners.get(event) || []).filter(function (l) { return l !== listener; })); };
    },
    workspaces: { list: function () { return call('workspaces.list'); }, focused: function () { return call('workspaces.focused'); } },
    runtime: {
      read: function (workspaceId) { return call('runtime.read', { workspaceId: workspaceId }); },
      show: function (workspaceId, kind, target) { return call('runtime.show', { workspaceId: workspaceId, kind: kind, target: target }); },
    },
    github: { open: function (url) { return call('github.open', { url: url }); } },
    releases: { list: function () { return call('releases.list'); } },
    pulls: { list: function (workspaceId) { return call('pulls.list', workspaceId ? { workspaceId: workspaceId } : {}); } },
    artifacts: {
      list: function (workspaceId) { return call('artifacts.list', { workspaceId: workspaceId }); },
      draft: function (workspaceId, content) { return call('artifacts.draft', { workspaceId: workspaceId, content: content }); },
    },
    mailbox: { list: function () { return call('mailbox.list'); } },
    sessions: { list: function (options) { return call('sessions.list', options); } },
    attention: { list: function () { return call('attention.list'); } },
    commands: { list: function (sessionId) { return call('commands.list', { sessionId: sessionId }); } },
    missions: { list: function () { return call('missions.list'); } },
    usage: { read: function (options) { return call('usage.read', options); } },
    conversations: { show: function (sessionId) { return call('conversations.show', { sessionId: sessionId }); } },
    messages: { send: function (sessionId, text) { return call('messages.send', { sessionId: sessionId, text: text }); } },
    run: function (action, argument) { return call('do', { action: action, argument: argument || '' }); },
  });
})();
