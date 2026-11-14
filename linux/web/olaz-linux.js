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
  // The host's second loopback port serves the two routes that hold a
  // connection for a long time (the long-poll and `/__olaz/file`). WebKitGTK
  // budgets ~6 connections per host, and the long-polls would pin two of them
  // permanently, starving photo requests to `base`. The aux port has its own
  // budget; its replies carry `Access-Control-Allow-Origin: *` because this is
  // a cross-origin fetch for the page.
  const base2 = 'http://127.0.0.1:' + (cfg.auxPort || cfg.port || 0);
  const jsonHeaders = { 'content-type': 'application/json' };
  const debug = !!cfg.debug;
  // Debug harness only (`OLAZ_DEV_EVAL=1`): the host sends `dev-eval` events and
  // waits for the result on `/__olaz/dev/eval-result`.
  const devEval = !!cfg.devEval;
  // Electron ran the hidden shared-worker page in its own window. Here it is a
  // second (hidden) Tauri webview on the same origin, so bridge pushes must say
  // which of the two windows they belong to. Popup viewer windows (photo/video)
  // carry their label via the `__olazWin` URL query (set when the host creates
  // the window for a renderer `window.open`).
  const win =
    new URLSearchParams(location.search).get('__olazWin') ||
    window.__OLAZ_LINUX_WINDOW ||
    (/shared-worker\.html$/.test(location.pathname) ? 'shared-worker' : 'main');
  // Captured before `network-shim.js` replaces `fetch` with its proxied version:
  // bridge traffic must go straight to the loopback host, never through the
  // renderer's own proxy (that would recurse back into this handler).
  const nativeFetch = window.fetch.bind(window);

  window.__olazLinux = { base, debug, window: win };
  // Raw fetch for this host's own endpoints: the shim below sends `fetch`
  // through Rust to the Zalo origin, which is not where `/__olaz/…` lives.
  window.__olazLinux.fetch = (path, init) => nativeFetch(base + path, init);

  // ------------------------------------------------- message body normalization
  // The official DAL stores every string `message` as XOR(UIN,
  // JSON.stringify(value)) and re-reveals it on read; a row that instead holds
  // wire-plaintext (sync/import) makes the DAL's reveal+JSON.parse throw
  // "Fail to transform data" whenever the text is base64-shaped ("Kaka",
  // "1930", …) — and that throws away the whole partition, so the conversation
  // hangs on "loading" with only newly sent messages visible.
  // Runs on every launch (no "done" marker): a later sync/import rewrites the
  // same rows as ciphertext again, and a marker would then leave them encrypted
  // forever. The pass itself is idempotent — every `message` string is brought
  // to the official DB format, XOR(UIN, JSON.stringify(value)):
  //   * decode XOR → parses as JSON → the row is already ciphertext-encoded
  //     (the sync import's shape) — and when the parsed value is a string it
  //     must STAY encoded: storing the bare string makes the DAL's
  //     reveal+JSON.parse fail on base64-shaped texts ("Kaka", "1930", …),
  //     which throws "Fail to transform data" and hangs that conversation.
  //   * decode fails → the row holds wire-plaintext → encode it.
  const decryptMessagesOnce = (userId, uin) => {
    if (!userId || !uin) {
      report('decrypt: thiếu userId/UIN');
      return;
    }
    const xorUin = (text) => {
      let out = '';
      for (let i = 0; i < text.length; i += 1) {
        out += String.fromCharCode(text.charCodeAt(i) ^ uin.charCodeAt(i % uin.length));
      }
      return out;
    };
    const request = indexedDB.open('zdb_' + userId);
    request.onerror = () => report('decrypt: không mở được zdb_' + userId);
    request.onsuccess = () => {
      const db = request.result;
      const names = Array.prototype.slice.call(db.objectStoreNames || []);
      if (names.indexOf('message') < 0) {
        report('decrypt: không có store message (' + names.join(',') + ')');
        db.close();
        return;
      }
      const read = db.transaction('message', 'readonly').objectStore('message').getAll();
      read.onerror = () => {
        report('decrypt: getAll lỗi');
        db.close();
      };
      read.onsuccess = () => {
        const rows = read.result || [];
        let seen = 0;
        let fixed = 0;
        const tx = db.transaction('message', 'readwrite');
        const store = tx.objectStore('message');
        rows.forEach((row) => {
          const body = row && row.message;
          if (typeof body !== 'string' || body.length < 2) return;
          seen += 1;
          let value = body;
          try {
            value = JSON.parse(xorUin(body));
          } catch (e) {}
          // Định dạng chính thức: giá trị sau reveal; encode lại trọn vẹn nên
          // row ciphertext giữ nguyên (idempotent) và row plaintext được sửa.
          const target = xorUin(JSON.stringify(value));
          if (target !== body) {
            row.message = target;
            store.put(row);
            fixed += 1;
          }
        });
        tx.oncomplete = () => {
          db.close();
          report('decrypt: ' + fixed + '/' + seen + ' dòng đã chuẩn hoá (' + rows.length + ' dòng trong store)');
          // The photo warmer needs the decoded `params`: chain it here instead
          // of a fixed delay so a slow repair still feeds the cache pass.
          // A later sync/import rewrites the same rows as ciphertext again, so
          // a repair that fixed anything re-arms the one-shot warmer.
          try {
            if (fixed > 0) window.__olazPhotoWarm = false;
            void warmPhotoCaches(userId);
          } catch (e) {}
        };
        tx.onerror = () => {
          report('decrypt: ghi DB lỗi ' + (tx.error && tx.error.message));
          try {
            db.close();
          } catch (e) {}
        };
      };
    };
  };
  const scheduleMessageDecrypt = (userId, uin) => {
    if (!userId || !uin) return;
    try {
      localStorage.setItem('sh_olaz_session', JSON.stringify({ userId: userId, UIN: uin }));
    } catch (e) {}
    if (window.__olazDecryptScheduled) return;
    window.__olazDecryptScheduled = true;
    setTimeout(() => decryptMessagesOnce(userId, uin), 4000);
    // The sync import keeps rewriting rows as ciphertext after the first pass,
    // so re-scan on a slow cadence. The pass only rewrites rows that still
    // need it, and the warmer only re-runs when something was repaired.
    // The 90 s re-scan moved into the message-growth watcher below: at 130k
    // rows a pass reads the whole store and stalls the main thread — running
    // it unconditionally was the visible lag, and rows only change when new
    // messages arrive anyway.
  };
  window.__olazLinux.decryptMessagesOnce = decryptMessagesOnce;
  // The bridge replays the session on `renderer-ready`, but that can land after
  // this page started; the stored copy lets a plain restart repair history.
  try {
    const savedSession = JSON.parse(localStorage.getItem('sh_olaz_session') || 'null');
    if (savedSession && savedSession.userId && savedSession.UIN) {
      setTimeout(() => scheduleMessageDecrypt(savedSession.userId, savedSession.UIN), 8000);
    }
  } catch (e) {}

  // ------------------------------------------------------- photo cache warmer
  // The bubble's `<img>` is the official photo component reading
  // `ZaloDownloads/resource/<conv>/Cache/<cliMsgId>_<fromUid>_<toUid>_<quality>`.
  // Those files come from the native gen-thumb path and the official multi-source
  // downloader, whose module chain throws on Linux (`resolveMedia` answers
  // `reason: 'error'`), so a photo of a message this wrapper never downloaded keeps
  // its blurred LQIP. Fetch the CDN copy through the host downloader instead and
  // write the four variants the component probes.
  const PHOTO_URL_KEYS = ['oriUrl', 'hdUrl', 'normalUrl', 'thumbUrl', 'href'];
  // Every CDN variant of a row: the bubble may read `thumbUrl` while the
  // downloader preferred `oriUrl`, so the error-swap map needs all of them.
  const photoUrls = (row) => {
    const found = [];
    try {
      const body = row && row.message;
      if (!body || typeof body !== 'object') return found;
      let params = body.params;
      if (typeof params === 'string') {
        try { params = JSON.parse(params); } catch (e) { params = null; }
      }
      for (const source of [body, params]) {
        if (!source || typeof source !== 'object') continue;
        for (const key of PHOTO_URL_KEYS) {
          const value = source[key];
          if (typeof value === 'string' && value.indexOf('https://') === 0 && found.indexOf(value) < 0) {
            found.push(value);
          }
        }
      }
    } catch (e) {}
    return found;
  };
  const photoTargets = (rows, userId) => {
    const targets = [];
    const seen = new Set();
    for (const row of rows || []) {
      const body = row && row.message;
      if (!body || typeof body !== 'object') continue;
      let params = body.params;
      if (typeof params === 'string') {
        try { params = JSON.parse(params); } catch (e) { continue; }
      }
      if (!params || typeof params !== 'object') continue;
      // The photo payload keeps its URLs at both levels: `thumbUrl`/`oriUrl` sit on the
      // message object, `normalUrl`/`hdUrl` inside the JSON `params` string.
      const sources = [body, params];
      const url = PHOTO_URL_KEYS.map((key) => {
        for (const source of sources) {
          const value = source[key];
          if (typeof value === 'string' && value.indexOf('https://') === 0) return value;
        }
        return '';
      }).find((value) => !!value);
      // The bubble probes a different path per quality — the thumbnail is
      // `<stem>_t<ext>` under `Cache/`, every other quality folds `md5(url)` in under
      // `picture/`. Send every quality the row carries with the URL it belongs to, so
      // each file is written where the component will look for it.
      const qualities = [];
      for (const quality of PHOTO_URL_KEYS) {
        if (quality === 'href') continue;
        for (const source of sources) {
          const value = source[quality];
          if (typeof value === 'string' && value.indexOf('https://') === 0) {
            qualities.push([quality, value]);
            break;
          }
        }
      }
      let fromUid = row.fromUid ? String(row.fromUid) : '';
      if (fromUid === '0' || fromUid === '') fromUid = String(userId);
      let convId = row.toUid ? String(row.toUid) : '';
      if (convId === '0') convId = String(userId);
      const msgId = row.cliMsgId ? String(row.cliMsgId) : row.msgId ? String(row.msgId) : '';
      if (!qualities.length || !convId || !msgId) continue;
      const stem = msgId + '_' + fromUid + '_' + convId;
      if (seen.has(stem)) continue;
      seen.add(stem);
      targets.push({
        convId: convId,
        stem: stem,
        url: qualities[0][1],
        qualities: qualities,
        urls: photoUrls(row),
        userId: userId,
      });
    }
    return targets.slice(0, 400);
  };
  const readMessageRows = (userId) =>
    new Promise((resolve) => {
      const request = indexedDB.open('zdb_' + userId);
      request.onerror = () => resolve([]);
      request.onsuccess = () => {
        const db = request.result;
        const names = Array.prototype.slice.call(db.objectStoreNames || []);
        if (names.indexOf('message') < 0) {
          db.close();
          resolve([]);
          return;
        }
        const read = db.transaction('message', 'readonly').objectStore('message').getAll();
        read.onerror = () => {
          db.close();
          resolve([]);
        };
        read.onsuccess = () => {
          const rows = read.result || [];
          db.close();
          resolve(rows);
        };
      };
    });
  const warmPhotoCaches = async (userId) => {
    if (!userId || window.__olazPhotoWarm) return;
    const bridge = window.__zaloBridge;
    if (!bridge || typeof bridge.callAsync !== 'function') return;
    window.__olazPhotoWarm = true;
    try {
      const rows = await readMessageRows(userId);
      // Newest first: the store key order is not recency, and after a phone→PC
      // sync the oldest rows' CDN URLs have long expired — warming those first
      // burns the whole budget (400) on dead links.
      rows.sort((a, b) => (Number(b && b.sendDttm) || 0) - (Number(a && a.sendDttm) || 0));
      const targets = photoTargets(rows, userId);
      if (!targets.length) {
        const objects = (rows || []).filter((row) => row && row.message && typeof row.message === 'object').length;
        report('photo cache: không có tin ảnh (rows=' + (rows || []).length + ', object=' + objects + ')');
        return;
      }
      let downloaded = 0;
      let present = 0;
      let failed = 0;
      const queue = targets.slice();
      const worker = async () => {
        for (;;) {
          const target = queue.shift();
          if (!target) return;          try {
            const outcome = await bridge.callAsync('$olaz.photo.cache', [target]);
            const state = outcome && typeof outcome === 'object' ? outcome.state : '';
            if (state === 'ok') downloaded += 1;
            else if (state === 'cached') present += 1;
            else failed += 1;
            if ((state === 'ok' || state === 'cached') && outcome.path) {
              rememberPhotoFile(target.url, outcome.path);
              rememberPhotoStem(target.stem, outcome.path);
              rememberConvPhoto(target.convId, target.stem, outcome.path);
              for (const u of target.urls || []) rememberPhotoFile(u, outcome.path);
            }
          } catch (e) { failed += 1; }
        }
      };
      await Promise.all([worker(), worker(), worker()]);
      report(
        'photo cache: ' + (downloaded + present) + '/' + targets.length + ' (' + downloaded +
          ' tải, ' + present + ' có sẵn, ' + failed + ' lỗi)',
      );
      // The component probes the cache once, when the bubble mounts: remount the
      // conversation that is open so photos that just landed leave their LQIP.
      const conv = window.__olazLastConv;
      if (downloaded > 0 && conv && window.__zaloChat && typeof window.__zaloChat.open === 'function') {
        try {
          window.__zaloChat.open({ id: conv });
        } catch (e) {}
      }
    } catch (err) {
      report('photo cache lỗi: ' + err);
    }
  };
  // Remember which conversation is on screen, so the warmer can remount it.
  const trackConversation = () => {
    const chat = window.__zaloChat;
    if (!chat || typeof chat.open !== 'function' || chat.__olazTrackedOpen) return;
    const previous = chat.open;
    chat.open = function (target) {
      try {
        window.__olazLastConv = typeof target === 'string' ? target : (target && (target.id || target.convId));
      } catch (e) {}
      try {
        scheduleBubbleScan();
      } catch (e) {}
      return previous.apply(this, arguments);
    };
    chat.__olazTrackedOpen = true;
  };
  for (const delay of [1000, 3000, 6000, 12000]) setTimeout(trackConversation, delay);
  window.__olazLinux.warmPhotoCaches = warmPhotoCaches;

  // ---------------------------------------------------- warmer re-arm on growth
  // Messages land in the store long after boot: the phone→PC sync imports tens
  // of thousands over many minutes, and realtime messages arrive continuously.
  // The warmer runs once per launch, so synced history would never get
  // thumbnails. Watch the message-store row count — a cheap `count()` query —
  // and re-run the warmer whenever a batch of new rows appeared.
  const scheduleWarmerOnGrowth = (userId, uin) => {
    if (!userId || window.__olazGrowthWatch) return;
    window.__olazGrowthWatch = true;
    let lastCount = 0;
    const countRows = () => new Promise((resolve) => {
      const request = indexedDB.open('zdb_' + userId);
      request.onerror = () => resolve(0);
      request.onsuccess = () => {
        const db = request.result;
        let count = 0;
        try {
          const query = db.transaction('message', 'readonly').objectStore('message').count();
          query.onsuccess = () => { count = query.result; };
        } catch (e) {}
        setTimeout(() => { db.close(); resolve(count); }, 50);
      };
    });
    const tick = async () => {
      try {
        const count = await countRows();
        const grew = count - lastCount;
        if (lastCount > 0 && grew >= 20) {
          report('message store +' + grew + ' dòng — chạy lại photo warmer');
          // New rows may be XOR-encrypted (the sync import writes them that
          // way); decrypt first — it re-arms the warmer when it fixed rows.
          try { if (uin) decryptMessagesOnce(userId, uin); } catch (e) {}
          try { warmPhotoCaches(userId); } catch (e) {}
        }
        lastCount = count;
      } catch (e) {}
    };
    setTimeout(tick, 30000);
    setInterval(tick, 60000);
  };
  const scheduleWarmerOnGrowthWrap = () => {
    const session = (() => { try { return JSON.parse(localStorage.getItem('sh_olaz_session') || 'null'); } catch (e) { return null; } })();
    if (session && session.userId) scheduleWarmerOnGrowth(String(session.userId), String(session.UIN || ''));
    return false;
  };
  scheduleWarmerOnGrowthWrap();
  for (const delay of [5000, 15000, 30000]) setTimeout(scheduleWarmerOnGrowthWrap, delay);

  // ------------------------------------------ resolveMedia through the cache
  // The official photo path (multi-source `loadPC`) throws inside its module
  // chain on Linux, so `resolveMedia(..., 'photo')` answers `reason: 'error'`
  // even though the CDN bytes are reachable. The warmer above already stores the
  // same file the bubble reads, so resolve through that first and only fall back
  // to the official path when there is no cacheable photo.
  const findMessageRow = (userId, convId, msgId) =>
    new Promise((resolve) => {
      const request = indexedDB.open('zdb_' + userId);
      request.onerror = () => resolve(null);
      request.onsuccess = () => {
        const db = request.result;
        let store;
        try {
          store = db.transaction('message', 'readonly').objectStore('message');
        } catch (e) {
          db.close();
          resolve(null);
          return;
        }
        const cursor = store.openCursor();
        cursor.onsuccess = (event) => {
          const step = event.target.result;
          if (!step) {
            db.close();
            resolve(null);
            return;
          }
          const row = step.value;
          if (
            row && String(row.toUid || '') === convId &&
            (String(row.msgId || '') === msgId || String(row.cliMsgId || '') === msgId)
          ) {
            db.close();
            resolve(row);
            return;
          }
          step.continue();
        };
        cursor.onerror = () => {
          db.close();
          resolve(null);
        };
      };
    });
  const resolvePhotoViaCache = async (convId, msgId) => {
    try {
      const session = JSON.parse(localStorage.getItem('sh_olaz_session') || 'null');
      if (!session || !session.userId) return null;
      const row = await findMessageRow(session.userId, String(convId), String(msgId));
      if (!row) return null;
      const targets = photoTargets([row], session.userId);
      if (!targets.length) return null;
      const bridge = window.__zaloBridge;
      if (!bridge || typeof bridge.callAsync !== 'function') return null;
      const outcome = await bridge.callAsync('$olaz.photo.cache', [targets[0]]);
      if (!outcome || typeof outcome !== 'object') return null;
      const paths = Array.isArray(outcome.paths) ? outcome.paths : [];
      // The map order follows the qualities the row carries, so the last entry is the
      // highest quality actually written; `outcome.path` already points at it.
      const best = outcome.path || paths[paths.length - 1];
      if ((outcome.state === 'ok' || outcome.state === 'cached') && typeof best === 'string' && best) {
        return { state: 'ok', path: best, source: outcome.state === 'ok' ? 'download' : 'local' };
      }
      return null;
    } catch (e) {
      return null;
    }
  };
  const wrapResolveMedia = () => {
    const chat = window.__zaloChat;
    if (!chat || typeof chat.resolveMedia !== 'function' || chat.__olazResolveWrapped) {
      return !! (chat && chat.__olazResolveWrapped);
    }
    const previous = chat.resolveMedia;
    chat.resolveMedia = function (convId, msgId, kind) {
      if (kind === 'photo' || kind === 'videoThumb' || kind === undefined) {
        return resolvePhotoViaCache(convId, msgId).then((hit) => {
          if (hit) return hit;
          // The official downloader can hang on Linux; answer instead of leaving
          // the caller — and its `mediaFlights` entry — stuck, while a late
          // official result still settles the race harmlessly.
          return Promise.race([
            previous.call(this, convId, msgId, kind),
            new Promise((resolve) =>
              setTimeout(() => resolve({ state: 'unavailable', reason: 'timeout' }), 25000),
            ),
          ]);
        });
      }
      return previous.call(this, convId, msgId, kind);
    };
    chat.__olazResolveWrapped = true;
    report('resolveMedia: ảnh dùng cache local trước');
    return true;
  };
  const wrapTimer = setInterval(() => {
    try {
      if (wrapResolveMedia()) clearInterval(wrapTimer);
    } catch (e) {}
  }, 1500);
  setTimeout(() => clearInterval(wrapTimer), 30000);

  // --------------------------------- bubble thumbnails via the local cache
  // Avatar (`<img src=https://...avatar...>`) paints, but a photo bubble's
  // `<img>` stays blank: its CDN URL is expired (old synced history) or points
  // at bytes WebKitGTK cannot decode. The warmer already stores the same photo
  // as a plain JPEG under the probed cache name, so map every warmed CDN URL
  // to its local file and swap a failing bubble image to it. Untouched photos
  // (and avatars) keep loading exactly as before.
  const photoUrlMap = {};
  const photoStemMap = {};
  const fileUrlFor = (diskPath) => base2 + '/__olaz/file?path=' + encodeURIComponent(diskPath);
  const rememberPhotoFile = (cdnUrl, diskPath) => {
    if (typeof cdnUrl !== 'string' || !cdnUrl || typeof diskPath !== 'string' || !diskPath) return;
    const local = fileUrlFor(diskPath);
    photoUrlMap[cdnUrl] = local;
    const bare = cdnUrl.split('?')[0];
    if (!photoUrlMap[bare]) photoUrlMap[bare] = local;
  };
  const rememberPhotoStem = (stem, diskPath) => {
    if (typeof stem !== 'string' || !stem || typeof diskPath !== 'string' || !diskPath) return;
    if (!photoStemMap[stem]) photoStemMap[stem] = fileUrlFor(diskPath);
    // The image loader keys a photo as `${cli}.${from}.${to}` while the cache
    // stem is `${cli}_${from}_${to}`: index both spellings.
    const dotted = stem.replace(/_/g, '.');
    if (!photoStemMap[dotted]) photoStemMap[dotted] = fileUrlFor(diskPath);
  };
  // convId → [{cli, stem, local}]: the DOM injection below pairs bubbles with
  // these in message order when a bubble carries no readable id.
  const convPhotoLists = {};
  const rememberConvPhoto = (convId, stem, diskPath) => {
    if (!convId || !stem || !diskPath) return;
    const list = convPhotoLists[convId] || (convPhotoLists[convId] = []);
    if (list.some((it) => it.stem === stem)) return;
    list.push({ cli: stem.split('_')[0], stem: stem, local: fileUrlFor(diskPath) });
  };
  // Resolve a bubble/viewer image to its warmed JPEG. Three shapes occur:
  // a CDN `https://photo-…` URL (expired/JXL), a `file://` cache path (which
  // WebKitGTK refuses to load from an http:// page), or a cache filename stem
  // `<cli>_<from>_<to>_<q>` embedded anywhere in the URL. Avatars are plain
  // https and never match any of these, so they are left untouched.
  const localForImgSrc = (src) => {
    if (typeof src !== 'string' || !src) return '';
    const hit = photoUrlMap[src] || photoUrlMap[src.split('?')[0]];
    if (hit) return hit;
    if (src.indexOf('file://') === 0) {
      try {
        const diskPath = decodeURIComponent(src.replace(/^file:\/\/+/, '/').split('?')[0]);
        if (diskPath.indexOf('.jpg') > 0 || diskPath.indexOf('.jpeg') > 0 || diskPath.indexOf('Cache') >= 0) {
          return fileUrlFor(diskPath);
        }
      } catch (e) {}
    }
    const m = src.match(/(\d+_\d+_[0-9]+)_[a-z]+(\.[a-z]+)?(\?|$)/i);
    if (m && photoStemMap[m[1]]) return photoStemMap[m[1]];
    // Loader mediaId `${cli}.${from}.${to}` embedded in a URL or key.
    const d = src.match(/(\d+\.\d+\.[0-9]+)/);
    if (d && photoStemMap[d[1]]) return photoStemMap[d[1]];
    // Bare stem `${cli}_${from}_${to}` without a quality suffix.
    const b = src.match(/(\d+_\d+_[0-9]+)/);
    if (b && photoStemMap[b[1]]) return photoStemMap[b[1]];
    return '';
  };
  // The image loader fetches the CDN bytes itself (fetch/XHR through the
  // proxied network stack), never as an `<img src>`, so swapping `<img>`
  // tags cannot rescue it. Redirect the request to the warmed JPEG instead:
  // same photo, plain baseline JPEG the WebKit decoder handles.
  const redirectPhotoRequest = (url) => {
    if (typeof url !== 'string' || !url) return url;
    if (url.indexOf(base + '/__olaz/file') === 0) return url;
    // Avatars and stickers are plain https images that paint fine — only
    // rewrite URLs that belong to a warmed photo.
    if (url.indexOf('https://') !== 0 && url.indexOf('file://') !== 0) return url;
    const local = localForImgSrc(url);
    return local || url;
  };
  const wrapFetchOnce = () => {
    try {
      if (typeof window.fetch !== 'function' || window.fetch.__olazWrapped) return true;
      const prev = window.fetch.bind(window);
      const wrapped = function (input, init) {
        try {
          const url = typeof input === 'string' ? input : (input && input.url);
          const next = redirectPhotoRequest(url);
          if (next && next !== url) {
            if (typeof input === 'string') input = next;
            else if (input && typeof Request === 'function' && input instanceof Request) {
              input = new Request(next, input);
            }
          }
        } catch (e) {}
        return prev(input, init);
      };
      wrapped.__olazWrapped = true;
      window.fetch = wrapped;
      return true;
    } catch (e) {
      return false;
    }
  };
  // `network-shim.js` replaces `fetch` after this shim runs, so re-wrap
  // whatever is current whenever a conversation opens.
  const wrapXhrOnce = () => {
    try {
      if (!XMLHttpRequest.prototype.open.__olazWrapped) {
        const prevOpen = XMLHttpRequest.prototype.open;
        const nextOpen = function (method, url, ...rest) {
          try {
            url = redirectPhotoRequest(String(url));
          } catch (e) {}
          return prevOpen.call(this, method, url, ...rest);
        };
        nextOpen.__olazWrapped = true;
        XMLHttpRequest.prototype.open = nextOpen;
      }
      return true;
    } catch (e) {
      return false;
    }
  };
  const swapImgToLocal = (img, why) => {
    try {
      if (!img || img.tagName !== 'IMG') return false;
      const stage = img.__olazSwapStage || 0;
      if (stage >= 2) return false;
      const src = img.currentSrc || img.src || '';
      if (!src || src.indexOf(base + '/__olaz/file') === 0) return false;
      const local = localForImgSrc(String(src));
      if (!local) return false;
      img.__olazSwapStage = stage + 1;
      report('bubble img (' + why + ') → dùng cache local: ' + String(src).slice(-90));
      img.src = local;
      return true;
    } catch (e) {
      return false;
    }
  };
  window.addEventListener(
    'error',
    (event) => {
      const img = event && event.target;
      if (!img || img.tagName !== 'IMG') return;
      swapImgToLocal(img, 'lỗi');
    },
    true,
  );
  // The bubble may never fire `error` (empty src, background layer, or the
  // loader holding an error state), so after a conversation opens, rewrite
  // matching images proactively — including the photo viewer modal, whose
  // `<img>` usually carries the same CDN/file URL as the bubble.
  const proactiveBubbleFix = () => {
    let fixed = 0;
    try {
      const imgs = document.images;
      for (let k = 0; k < imgs.length; k += 1) {
        if (swapImgToLocal(imgs[k], 'quét')) fixed += 1;
      }
    } catch (e) {}
    try {
      const nodes = document.querySelectorAll('[class*=photo-message], [class*=PhotoMessage]');
      for (let n = 0; n < nodes.length; n += 1) {
        const els = nodes[n].querySelectorAll('*');
        for (let k = 0; k < els.length; k += 1) {
          const el = els[k];
          if (el.tagName === 'IMG' || el.__olazBgSwapped) continue;
          let bi = '';
          try {
            bi = el.style && el.style.backgroundImage;
          } catch (e) {}
          if (!bi || bi.indexOf('url(') < 0) continue;
          const m = bi.match(/url\(["']?([^"')]+)["']?\)/);
          const local = m && localForImgSrc(m[1]);
          if (!local) continue;
          el.__olazBgSwapped = true;
          try {
            el.style.backgroundImage = 'url("' + local + '")';
            fixed += 1;
          } catch (e) {}
        }
      }
    } catch (e) {}
    return fixed;
  };
  // Own lightbox: the official viewer renders in an Electron child window
  // (`photoviewer_popup_mode == 2` → `/popup-viewer.html`), which this host
  // does not have, so a thumbnail click did nothing. Open our own overlay:
  // the gallery is every photo bubble currently mounted, arrow keys and the
  // side buttons navigate, and the largest cached variant wins (try `_o`,
  // fall back to the thumbnail the bubble is already showing).
  const lightboxState = { items: [], index: 0 };
  const lightboxUpgrade = (src) => {
    if (src.indexOf('/Cache/') < 0) return [src];
    const candidates = [];
    for (const quality of ['_o', '_h']) {
      const upgraded = src.replace(/_(t|n|h|o)(\.[a-z0-9]+)?(\?|$)/i, quality + '$2$3');
      if (upgraded !== src && candidates.indexOf(upgraded) < 0) candidates.push(upgraded);
    }
    candidates.push(src);
    return candidates;
  };
  const lightboxClose = () => {
    const box = document.getElementById('__olazLightbox');
    if (box) box.remove();
    lightboxState.items = [];
    window.removeEventListener('keydown', lightboxKeys, true);
  };
  const lightboxKeys = (ev) => {
    if (ev.key === 'Escape') { lightboxClose(); return; }
    if (lightboxState.items.length < 2) return;
    if (ev.key === 'ArrowLeft') { lightboxShow(lightboxState.index - 1); ev.preventDefault(); }
    if (ev.key === 'ArrowRight') { lightboxShow(lightboxState.index + 1); ev.preventDefault(); }
  };
  const lightboxShow = (index) => {
    const box = document.getElementById('__olazLightbox');
    if (!box || !lightboxState.items.length) return;
    const total = lightboxState.items.length;
    lightboxState.index = ((index % total) + total) % total;
    const src = lightboxState.items[lightboxState.index];
    const img = box.querySelector('img');
    const counter = box.querySelector('[data-counter]');
    if (counter) counter.textContent = (lightboxState.index + 1) + '/' + total;
    if (!img) return;
    const candidates = lightboxUpgrade(src);
    let at = 0;
    img.style.opacity = '0.4';
    img.onerror = () => {
      at += 1;
      if (at < candidates.length) img.src = candidates[at];
      else img.onerror = null;
    };
    img.onload = () => { img.style.opacity = '1'; };
    img.src = candidates[0];
  };
  const openOlazLightbox = (local, caption) => {
    try {
      lightboxClose();
      const seen = new Set();
      const items = [];
      for (const el of document.querySelectorAll('.img-center-box img')) {
        const src = el.currentSrc || el.src || '';
        if (src.indexOf('/__olaz/file') < 0 || el.naturalWidth === 0) continue;
        const path = decodeURIComponent((src.split('path=')[1] || '').split('&')[0]);
        if (seen.has(path)) continue;
        seen.add(path);
        items.push(src);
      }
      if (!items.length) items.push(local);
      let index = items.indexOf(local);
      if (index < 0) { items.unshift(local); index = 0; }
      lightboxState.items = items;
      lightboxState.index = index;
      const box = document.createElement('div');
      box.id = '__olazLightbox';
      box.setAttribute(
        'style',
        'position:fixed;inset:0;z-index:999999;background:rgba(0,0,0,.88);' +
          'display:flex;align-items:center;justify-content:center;cursor:zoom-out;user-select:none;',
      );
      const img = document.createElement('img');
      img.alt = caption || 'photo';
      img.setAttribute(
        'style',
        'max-width:92vw;max-height:88vh;object-fit:contain;border-radius:8px;' +
          'box-shadow:0 8px 60px rgba(0,0,0,.6);transition:opacity .15s;',
      );
      box.appendChild(img);
      const mkButton = (text, side) => {
        if (items.length < 2) return null;
        const b = document.createElement('div');
        b.textContent = text;
        b.setAttribute(
          'style',
          'position:absolute;top:50%;' + side + ':24px;transform:translateY(-50%);' +
            'width:44px;height:44px;line-height:44px;text-align:center;border-radius:50%;' +
            'background:rgba(255,255,255,.12);color:#fff;font-size:26px;cursor:pointer;',
        );
        b.addEventListener('click', (ev) => {
          ev.stopPropagation();
          lightboxShow(lightboxState.index + (text === '‹' ? -1 : 1));
        });
        return b;
      };
      const prev = mkButton('‹', 'left');
      if (prev) box.appendChild(prev);
      const next = mkButton('›', 'right');
      if (next) box.appendChild(next);
      if (items.length > 1) {
        const counter = document.createElement('div');
        counter.setAttribute('data-counter', '1');
        counter.setAttribute(
          'style',
          'position:absolute;bottom:18px;left:50%;transform:translateX(-50%);' +
            'color:rgba(255,255,255,.85);font-size:14px;background:rgba(0,0,0,.4);' +
            'padding:4px 12px;border-radius:12px;cursor:default;',
        );
        box.appendChild(counter);
      }
      const close = document.createElement('div');
      close.textContent = '×';
      close.setAttribute(
        'style',
        'position:absolute;top:14px;right:20px;color:#fff;font-size:30px;' +
          'cursor:pointer;width:40px;height:40px;line-height:40px;text-align:center;',
      );
      close.addEventListener('click', (ev) => { ev.stopPropagation(); lightboxClose(); });
      box.appendChild(close);
      box.addEventListener('click', (ev) => {
        if (ev.target === box) lightboxClose();
      });
      document.body.appendChild(box);
      window.addEventListener('keydown', lightboxKeys, true);
      lightboxShow(index);
      report('modal ảnh: mở ' + String(local).slice(-60));
    } catch (e) {}
  };
  // The official viewer needs a child window, so route bubble-photo clicks to
  // the lightbox instead. Capture phase, before the component's own handler;
  // skipped while multi-select is active (clicks pick messages there).
  document.addEventListener(
    'click',
    (ev) => {
      try {
        const img = ev.target;
        if (!img || img.tagName !== 'IMG') return;
        const src = img.currentSrc || img.src || '';
        if (src.indexOf(base2 + '/__olaz/file') < 0 && src.indexOf(base + '/__olaz/file') < 0) return;
        if (!img.closest('.img-center-box')) return;
        if (document.querySelector('[class*=multisel]:not([class*="false"]), [class*=multi-select]')) {
          if (img.closest('[class*=selected]')) return;
        }
        ev.preventDefault();
        ev.stopPropagation();
        openOlazLightbox(src, '');
      } catch (e) {}
    },
    true,
  );
  // Last resort when the official loader holds no pixels (expired CDN, JXL it
  // cannot decode, or a `file://` read WebKitGTK blocks): paint our warmed
  // JPEG into the bubble box directly. Matching prefers a cliMsgId found in
  // the bubble's own attributes, and falls back to message order.
  const injectBubblePhotos = (convId) => {
    let injected = 0;
    try {
      const list = convPhotoLists[convId] || [];
      if (!list.length) return 0;
      const byCli = {};
      for (const it of list) byCli[it.cli] = it;
      const sels = [
        '[class*=photo-message]', '[class*=PhotoMessage]', '[class*=chatImageMessage]',
        'div[id*=Photo]', 'div[id*=Image]', '[data-qId]',
      ];
      let nodes = [];
      for (const sel of sels) {
        try {
          nodes = nodes.concat(Array.from(document.querySelectorAll(sel)));
        } catch (e) {}
      }
      // Keep outermost boxes only: drop a node nested inside another hit.
      nodes = nodes.filter((el, i) => nodes.every((other, j) => i === j || !other.contains(el) || other === el));
      if (!nodes.length) return 0;
      const unmatched = list.slice();
      for (const node of nodes) {
        try {
          if (node.__olazInjected) continue;
          let html = '';
          try {
            html = (node.id || '') + ' ' + (node.getAttribute('data-id') || '') + ' ' +
              (node.getAttribute('data-qid') || node.getAttribute('data-qId') || '');
          } catch (e) {}
          let item = null;
          const m = String(html).match(/(\d{8,})/);
          if (m && byCli[m[1]]) {
            item = byCli[m[1]];
            const ix = unmatched.indexOf(item);
            if (ix >= 0) unmatched.splice(ix, 1);
          } else if (unmatched.length) {
            // Bubbles mount in message order; consume the next photo.
            item = unmatched.shift();
          }
          if (!item) continue;
          const healthy = Array.from(node.querySelectorAll('img')).some((im) => im.naturalWidth > 1);
          if (healthy) continue;
          let box = node.querySelector('.chat-message-picture__error, [class*=photooverlay], [class*=bg-color]') || node;
          const img = document.createElement('img');
          img.src = item.local;
          img.alt = 'photo';
          img.setAttribute(
            'style',
            'width:100%;height:100%;max-width:320px;max-height:320px;' +
              'object-fit:cover;border-radius:6px;display:block;cursor:zoom-in;',
          );
          img.addEventListener('click', (ev) => {
            try { ev.stopPropagation(); } catch (e) {}
            openOlazLightbox(item.local, item.cli);
          });
          try { box.insertBefore(img, box.firstChild); } catch (e) { box.appendChild(img); }
          node.__olazInjected = true;
          injected += 1;
        } catch (e) {}
      }
    } catch (e) {}
    return injected;
  };
  // Passive check, so a headless run can still tell whether thumbnails paint:
  // a few seconds after a conversation opens, count its bubble images that
  // decoded (naturalWidth > 1) versus the ones still blank.
  const scheduleBubbleScan = () => {
    // Fix first, then measure: bubbles mount lazily while scrolling, so sweep
    // a few times and only report the final painted state.
    try {
      wrapFetchOnce();
      wrapXhrOnce();
      proactiveBubbleFix();
    } catch (e) {}
    const convId = window.__olazLastConv;
    if (convId) {
      try {
        const n = injectBubblePhotos(convId);
        if (n) report('bubble: tự vẽ ' + n + ' thumbnail từ cache (' + convId + ')');
      } catch (e) {}
    }
    let sweep = 0;
    const sweeper = setInterval(() => {
      try {
        wrapFetchOnce();
        wrapXhrOnce();
        proactiveBubbleFix();
        retriggerWedgedImages();
        if (convId) injectBubblePhotos(convId);
      } catch (e) {}
      sweep += 1;
      if (sweep >= 5) {
        try {
          clearInterval(sweeper);
        } catch (e) {}
      }
    }, 3000);
    setTimeout(() => {
      try {
        const nodes = document.querySelectorAll('[class*=photo-message], [class*=PhotoMessage]');
        const imgs = Array.from(document.querySelectorAll('[class*=photo-message] img, [class*=PhotoMessage] img')).slice(0, 12);
        let ok = 0;
        const bad = [];
        for (const img of imgs) {
          if (img.naturalWidth > 1) ok += 1;
          else if (bad.length < 3) bad.push(String(img.currentSrc || img.src || '').slice(-80));
        }
        let extra = '';
        if (!imgs.length && nodes.length) {
          const kids = Array.from(nodes[0].querySelectorAll('*'))
            .slice(0, 8)
            .map((el) => el.tagName + '.' + String(el.className || '').slice(0, 40));
          extra = ' | mẫu: ' + kids.join(' ');
        }
        if (!nodes.length) {
          const photoImgs = Array.from(document.images).filter((img) =>
            /photo|thumb|jxl|image-msg/i.test(img.currentSrc || img.src || ''),
          );
          const lines = photoImgs.slice(0, 4).map((img) => {
            const chain = [];
            let el = img;
            for (let i = 0; i < 3 && el; i += 1) {
              chain.push(el.tagName + '.' + String(el.className || '').slice(0, 40));
              el = el.parentElement;
            }
            return img.naturalWidth + 'x' + img.naturalHeight + ' ' +
              String(img.currentSrc || img.src).slice(-70) + ' <= ' + chain.join(' < ');
          });
          const bg = [];
          try {
            const all = document.querySelectorAll('*');
            for (let i = 0; i < all.length && bg.length < 3; i += 1) {
              const bi = all[i].style && all[i].style.backgroundImage;
              if (bi && /photo|thumb|jxl/i.test(bi)) {
                bg.push(all[i].tagName + '.' + String(all[i].className).slice(0, 40) + ' ' + bi.slice(0, 90));
              }
            }
          } catch (e) {}
          extra += ' | photoImgs=' + photoImgs.length +
            (lines.length ? ' [' + lines.join(' || ') + ']' : '') +
            (bg.length ? ' bg=[' + bg.join(' || ') + ']' : '');
        }
        const summary =
          'bubble ảnh: nodes=' + nodes.length + ' imgs=' + imgs.length + ' hiện=' + ok +
          (bad.length ? ' | lỗi: ' + bad.join(' || ') : '') + extra;
        report(summary);
        // The user runs the AppImage by double-click (no terminal log), so
        // mirror the painted/broken counts into the window title briefly.
        try {
          if (!window.__olazTitleOrig) window.__olazTitleOrig = document.title;
          document.title =
            '[Olaz] bubble hiện ' + ok + '/' + imgs.length + ' | ' + window.__olazTitleOrig;
          setTimeout(() => {
            try {
              if (window.__olazTitleOrig) document.title = window.__olazTitleOrig;
            } catch (e) {}
          }, 90000);
        } catch (e) {}
      } catch (e) {}
    }, 7000);
  };

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

  // ---------------------------------------------------------------- local media
  // This page runs on http://127.0.0.1, and WebKit refuses `file://`
  // subresources from a non-file origin (verified: a `file://` image errors,
  // while the same bytes through `/__olaz/file` decode). Zalo's image component
  // addresses the copy the download manager saved as `file://`, so those URLs
  // are mapped onto the loopback route.
  const localUrl = (value) => {
    if (typeof value !== 'string' || value.slice(0, 7) !== 'file://') return value;
    let path = value.slice(7);
    try { path = decodeURIComponent(path); } catch {}
    if (path.charAt(0) !== '/') return value;
    return base2 + '/__olaz/file?path=' + encodeURIComponent(path);
  };
  const mapFileUrls = (text) =>
    typeof text === 'string' && text.indexOf('file://') >= 0
      ? text.replace(/file:\/\/[^"')]+/g, localUrl)
      : text;
  // React assigns `src` on the element itself, so both the property and
  // `setAttribute` are covered; the observer is the net for inline styles.
  const patchSrc = (proto) => {
    const desc = proto && Object.getOwnPropertyDescriptor(proto, 'src');
    if (!desc || !desc.set || !desc.configurable) return;
    Object.defineProperty(proto, 'src', {
      configurable: true,
      enumerable: desc.enumerable,
      get: desc.get,
      set(value) { desc.set.call(this, localUrl(value)); },
    });
  };
  for (const proto of [window.HTMLImageElement, window.HTMLVideoElement, window.HTMLSourceElement, window.HTMLAudioElement]) {
    try { patchSrc(proto && proto.prototype); } catch {}
  }
  try {
    const desc = Object.getOwnPropertyDescriptor(window.CSSStyleDeclaration.prototype, 'backgroundImage');
    if (desc && desc.set && desc.configurable) {
      Object.defineProperty(window.CSSStyleDeclaration.prototype, 'backgroundImage', {
        configurable: true,
        enumerable: desc.enumerable,
        get: desc.get,
        set(value) { desc.set.call(this, mapFileUrls(value)); },
      });
    }
  } catch {}
  const setAttribute = Element.prototype.setAttribute;
  Element.prototype.setAttribute = function (name, value) {
    if ((name === 'src' || name === 'poster') && typeof value === 'string') value = localUrl(value);
    return setAttribute.call(this, name, value);
  };
  const rewriteNode = (node) => {
    if (!node || node.nodeType !== 1 || !node.getAttribute) return;
    for (const attr of ['src', 'poster']) {
      const value = node.getAttribute(attr);
      const mapped = localUrl(value);
      if (mapped !== value) node.setAttribute(attr, mapped);
    }
    if (node.style && node.style.backgroundImage) node.style.backgroundImage = mapFileUrls(node.style.backgroundImage);
  };
  const rewriteTree = (root) => {
    rewriteNode(root);
    if (root.querySelectorAll) root.querySelectorAll('[src],[poster],[style]').forEach(rewriteNode);
  };
  new MutationObserver((records) => {
    for (const record of records) {
      if (record.type === 'attributes') rewriteNode(record.target);
      else for (const node of record.addedNodes) if (node.nodeType === 1) rewriteTree(node);
    }
  }).observe(document.documentElement || document, {
    subtree: true, childList: true, attributes: true, attributeFilter: ['src', 'poster', 'style'],
  });

  // ---------------------------------------------------------------- libjxl
  // Zalo's photo component decodes JPEG XL through `$zFeatures.libjxl`, the
  // native `zjxl` addon that ships only for macOS/Windows. `local-modules.js`
  // answers that namespace with "module not found", so a downloaded photo never
  // becomes a `<img>` here. Route the namespace to the host's own decoder
  // (jxl-oxide through the bridge) and report the module as ready.
  const jxlCall = (op, ...rest) => {
    const bridge = window.__zaloBridge;
    if (!bridge || typeof bridge.callAsync !== 'function') {
      return Promise.reject(new Error('bridge chưa nạp'));
    }
    return bridge.callAsync('$olaz.jxl.' + op, rest);
  };
  // Debug builds: which libjxl entry the photo component actually reaches.
  const jxlLog = (msg) => {
    if (!debug) return;
    try { window.__olazLinux.fetch('/__olaz/log?m=' + encodeURIComponent('jxl ' + msg)).catch(() => {}); } catch {}
  };
  const withJxl = (local) => {
    if (!local || typeof local !== 'object') return local;
    try {
      local['$zFeatures.libjxl.moduleReady'] = () => {
        jxlLog('moduleReady()');
        return Promise.resolve(true);
      };
      local['$zFeatures.libjxl.decodeToJpeg'] = (bytes, quality) => {
        const size = (bytes && (bytes.byteLength || bytes.length)) || 0;
        return jxlCall('decode', bytes, typeof quality === 'number' ? quality : 90).then(
          (data) => {
            jxlLog('decodeToJpeg ' + size + ' -> ' + ((data && data.byteLength) || 0));
            return { data };
          },
          (err) => {
            jxlLog('decodeToJpeg ' + size + ' failed: ' + err);
            throw err;
          },
        );
      };
      local['$zFeatures.libjxl.getJxlInfo'] = (bytes) => {
        jxlLog('getJxlInfo');
        return jxlCall('info', bytes);
      };
      // Multi-frame decode and encoding stay unsupported; log when they are the
      // path the component took, so a still-blank photo can be attributed.
      for (const key of ['jxlDecompressMulti', 'encodeJxl']) {
        const previous = local['$zFeatures.libjxl.' + key];
        local['$zFeatures.libjxl.' + key] = function (...rest) {
          jxlLog(key + ' called');
          return previous.apply(this, rest);
        };
      }
    } catch (err) {
      report('không gắn được libjxl: ' + err);
    }
    return local;
  };
  // `local-modules.js` assigns the whole object once (and skips itself when the
  // global already exists), so patch it from the setter.
  try {
    let localModules;
    Object.defineProperty(window, '__zaloLocal', {
      configurable: true,
      enumerable: true,
      get: () => localModules,
      set: (value) => { localModules = withClipboard(withJxl(value)); },
    });
  } catch (err) {
    report('không gắn được libjxl: ' + err);
  }

  // -------------------------------------------------- utility-process tasks
  // The photo pipeline runs every media-file operation through a CrossEnvTask
  // to Electron's utility process (`IPhotoFs` → `PHOTO_FS`, the JXL decoder →
  // `DECODE`, both with `connectionId: main-thread-to-utility-process`). That
  // process is part of Electron's main side, which this host replaces, so the
  // connection the renderer registers never connects: `photoFs.exists()` — the
  // first thing the media disk fetcher awaits — never settles, the image load
  // task wedges in `LoadImageTaskQueue.runningTasks` forever, and every bubble
  // keeps its blurred placeholder even though the file is already on disk.
  //
  // Register a connection for the same id that answers the tasks in-page:
  // `PHOTO_FS` from the bridge's fs members, `DECODE` from the bridge's libjxl.
  // The official client sends a plain descriptor over `sendTask` and expects
  // the answer as a `response` message through the manager's `onTask`
  // callback (fall back to `client.handleResponseTask` when that listener has
  // not been registered yet).
  const installUtilityConnection = () => {
    const req = window.__zaloRequire;
    const ModuleContainer = req && req('jDHv') && req('jDHv').ModuleContainer;
    const conn = req && req('k7L8');
    const taskClient = req && req('daC3');
    if (!ModuleContainer || !conn || !taskClient || !window.__zaloBridge) return false;
    const client = ModuleContainer.resolve(taskClient.a);
    const manager = client.connectionManager;
    const utilityId = conn.a.MAIN_THREAD_TO_UTILITY_PROCESS;
    const current = manager.connections.get(utilityId);
    if (current && current.__olazUtility) return true;
    const bridge = window.__zaloBridge;
    const call = (path, args) => bridge.callAsync(path, args);
    const fileExists = (path) => call('$znode.fs.access', [path]).then(() => true).catch(() => false);
    const fileStat = (path) => call('$znode.fs.stat', [path]);
    const readFile = (path) => call('$znode.fs.readFile', [path]);
    const readRange = (path, start, end) =>
      call('$zFileManager.svcReadPath', [path, { start: start, end: end }]);
    const writeFile = (path, data) => call('$znode.fs.writeFile', [path, data]);
    const photoFs = async (payload) => {
      const op = payload && payload.operation;
      const path = payload && (payload.path || payload.src);
      if (op === 'EXISTS') return fileExists(path);
      if (op === 'STAT') return fileStat(path);
      if (op === 'READ_PARTIAL') return readRange(path, payload.start, payload.end);
      if (op === 'READ_ARRAY_BUFFER') return readFile(path);
      if (op === 'READ_BLOB') return readFile(path);
      if (op === 'WRITE_BUFFER') return writeFile(path, payload.buffer);
      if (op === 'WRITE_BLOB') return writeFile(path, payload.buffer);
      if (op === 'DELETE') return call('$znode.fs.unlink', [path]).catch(() => {});
      if (op === 'COPY') return call('$znode.fsExtra.copy', [payload.src, payload.dest]).catch(() => {});
      if (op === 'MOVE') return call('$znode.fsExtra.rename', [payload.src, payload.dest]).catch(() => {});
      if (op === 'ENSURE_DIR') return call('$znode.fsExtra.ensureDir', [path]).catch(() => {});
      throw new Error('PHOTO_FS: operation không hỗ trợ — ' + op);
    };
    const decode = async (params) => {
      const p = typeof params === 'string' ? JSON.parse(params) : params;
      const buf = p && (p.buffer || p.bytes || p.data);
      const libjxl = window.$zFeatures && window.$zFeatures.libjxl;
      if (!buf || !libjxl) throw new Error('DECODE: thiếu dữ liệu hoặc libjxl');
      return libjxl.decodeToJpeg(buf, 88);
    };
    let incoming = null;
    const connection = {
      __olazUtility: true,
      getId: () => utilityId,
      connect: () => Promise.resolve(),
      onTask: (cb) => { incoming = cb; return () => {}; },
      abort: () => {},
      respond: (id, field, value) => {
        const message = { id: id, messageType: 'response' };
        message[field] = value;
        if (incoming) {
          try { incoming(message); return; } catch (e) { report('utility response qua listener lỗi: ' + e); }
        }
        try { client.handleResponseTask(message); } catch (e) { report('utility response lỗi: ' + e); }
      },
      sendTask: (desc) => {
        Promise.resolve().then(() => {
          if (desc.type === 'PHOTO_FS') {
            const p = desc.params;
            return photoFs(p && (p.payload || p));
          }
          if (desc.type === 'DECODE') return decode(desc.params);
          if (desc.type === 'PING') return {};
          throw new Error('utility task không hỗ trợ: ' + desc.type);
        }).then(
          (result) => connection.respond(desc.id, 'result', result),
          (err) => connection.respond(desc.id, 'error', err && err.message ? err.message : String(err)),
        );
      },
    };
    manager.connections.set(utilityId, connection);
    // Mirror `ConnectionManager.registerConnection` for the entry we just
    // replaced (it skips ids it already has, so calling it would be a no-op):
    // hand the manager's incoming-task fan-out to our `onTask` and mark the
    // connection as live, exactly like the official registration does.
    connection.onTask((message) => manager.processIncomingTask(message));
    connection.connect();
    report('utility-process: PHOTO_FS/DECODE chạy qua bridge nội bộ');
    return true;
  };
  const utilityTimer = setInterval(() => {
    try { if (installUtilityConnection()) clearInterval(utilityTimer); } catch (e) {}
  }, 1000);
  setTimeout(() => clearInterval(utilityTimer), 120000);

  // The photo component mounts its `<img loading="lazy">` hidden
  // (`visibility: hidden`) and waits for `load` before showing it — but
  // WebKitGTK never starts a lazy load for a hidden element, so the two wait
  // on each other forever and the bubble keeps its blurred placeholder even
  // though the file is served. Force `loading: eager` on wedged images: eager
  // loading ignores the visibility gate, the load completes, and the component
  // flips the element visible. Rate-limited so a legitimately pending lazy
  // image is not churned.
  const retriggerWedgedImages = () => {
    let n = 0;
    const now = Date.now();
    for (const img of document.images) {
      if (img.naturalWidth > 0 || img.complete) continue;
      const src = img.currentSrc || img.src || '';
      if (src.indexOf(base2 + '/__olaz/file') !== 0 && src.indexOf(base + '/__olaz/file') !== 0) continue;
      const attempts = img.__olazRetries || 0;
      const last = img.__olazRetryAt || 0;
      if (attempts >= 4 || now - last < 15000) continue;
      img.__olazRetries = attempts + 1;
      img.__olazRetryAt = now;
      img.loading = 'eager';
      n += 1;
    }
    return n;
  };
  // The conversation-open sweeper stops after 15 s; a first open right after
  // boot can still wedge images later, so keep a slow background pass too.
  const wedgedTimer = setInterval(() => {
    try { retriggerWedgedImages(); } catch (e) {}
  }, 5000);
  setTimeout(() => clearInterval(wedgedTimer), 300000);

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

  // ------------------------------------------------------------- external links
  // Zalo PC mở link qua `window.open` (Electron main → shell.openExternal). Trên
  // WebKitGTK với UA Safari, `getBrowserName()` của renderer trả "Safari" và
  // chuyển sang `openUrlInSandBox` — link nạp vào một iframe ẩn rồi bị xoá sau
  // 5s: không có gì mở ra. Fix gốc là UA Chromium 108 (main.rs) + `window.chrome`
  // bên dưới → renderer trả "Chrome" như trong Electron và đi đúng đường
  // window.open; shim này đẩy URL về channel `open-external` của host
  // (xdg-open — thay `NSWorkspace.shared.open` trên macOS).
  // `window.chrome` như Chromium thật (Electron có sẵn) — thiếu nó
  // `getBrowserName()` trả "" thay vì "Chrome", các nhánh Chromium không chạy.
  if (!window.chrome) {
    window.chrome = { app: { isInstalled: false }, runtime: {}, webstore: {} };
  }
  const hostOpenExternal = (url) => {
    try {
      nativeFetch(base + '/bridge/zbridge', {
        method: 'POST',
        headers,
        body: JSON.stringify({ channel: 'open-external', args: [String(url)] }),
      });
    } catch {}
  };
  const isInternalUrl = (url) =>
    url.indexOf(base) === 0 || (base2 !== base && url.indexOf(base2) === 0);
  const nativeOpen = window.open.bind(window);
  window.open = (url, target, features) => {
    if (typeof url === 'string' && /^https?:\/\//i.test(url) && !isInternalUrl(url)) {
      hostOpenExternal(url);
      return null;
    }
    return nativeOpen(url, target, features);
  };

  // --------------------------------------------------------------- copy image
  // `$zelectronNative.writeImageToClipboard(dataUrl)` — preload Electron vẽ ảnh
  // ra canvas, nén JPEG 0.9 rồi `clipboard.writeImage`. Ở đây phần cuối đi qua
  // bridge (`clipboardWriteImage` → GTK clipboard) vì không có nativeImage.
  function withClipboard(local) {
    local['$zelectronNative.writeImageToClipboard'] = (dataUrl) => new Promise((resolve) => {
      const fail = () => resolve(false);
      try {
        const img = document.createElement('img');
        img.onload = () => {
          try {
            const canvas = document.createElement('canvas');
            canvas.width = img.naturalWidth || img.width;
            canvas.height = img.naturalHeight || img.height;
            if (!canvas.width || !canvas.height) return fail();
            canvas.getContext('2d').drawImage(img, 0, 0);
            canvas.toBlob((blob) => {
              if (!blob) return fail();
              const reader = new FileReader();
              reader.onload = () => {
                try {
                  const result = String(reader.result || '');
                  const base64 = result.slice(result.indexOf(',') + 1);
                  const ok = window.__zaloBridge.callSync(
                    '$zelectronNative.clipboardWriteImage',
                    ['image/jpeg', base64],
                  );
                  resolve(ok !== false);
                } catch (err) {
                  report('copy ảnh: bridge lỗi — ' + err);
                  fail();
                }
              };
              reader.onerror = fail;
              reader.readAsDataURL(blob);
            }, 'image/jpeg', 0.9);
          } catch (err) {
            report('copy ảnh: canvas lỗi — ' + err);
            fail();
          }
        };
        img.onerror = fail;
        img.src = dataUrl;
      } catch (err) {
        report('copy ảnh: dataUrl lỗi — ' + err);
        fail();
      }
    });
    return local;
  }

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
  // WebKitGTK exposes an empty, non-configurable `window.webkit`, and its
  // `messageHandlers` bag can appear or be replaced after the boot script runs.
  // Host scripts still call `window.webkit.messageHandlers.zlog` directly
  // (network-shim worker logs, adapters), so keep a stable getter over the
  // shim's handlers and re-install it as the platform object comes up.
  const installPlatformHandlers = () => {
    const target = window.webkit;
    if (!target) return false;
    let stable = false;
    try {
      Object.defineProperty(target, 'messageHandlers', {
        configurable: true,
        enumerable: true,
        get: () => handlers,
      });
      stable = true;
    } catch {}
    return installHandlers() || stable;
  };
  if (!installPlatformHandlers()) {
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
  for (const delay of [0, 100, 500, 1500, 4000]) setTimeout(installPlatformHandlers, delay);
  // WebKitGTK can replace the platform bag after those one-shot attempts; keep
  // the getter installed so worker diagnostics and adapter `zlog` calls survive.
  setInterval(installPlatformHandlers, 1000);

  // ---------------------------------------------------------------- dev harness
  // The macOS dev channel can hand a composer action a real file pick. Linux has
  // no equivalent native hook, so read the paths through the normal file bridge
  // and inject the resulting `File`s into the hidden input the composer adapter
  // clicks. Dev-only: the watcher and the endpoints are gated by OLAZ_DEV_EVAL.
  const devFileType = (name) => {
    const ext = String(name).split('.').pop().toLowerCase();
    return ({
      jpg: 'image/jpeg', jpeg: 'image/jpeg', png: 'image/png', gif: 'image/gif', webp: 'image/webp',
      bmp: 'image/bmp', heic: 'image/heic', mp4: 'video/mp4', mov: 'video/quicktime',
      m4v: 'video/x-m4v', mp3: 'audio/mpeg', m4a: 'audio/mp4', wav: 'audio/wav',
      ogg: 'audio/ogg', pdf: 'application/pdf', txt: 'text/plain', json: 'application/json', zip: 'application/zip',
    })[ext] || 'application/octet-stream';
  };
  window.__olazLinux.devCompose = async (method, args, files) => {
    if (method === 'screenshot') return 'unsupported: ZaloCap/ScreenCapture chưa port sang Linux';
    const adapter = window.__zaloCompose;
    if (!adapter || typeof adapter[method] !== 'function') return 'not-ready';
    const paths = Array.isArray(files) ? files.filter((path) => typeof path === 'string' && path) : [];
    const picked = [];
    for (const path of paths) {
      let bytes;
      try {
        bytes = window.__zaloBridge.callSync('$zFileManager.getFileArrayBuffer', [path]);
      } catch (err) {
        return 'cannot-read: ' + path + ' — ' + err;
      }
      if (!bytes) return 'cannot-read: ' + path;
      const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
      const name = String(path).split('/').pop() || 'file';
      picked.push(new File([view], name, { type: devFileType(name), lastModified: Date.now() }));
    }
    let restore = null;
    if (picked.length) {
      if (typeof DataTransfer !== 'function') return 'unsupported: DataTransfer không có trong WebKitGTK';
      const original = HTMLInputElement.prototype.click;
      let armed = picked;
      HTMLInputElement.prototype.click = function () {
        if (this.type !== 'file' || !armed) return original.apply(this, arguments);
        const list = armed;
        armed = null;
        const transfer = new DataTransfer();
        for (const file of list) transfer.items.add(file);
        try { this.files = transfer.files; } catch {}
        if (!this.files || this.files.length !== list.length) {
          try { Object.defineProperty(this, 'files', { configurable: true, value: transfer.files }); } catch {}
        }
        this.dispatchEvent(new Event('change', { bubbles: true }));
        return undefined;
      };
      restore = () => { HTMLInputElement.prototype.click = original; };
    }
    try {
      const outcome = await adapter[method].apply(adapter, Array.isArray(args) ? args : []);
      return typeof outcome === 'string' ? outcome : JSON.stringify(outcome);
    } catch (err) {
      return 'error: ' + String((err && err.stack) || err);
    } finally {
      if (restore) setTimeout(restore, 0);
    }
  };

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
    // Host-driven page control (loopback only): e.g. the notification
    // permission grant for the shim the official renderer expects.
    if (ev.kind === 'js') {
      try { (0, eval)(String(ev.code || '')); } catch (e) {}
      return;
    }
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
    if (ev.kind === 'send') {
      const session = (ev.args || []).find((a) => a && typeof a === 'object' && typeof a.UIN === 'string' && a.UIN);
      if (session) scheduleMessageDecrypt(String(session.userId || ''), String(session.UIN));
    }
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
    // No custom headers: a cross-origin GET with `content-type: application/json`
    // would trigger a CORS preflight on every poll. `win` tells the host which
    // window is asking, so per-window control events (dev-eval) reach the
    // right page even when the broadcast ring is flooded.
    nativeFetch(base2 + '/poll?cursor=' + cursor + '&win=' + win)
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
