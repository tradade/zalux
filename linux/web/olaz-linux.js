// Olaz Linux host shim — injected at document start, before any of the shared
// Olaz host scripts (HostScripts/*.js).
//
// Those scripts were written for WKWebView: they call
// `window.webkit.messageHandlers.<name>.postMessage(msg)` and expect a Promise
// back, and `bridge.js` makes its synchronous calls with a blocking XHR to
// `zalo-app://local/__olaz/bridge-sync`. Both work here unchanged:
//
//   * `messageHandlers` is rebuilt on top of POSTs to the host's loopback server
//     (which runs on its own threads, so a blocking XHR there cannot deadlock
//     the WebKit main thread);
//   * any `zalo-app://` XHR is rewritten to the same loopback server;
//   * host → page pushes arrive through a long-poll loop and are fed to
//     `window.__zaloBridge.ipcEmit` / `.emit`, exactly like Swift did with
//     `callAsyncJavaScript`.
(() => {
  'use strict';
  if (window.__olazLinux) return;

  const cfg = window.__OLAZ_LINUX || {};
  const base = 'http://127.0.0.1:' + (cfg.port || 0);
  const jsonHeaders = { 'content-type': 'application/json' };
  const debug = !!cfg.debug;
  // Debug harness only (`OLAZ_DEV_EVAL=1`): the host sends `dev-eval` events and
  // waits for the result on `/__olaz/dev/eval-result`.
  const devEval = !!cfg.devEval;
  // Electron ran the hidden shared-worker page in its own window. Here it is a
  // second (hidden) Tauri webview on the same origin, so bridge pushes must say
  // which of the two windows they belong to.
  const win = /shared-worker\.html$/.test(location.pathname) ? 'shared-worker' : 'main';
  // Captured before `network-shim.js` replaces `fetch` with its proxied version:
  // bridge traffic must go straight to the loopback host, never through the
  // renderer's own proxy (that would recurse back into this handler).
  const nativeFetch = window.fetch.bind(window);

  window.__olazLinux = { base, debug, window: win };
  // Raw fetch for this host's own endpoints: the shim below sends `fetch`
  // through Rust to the Zalo origin, which is not where `/__olaz/…` lives.
  window.__olazLinux.fetch = (path, init) => nativeFetch(base + path, init);

  // ---------------------------------------------------------------- reporting
  // A webview that stalls has nowhere to say so: send page errors and the
  // synchronous bridge calls (the ones that can block the page) to the host
  // log. `--serve-only` in a terminal then shows the exact call it stopped at.
  const report = (text) => {
    try {
      nativeFetch(base + '/bridge/zlog', {
        method: 'POST',
        headers: jsonHeaders,
        body: JSON.stringify({ message: '[page] ' + text }),
      });
    } catch {}
  };
  if (debug) report('mở ' + location.pathname + ' top=' + (window.top === window));
  window.addEventListener('error', (ev) =>
    report('lỗi JS: ' + (ev.message || ev.type) + ' @ ' + (ev.filename || '?') + ':' + (ev.lineno || 0)),
  );
  window.addEventListener('unhandledrejection', (ev) => {
    const reason = ev.reason;
    report('promise bị từ chối: ' + String((reason && (reason.message || reason.stack)) || reason));
  });
  if (debug) {
    // What the page actually painted — the only view into a webview that has no
    // console forwarding.
    setTimeout(() => {
      const text = document.body ? document.body.innerText.replace(/\s+/g, ' ').slice(0, 240) : '';
      report(
        'DOM: ' + document.title + ' | canvas=' + document.querySelectorAll('canvas').length +
          ' img=' + document.images.length + ' | ' + text,
      );
    }, 8000);
  }

  // ---------------------------------------------------------------- URL rewrite
  // bridge.js builds `zalo-app://local/__olaz/bridge-sync`; that scheme only
  // existed because WKWebView needed a URL scheme handler. Here the same path
  // lives on the loopback server, and the sync request stays a real XHR so it
  // keeps its synchronous semantics.
  const open = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function (method, url, ...rest) {
    const raw = String(url);
    if (raw.indexOf('zalo-app://') === 0) {
      // `bridge.js` uses this scheme for exactly one thing: its synchronous
      // bridge endpoint. Everything else stays unreachable by design.
      const path = raw.slice('zalo-app://'.length);
      url = base + (/\/__olaz\/bridge-sync$/.test(path) ? '/sync' : '/');
    }
    this.__olazUrl = String(url);
    this.__olazSync = rest[0] === false;
    return open.call(this, method, url, ...rest);
  };
  // Sync XHR runs on the page's own thread, so one that never answers freezes
  // the renderer before it paints anything past its first HTML. Log each side.
  const send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function (body) {
    if (!this.__olazSync) return send.call(this, body);
    const target = this.__olazUrl.replace(base, '');
    if (debug) report('sync → ' + target + ' ' + String(body || '').slice(0, 300));
    try {
      const value = send.call(this, body);
      if (debug) report('sync ✓ ' + target + ' → ' + String(this.responseText || '').slice(0, 300));
      return value;
    } catch (err) {
      report('sync ✗ ' + target + ' — ' + err);
      throw err;
    }
  };

  // ---------------------------------------------------------------- message handlers
  const headers = { ...jsonHeaders, 'x-olaz-window': win };
  const rpc = (name, msg) =>
    nativeFetch(base + '/bridge/' + name, { method: 'POST', headers, body: JSON.stringify(msg) }).then(
      (res) =>
        res.json().then((body) => {
          if (!body || body.ok !== true) throw new Error((body && body.error) || name + ' bridge failed');
          return body.value;
        }),
      (err) => {
        throw new Error(name + ' bridge unreachable: ' + err);
      },
    );
  const fireAndForget = (name, msg) => {
    try {
      nativeFetch(base + '/bridge/' + name, { method: 'POST', headers, body: JSON.stringify(msg) });
    } catch {}
  };
  // Used by the diagnostics page: `fetch` there is the proxied one.
  window.__olazLinux.post = (message) => fireAndForget('zlog', { message: String(message) });

  const handlers = {
    zbridge: { postMessage: (msg) => rpc('zbridge', msg) },
    znet: { postMessage: (msg) => rpc('znet', msg) },
    zlog: { postMessage: (msg) => void fireAndForget('zlog', { message: String(msg == null ? '' : msg) }) },
    zstate: { postMessage: (msg) => void fireAndForget('zstate', msg) },
    zsqlite: { postMessage: () => Promise.reject(new Error('SQLite không dùng trên Linux (IndexedDB)')) },
  };
  // WebKitGTK already defines `window.webkit` — non-writable, non-configurable,
  // with its own (empty) `messageHandlers` — so the handlers have to be added to
  // that platform object. Redefining `window.webkit` throws and silently leaves
  // the renderer without a bridge, which is what kept it on its first paint.
  const installHandlers = () => {
    const target = window.webkit;
    if (!target) return false;
    const bag = target.messageHandlers;
    if (bag && typeof bag === 'object') {
      try {
        Object.assign(bag, handlers);
      } catch {}
      if (bag.zbridge) return true;
      try {
        for (const [name, value] of Object.entries(handlers))
          Object.defineProperty(bag, name, { configurable: true, writable: true, enumerable: true, value });
      } catch {}
      if (bag.zbridge) return true;
    }
    try {
      Object.defineProperty(target, 'messageHandlers', { configurable: true, writable: true, value: handlers });
      return true;
    } catch {}
    return false;
  };
  // The platform bag is a live object and does not keep `zbridge` reliably, so
  // bridge.js/network-shim.js resolve handlers through here first (see the
  // `handler` lookup added there); macOS has no `__olazLinux` and is unaffected.
  window.__olazLinux.handler = (name) => handlers[name];
  if (!installHandlers()) {
    try {
      Object.defineProperty(window, 'webkit', {
        configurable: true,
        writable: true,
        value: { messageHandlers: handlers },
      });
    } catch (err) {
      report('không gắn được messageHandlers: ' + err);
    }
  }

  // Way back to the status page from any renderer page, so a renderer that
  // stalls can never leave the window with no way to see what happened.
  window.addEventListener(
    'keydown',
    (ev) => {
      if (ev.ctrlKey && ev.shiftKey && (ev.key === 'D' || ev.key === 'd')) {
        ev.preventDefault();
        location.assign(base + '/__olaz/index.html');
      }
    },
    true,
  );

  // ---------------------------------------------------------------- host → page
  let cursor = 0;
  const dispatch = (ev) => {
    if (ev.target && ev.target !== win) return;
    if (devEval && ev.kind === 'dev-eval') {
      const id = ev.id;
      const code = String(ev.code || '');
      const sendResult = (result) =>
        nativeFetch(base + '/__olaz/dev/eval-result', {
          method: 'POST',
          headers: jsonHeaders,
          body: JSON.stringify({ id, result }),
        }).catch(() => {});
      Promise.resolve()
        .then(() => (0, eval)('(async()=>{' + code + '\n})()'))
        .then((value) => {
          let result;
          try {
            result = JSON.stringify(
              { ok: true, value },
              (key, item) => (typeof item === 'bigint' ? String(item) : item),
            );
          } catch (err) {
            result = JSON.stringify({ ok: false, error: 'serialize: ' + err });
          }
          return sendResult(result);
        })
        .catch((err) => sendResult(JSON.stringify({ ok: false, error: String((err && err.stack) || err) })));
      return;
    }
    // Only the main window navigates; a `navigate` broadcast would send the
    // hidden worker to the diagnostics or main page.
    if (ev.kind === 'navigate' && win !== 'main') return;
    const bridge = window.__zaloBridge;
    if (debug && ev.kind === 'log') console.log('[olaz]', ev.message);
    if (!bridge) return;
    if (ev.kind === 'socket' && window.__zaloNet && window.__zaloNet.socketEvent) {
      if (debug && win === 'main' && (ev.event === 'close' || ev.event === 'error' || ev.event === 'open')) {
        const p = ev.payload || {};
        report('ws ' + ev.id + ' ' + ev.event + (ev.event === 'close' ? ' code=' + p.code + ' reason=' + String(p.reason || '').slice(0, 120) : ''));
      }
      window.__zaloNet.socketEvent(ev.id, ev.event, ev.payload || {});
      return;
    }
    try {
      if (ev.kind === 'send') bridge.ipcEmit(ev.channel, ev.args || []);
      else if (ev.kind === 'emit') bridge.emit(ev.path, ev.args || []);
      else if (ev.kind === 'navigate') location.assign(base + ev.url);
    } catch (err) {
      setTimeout(() => {
        throw err;
      });
    }
  };
  const poll = () => {
    nativeFetch(base + '/poll?cursor=' + cursor, { headers: jsonHeaders })
      .then((res) => res.json())
      .then((body) => {
        if (typeof body.cursor === 'number') cursor = body.cursor;
        (body.events || []).forEach(dispatch);
      })
      .catch(() => {})
      .then(() => setTimeout(poll, 200));
  };
  setTimeout(poll, 250);
})();
