//! Loopback HTTP host: serves the official renderer out of `app.asar`, and
//! carries the bridge (`zbridge`, `znet`, `zlog`, `zstate`), the synchronous
//! bridge used by `bridge.js`, and host → page pushes.
//!
//! Why a local HTTP server instead of Tauri IPC: `bridge.js` makes genuinely
//! synchronous calls (blocking XHR). In WKWebView Swift answered those inside a
//! URL-scheme handler; in WebKitGTK a custom-scheme handler runs on the same
//! thread as the blocking XHR, so it would deadlock. A loopback server answers
//! from its own threads, so the request completes exactly like on macOS.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::asar::Asar;
use crate::bridge::Bridge;

pub struct AppContext {
    /// The renderer, once it exists. Mutable: the diagnostics page can install
    /// the runtime while the window is already open (`adopt_runtime`).
    pub asar: Mutex<Option<Asar>>,
    pub userdata: PathBuf,
    pub bridge: Bridge,
    pub events: Arc<Events>,
    pub debug: bool,
    /// JS evaluation in the main renderer and the hidden shared worker.
    /// On by default in debug builds; release builds require `OLAZ_DEV_EVAL=1`,
    /// and `OLAZ_DEV_EVAL=0` turns it off in either build. The endpoints, the
    /// file watcher and the page handler all check this flag.
    dev_eval: bool,
    dev_eval_seq: AtomicU64,
    dev_eval_waiters: Mutex<HashMap<u64, mpsc::Sender<String>>>,
    /// First-run installer state, shown on `/__olaz/index.html`.
    pub runtime: Arc<Mutex<RuntimeState>>,
    port: AtomicU16,
    aux_port: AtomicU16,
    /// The whole injected bundle (shim + spec + host scripts), served as
    /// `/__olaz/boot.js` and referenced from every page this host serves.
    boot: Mutex<String>,
}

/// Why there is no renderer yet, mirrored to the diagnostics page so a GUI
/// launch (no terminal, hence no stderr) still says what went wrong.
#[derive(Clone)]
pub enum RuntimeState {
    Checking,
    Missing(String, Vec<crate::runtime::Check>),
    Fetching(Vec<crate::runtime::Check>),
    Ready(String),
    Failed(String, Vec<crate::runtime::Check>),
}

impl RuntimeState {
    fn json(&self) -> Value {
        let checks = |checks: &[crate::runtime::Check]| checks.iter().map(crate::runtime::Check::json).collect::<Vec<_>>();
        match self {
            RuntimeState::Checking => json!({ "state": "checking", "message": "đang kiểm tra runtime…" }),
            RuntimeState::Missing(message, list) => {
                json!({ "state": "missing", "message": message, "checks": checks(list) })
            }
            RuntimeState::Fetching(list) => {
                json!({ "state": "fetching", "message": "đang tải và cài renderer Zalo", "checks": checks(list) })
            }
            RuntimeState::Ready(version) => json!({ "state": "ready", "version": version, "message": "" }),
            RuntimeState::Failed(message, list) => {
                json!({ "state": "failed", "message": message, "checks": checks(list) })
            }
        }
    }
}

/// Host → page queue, read by the long-poll loop in `web/olaz-linux.js`.
pub struct Events {
    state: Mutex<(u64, VecDeque<Value>)>,
    signal: Condvar,
    /// Control events (dev-eval) that must never be dropped by the ring
    /// buffer: keyed by id, delivered once to the window they name, removed
    /// on delivery (the page posts the result to `/__olaz/dev/eval-result`).
    control: Mutex<VecDeque<(u64, String, Value)>>,
}

impl Events {
    pub fn new() -> Self {
        Self {
            state: Mutex::new((0, VecDeque::new())),
            signal: Condvar::new(),
            control: Mutex::new(VecDeque::new()),
        }
    }

    pub fn push(&self, mut event: Value) {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        let seq = state.0;
        if let Some(object) = event.as_object_mut() {
            object.insert("seq".into(), json!(seq));
        }
        state.1.push_back(event);
        while state.1.len() > 4096 {
            state.1.pop_front();
        }
        self.signal.notify_all();
    }

    /// Queues a control event for one window. Unlike the broadcast ring these
    /// are never dropped: the sync machine and the diagnostics tooling drive
    /// the renderer through them, and a flood of realtime events must not
    /// starve that channel.
    pub fn push_control(&self, id: u64, target: &str, event: Value) {
        self.control
            .lock()
            .unwrap()
            .push_back((id, target.to_string(), event));
        self.signal.notify_all();
    }

    /// Removes and returns the control events addressed to `window`.
    pub fn drain_control(&self, window: &str) -> Vec<Value> {
        let mut queue = self.control.lock().unwrap();
        let mut kept = VecDeque::new();
        let mut out = Vec::new();
        for (id, target, event) in queue.drain(..) {
            if target == window {
                out.push(event);
            } else {
                kept.push_back((id, target, event));
            }
        }
        *queue = kept;
        out
    }

    /// Events after `cursor`; blocks up to `timeout` for the first one.
    /// `cursor == 0` starts at "now" (a page that just loaded does not replay
    /// pushes aimed at the previous page).
    pub fn since(&self, cursor: u64, timeout: Duration) -> (u64, Vec<Value>) {
        let mut state = self.state.lock().unwrap();
        let mut cursor = if cursor == 0 { state.0 } else { cursor };
        if state.0 <= cursor {
            let (next, _) = self.signal.wait_timeout(state, timeout).unwrap();
            state = next;
        }
        let events: Vec<Value> = state
            .1
            .iter()
            .filter(|event| event.get("seq").and_then(|s| s.as_u64()).unwrap_or(0) > cursor)
            .cloned()
            .collect();
        if let Some(last) = events.last().and_then(|event| event.get("seq")).and_then(|s| s.as_u64()) {
            cursor = last;
        } else if state.0 > cursor {
            cursor = state.0;
        }
        (cursor, events)
    }
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

impl AppContext {
    pub fn load() -> Result<Self, String> {
        let debug = std::env::var("OLAZ_DEBUG").map(|v| v != "0").unwrap_or(false);
        let dev_eval = match std::env::var("OLAZ_DEV_EVAL") {
            Ok(value) => value == "1",
            Err(_) => cfg!(debug_assertions),
        };
        let userdata = userdata_dir();
        std::fs::create_dir_all(&userdata).map_err(|e| format!("không tạo được {}: {e}", userdata.display()))?;
        // A runtime downloaded by the previous launch is promoted before any
        // asar is opened; a running page is never serving from a replaced file.
        crate::runtime::promote_pending(&userdata);

        // Startup only looks at what is on disk — it never downloads. The
        // renderer is installed in the background (`bootstrap_runtime`) while
        // the window already shows the diagnostics page, so a first run with a
        // 323 MB download does not look like an app that failed to start.
        let asar_path = find_asar();
        let checks = crate::runtime::preflight(&userdata);
        let asar = match &asar_path {
            Some(path) => match Asar::open(path) {
                Ok(archive) => {
                    eprintln!("[olaz] app.asar: {} ({} file)", path.display(), archive.count());
                    Some(archive)
                }
                Err(err) => {
                    eprintln!("[olaz] app.asar không đọc được — {err}");
                    None
                }
            },
            None => {
                eprintln!(
                    "[olaz] chưa có app.asar — mở trang chẩn đoán nội bộ; renderer sẽ được cài ở nền \
                     (đặt OLAZ_ASAR=/đường/dẫn/app.asar hoặc OLAZ_AUTO_FETCH=0 để đổi cách chạy)."
                );
                None
            }
        };

        let runtime_state = match (&asar, &asar_path) {
            (Some(_), _) => {
                let version = if std::env::var("OLAZ_ASAR").is_ok() {
                    "OLAZ_ASAR".to_string()
                } else {
                    crate::runtime::installed_version(&userdata)
                };
                RuntimeState::Ready(version)
            }
            (None, Some(path)) => RuntimeState::Missing(
                format!("app.asar ở {} không đọc được", path.display()),
                checks,
            ),
            (None, None) => RuntimeState::Missing("chưa cài runtime Zalo".to_string(), checks),
        };
        let client_version = asar
            .as_ref()
            .and_then(|a| a.read("main-dist/main.js"))
            .and_then(|data| client_version_of(&data))
            .or_else(|| std::env::var("OLAZ_CLIENT_VERSION").ok())
            .unwrap_or_else(|| "695".to_string());

        // The page query must report the runtime actually being served. For a
        // managed runtime that is its version file (updated by promote_pending);
        // for OLAZ_ASAR, read package.json from the user's archive.
        let app_version = if std::env::var("OLAZ_ASAR").is_ok() {
            asar.as_ref()
                .and_then(|archive| archive.read("package.json"))
                .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
                .and_then(|package| package.get("version").and_then(|v| v.as_str()).map(str::to_string))
                .unwrap_or_else(|| crate::runtime::PINNED_VERSION.to_string())
        } else {
            crate::runtime::installed_version(&userdata)
        };

        let events = Arc::new(Events::new());
        let bridge = Bridge::new(userdata.clone(), client_version, debug, events.clone());
        bridge.set_app_version(&app_version);
        Ok(Self {
            asar: Mutex::new(asar),
            userdata,
            bridge,
            events,
            debug,
            dev_eval,
            dev_eval_seq: AtomicU64::new(0),
            dev_eval_waiters: Mutex::new(HashMap::new()),
            runtime: Arc::new(Mutex::new(runtime_state)),
            port: AtomicU16::new(0),
            aux_port: AtomicU16::new(0),
            boot: Mutex::new(String::new()),
        })
    }

    /// Bakes the loopback port into the boot bundle once the server has a port.
    pub fn set_port(&self, port: u16) {
        self.port.store(port, Ordering::Relaxed);
        let script = init_script(self, port);
        *self.boot.lock().unwrap() = script;
    }

    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    /// Secondary loopback port for the long-poll and media file routes (see
    /// `start`), so they never contend with the page's own traffic for the
    /// renderer's per-host connection budget.
    pub fn set_aux_port(&self, port: u16) {
        self.aux_port.store(port, Ordering::Relaxed);
    }

    pub fn aux_port(&self) -> u16 {
        self.aux_port.load(Ordering::Relaxed)
    }

    pub fn dev_eval_enabled(&self) -> bool {
        self.dev_eval
    }

    /// Runs `code` in the main renderer or the hidden shared worker and waits
    /// for the page to post its JSON result back to `/__olaz/dev/eval-result`.
    fn dev_eval(&self, target: &str, code: &str, timeout: Duration) -> Result<String, String> {
        let id = self.dev_eval_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = mpsc::channel();
        self.dev_eval_waiters.lock().unwrap().insert(id, sender);
        self.events.push_control(
            id,
            target,
            json!({ "kind": "dev-eval", "id": id, "code": code, "target": target }),
        );
        match receiver.recv_timeout(timeout) {
            Ok(result) => Ok(result),
            Err(_) => {
                self.dev_eval_waiters.lock().unwrap().remove(&id);
                Err(format!(
                    "renderer {target} không trả lời trong {} giây",
                    timeout.as_secs()
                ))
            }
        }
    }

    pub fn set_runtime(&self, state: RuntimeState) {
        *self.runtime.lock().unwrap() = state;
    }

    pub fn boot_script(&self) -> String {
        self.boot.lock().unwrap().clone()
    }

    /// Where the window should land: the renderer once it exists, the
    /// diagnostics page until then. Recomputed — the runtime can arrive late.
    pub fn entry(&self) -> String {
        let login = self
            .asar
            .lock()
            .unwrap()
            .as_ref()
            .map(|archive| archive.contains("pc-dist/login.html"))
            .unwrap_or(false);
        if login {
            self.renderer_entry()
        } else {
            "/__olaz/index.html".to_string()
        }
    }

    /// Where the renderer starts: the main window when a session was saved,
    /// otherwise the QR login page. Same decision main makes from the cookie jar.
    pub fn renderer_entry(&self) -> String {
        if self.bridge.has_session() {
            format!("{}&already_login=y", self.bridge.page_url("/app/index.html"))
        } else {
            self.bridge.page_url("/app/login.html")
        }
    }

    /// Swaps in a runtime installed while the app is running (the diagnostics
    /// page's button) and sends the window to the renderer — no restart.
    pub fn adopt_runtime(&self, archive: Asar, version: &str) {
        let client_version = archive.read("main-dist/main.js").as_deref().and_then(client_version_of);
        let url = if archive.contains("pc-dist/login.html") {
            self.renderer_entry()
        } else {
            "/__olaz/index.html".to_string()
        };
        *self.asar.lock().unwrap() = Some(archive);
        *self.runtime.lock().unwrap() = RuntimeState::Ready(version.to_string());
        self.bridge.set_client_version(client_version);
        self.bridge.set_app_version(version);
        self.events.push(json!({ "type": "navigate", "url": url }));
        eprintln!("[olaz] runtime: Zalo {version} đã dùng được, chuyển cửa sổ sang {url}");
    }

    pub fn serve_page(&self, path: &str) -> Option<Vec<u8>> {
        let guard = self.asar.lock().unwrap();
        let archive = guard.as_ref()?;
        let relative = path.trim_start_matches('/');
        let relative = if relative.is_empty() { "index.html".to_string() } else { relative.to_string() };
        archive
            .read(&format!("pc-dist/{relative}"))
            .map(|body| patch_sync_asset(&relative, body))
    }
}

/// The macOS host finishes the DAL worker's setup before its own bundle runs.
/// `sync-v2-sub-worker` writes imported messages through the "lite IDB" path,
/// whose encryption-state store needs the DB client created by
/// `database-manager.createQueryBuilder`; nothing in a web worker creates that
/// client (Application lifecycle is unsupported there). Linux served the
/// original JS, so sync failed before writing a message.
const SYNC_WORKER_PRELUDE: &str = r#"
(() => {
  const fwd = (k, v) => { try { self.postMessage({ __zhostLog: k + ' ' + String(v).slice(0, 300) }); } catch {} };
  self.__zhostFwd = fwd;
  fwd('prelude-loaded', self.name || '');
  const errText = (e) => e ? (e.name || '') + ': ' + (e.message || e) : String(e);
  self.addEventListener('error', (e) => fwd('error', e.message));
  self.addEventListener('unhandledrejection', (e) => fwd('unhandledrejection', errText(e.reason)));
  for (const lvl of ['warn', 'error']) {
    const o = console[lvl];
    console[lvl] = (...a) => { fwd('console.' + lvl, a.map((x) => x instanceof Error ? errText(x) : (typeof x === 'string' ? x : '')).join(' ')); return o.apply(console, a); };
  }
  const ensureDatabaseClient = () => {
    try {
      const req = self.__swReq;
      if (!req || !req.c) { fwd('webpack require not exposed', ''); return; }
      for (const id of Object.keys(req.c)) {
        try {
          const d = req.c[id] && req.c[id].exports && req.c[id].exports.default;
          if (d && typeof d.getInstance === 'function' && String(d.getInstance).includes('createQueryBuilder')) {
            d.getInstance(); fwd('database client ready', id); return;
          }
        } catch (e) {}
      }
      fwd('database client accessor not found', '');
    } catch (e) { fwd('database client setup failed', errText(e)); }
  };
  if (typeof self.importScripts === 'function') {
    const imp = self.importScripts;
    self.importScripts = function (...urls) {
      const r = imp.apply(this, urls);
      if (urls.some((u) => /sync-v2-sub-worker/.test(String(u)))) { fwd('sync-v2 imported', ''); ensureDatabaseClient(); }
      return r;
    };
  }
  if (typeof IDBDatabase !== 'undefined') {
    const tx = IDBDatabase.prototype.transaction;
    IDBDatabase.prototype.transaction = function (...a) {
      const t = tx.apply(this, a);
      t.addEventListener('abort', () => fwd('idb-abort', errText(t.error)));
      return t;
    };
    for (const m of ['add', 'put']) {
      const o = IDBObjectStore.prototype[m];
      IDBObjectStore.prototype[m] = function (...a) {
        try {
          const r = o.apply(this, a);
          r.addEventListener('error', () => fwd('idb-' + m + '-error', this.name + ' ' + errText(r.error)));
          return r;
        } catch (e) { fwd('idb-' + m + '-throw', this.name + ' ' + errText(e)); throw e; }
      };
    }
    const open = IDBFactory.prototype.open;
    IDBFactory.prototype.open = function (...a) {
      const r = open.apply(this, a);
      r.addEventListener('error', () => fwd('idb-open-error', errText(r.error)));
      r.addEventListener('blocked', () => fwd('idb-open-blocked', ''));
      return r;
    };
  }
})();
"#;

/// The sub-worker runs in its own scope, where the DAL prelude never runs, so
/// `self.__zhostFwd` was undefined and the instrumentation above stayed silent.
/// Give it a forwarder that reaches the host over loopback (the HTTP route logs it).
const SUB_WORKER_PRELUDE: &str = r#"
(() => {
  self.__zhostFwd = (k, v) => { try { fetch('/__olaz/log?m=' + encodeURIComponent(k + ' ' + String(v).slice(0, 300)), { cache: 'no-store' }).catch(() => {}); } catch (e) {} };
})();
"#;

fn patch_sync_asset(path: &str, data: Vec<u8>) -> Vec<u8> {
    let is_js = path.ends_with(".js");
    let data = if path.starts_with("dal-worker.") && is_js {
        let mut out = Vec::with_capacity(SYNC_WORKER_PRELUDE.len() + data.len() + 2);
        out.extend_from_slice(SYNC_WORKER_PRELUDE.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(&data);
        out
    } else if path.starts_with("sync-v2-sub-worker.") && is_js {
        let data = expose_webpack_require(data);
        #[cfg(debug_assertions)]
        let data = {
            let mut out = Vec::with_capacity(SUB_WORKER_PRELUDE.len() + data.len() + 1);
            out.extend_from_slice(SUB_WORKER_PRELUDE.as_bytes());
            out.extend_from_slice(&data);
            patch_sync_insert_success(patch_sync_insert_error(out))
        };
        data
    } else if path.starts_with("lazy/main-startup.") && is_js {
        patch_sync_global_id_resolver(data)
    } else {
        data
    };
    if is_js { patch_worker_pool_timeout(data) } else { data }
}

/// The sync-v2 worker pool settles a task as `TIMEOUT` and drops it, which
/// surfaces as `insertToDb` failing and the transfer ending in
/// `MSG_IMPORT_FAILED` (1037). Two bundle shapes carry that budget:
///
/// * an older shape sized it from a constructor parameter
///   (`this.taskTimeout=i,…`), patched to at least 120 s;
/// * the current shape hardcodes `WorkerTimeOut:6e4` (60 s) next to the
///   message-channel table — raised to 10 minutes. WebKitGTK's IndexedDB is
///   far slower than Zalo's SQLite for the 2000-message import batches, so
///   batches that are still writing get killed. The task itself is unchanged,
///   only how long it may take.
fn patch_worker_pool_timeout(data: Vec<u8>) -> Vec<u8> {
    const OLD_NEEDLE: &[u8] = b"this.taskTimeout=i,this.workers=[]";
    const OLD_REPLACEMENT: &[u8] = b"this.taskTimeout=Math.max(i,1.2e5),this.workers=[]";
    let data = match find_bytes(&data, OLD_NEEDLE) {
        Some(pos) if find_bytes(&data[pos + OLD_NEEDLE.len()..], OLD_NEEDLE).is_none() => {
            let mut out = Vec::with_capacity(data.len() + OLD_REPLACEMENT.len() - OLD_NEEDLE.len());
            out.extend_from_slice(&data[..pos]);
            out.extend_from_slice(OLD_REPLACEMENT);
            out.extend_from_slice(&data[pos + OLD_NEEDLE.len()..]);
            out
        }
        _ => data,
    };
    const NEEDLE: &[u8] = b"WorkerTimeOut:6e4";
    const REPLACEMENT: &[u8] = b"WorkerTimeOut:6e5";
    match find_bytes(&data, NEEDLE) {
        Some(pos) if find_bytes(&data[pos + NEEDLE.len()..], NEEDLE).is_none() => {
            let mut out = Vec::with_capacity(data.len() + REPLACEMENT.len() - NEEDLE.len());
            out.extend_from_slice(&data[..pos]);
            out.extend_from_slice(REPLACEMENT);
            out.extend_from_slice(&data[pos + NEEDLE.len()..]);
            out
        }
        _ => data,
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Debug builds: the official logger swallows `insertToDb` failures. Forward
/// the caught error to the DAL worker prelude so a stalled sync says why.
fn patch_sync_insert_error(data: Vec<u8>) -> Vec<u8> {
    const NEEDLE: &[u8] = b"throw this.logger.zsymb(18,\"cEXelp\",\"Error when insert messages to db\",n)";
    let Some(pos) = find_bytes(&data, NEEDLE) else { return data };
    if find_bytes(&data[pos + NEEDLE.len()..], NEEDLE).is_some() {
        return data;
    }
    const REPORT: &[u8] = br#"self.__zhostFwd&&self.__zhostFwd('insertToDb-failed',n&&((n.name||'')+': '+(n.message||n)+' | '+String(n.stack||'').split('\n').slice(0,6).join(' < ')));"#;
    let mut out = Vec::with_capacity(data.len() + REPORT.len());
    out.extend_from_slice(&data[..pos]);
    out.extend_from_slice(REPORT);
    out.extend_from_slice(&data[pos..]);
    out
}

/// Debug builds: the importer reports a finished batch only through the official logger, which
/// this host never sees, so a sync run cannot tell written rows from dropped ones. Forward the
/// counts of the batch that `insertMessage` just returned.
fn patch_sync_insert_success(data: Vec<u8>) -> Vec<u8> {
    // Inserted after a complete statement: splicing between call arguments produced a
    // stray comma and the whole sub-worker failed to parse (`Unexpected identifier 'self'`).
    const NEEDLE: &[u8] = b"const a=await this.insertMessage(e);";
    const REPORT: &[u8] = br#"self.__zhostFwd&&self.__zhostFwd('insert-msgs','ok='+a.success.length+' fail='+a.fail.length);"#;
    let Some(pos) = find_bytes(&data, NEEDLE) else { return data };
    if find_bytes(&data[pos + NEEDLE.len()..], NEEDLE).is_some() {
        return data;
    }
    let at = pos + NEEDLE.len();
    let mut out = Vec::with_capacity(data.len() + REPORT.len());
    out.extend_from_slice(&data[..at]);
    out.extend_from_slice(REPORT);
    out.extend_from_slice(&data[at..]);
    out
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

fn valid_ident(text: &str) -> bool {
    !text.is_empty() && !text.as_bytes()[0].is_ascii_digit() && text.bytes().all(is_ident_byte)
}

fn ident_start(text: &str, end: usize) -> usize {
    let bytes = text.as_bytes();
    let mut start = end;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    start
}

/// Exposes the sync-v2 sub-worker's private webpack `require` as
/// `self.__swReq`, so `SYNC_WORKER_PRELUDE` can finish the DB-client setup.
/// Only patches one unambiguous runtime; otherwise serves the original bytes.
fn expose_webpack_require(data: Vec<u8>) -> Vec<u8> {
    const TAIL: &[u8] = b"=function(e){var t=[],n=a[e]";
    let Some(pos) = find_bytes(&data, TAIL) else {
        eprintln!("[olaz] sync-v2-sub-worker served unpatched: runtime shape not found");
        return data;
    };
    if find_bytes(&data[pos + TAIL.len()..], TAIL).is_some() {
        eprintln!("[olaz] sync-v2-sub-worker served unpatched: runtime shape ambiguous");
        return data;
    }
    if pos < 2 || &data[pos - 2..pos] != b".e" {
        eprintln!("[olaz] sync-v2-sub-worker served unpatched: no .e accessor");
        return data;
    }
    let mut start = pos - 2;
    while start > 0 && is_ident_byte(data[start - 1]) {
        start -= 1;
    }
    if start == pos - 2 || start == 0 || data[start].is_ascii_digit() || !matches!(data[start - 1], b'}' | b';') {
        eprintln!("[olaz] sync-v2-sub-worker served unpatched: not at a statement boundary");
        return data;
    }
    let mut out = Vec::with_capacity(data.len() + (pos - start) + 20);
    out.extend_from_slice(&data[..start]);
    out.extend_from_slice(b"self.__swReq=");
    out.extend_from_slice(&data[start..pos - 2]);
    out.push(b';');
    out.extend_from_slice(&data[start..]);
    out
}

/// The macOS `patchSyncGlobalIdResolver`: ask the official global-id service
/// for the 1-1 ids the profile lookup left unmapped. Without it, a resumed
/// sync can abort with EXCEEDED_MAX_BATCH_FAIL.
fn patch_sync_global_id_resolver(data: Vec<u8>) -> Vec<u8> {
    let Ok(source) = String::from_utf8(data.clone()) else { return data };
    const MARKER: &str = "getGlobalIdsFromUserIds";
    let mut patched: Option<(usize, usize, String)> = None;
    for (marker, _) in source.match_indices(MARKER) {
        let Some(found) = parse_sync_resolver(&source, marker) else { continue };
        if patched.is_some() {
            eprintln!("[olaz] main-startup served unpatched: resolver shape ambiguous");
            return data;
        }
        patched = Some(found);
    }
    let Some((start, end, replacement)) = patched else {
        eprintln!("[olaz] main-startup served unpatched: resolver shape not found");
        return data;
    };
    let mut out = source;
    out.replace_range(start..end, &replacement);
    out.into_bytes()
}

/// Parses `svc.resolveWithRetry(ids,(arg=>mgr.default.getGlobalIdsFromUserIds(arg)))`
/// around `marker`, where `marker` is the start of `getGlobalIdsFromUserIds`.
fn parse_sync_resolver(source: &str, marker: usize) -> Option<(usize, usize, String)> {
    const DEFAULT: &str = ".default.";
    if !source[..marker].ends_with(DEFAULT) {
        return None;
    }
    let mgr_end = marker - DEFAULT.len();
    let mgr_start = ident_start(source, mgr_end);
    let mgr = &source[mgr_start..mgr_end];
    if !valid_ident(mgr) || !source[..mgr_start].ends_with("=>") {
        return None;
    }
    let arg_end = mgr_start - 2;
    let arg_start = ident_start(source, arg_end);
    let arg = &source[arg_start..arg_end];
    if !valid_ident(arg) || !source[..arg_start].ends_with('(') {
        return None;
    }
    let arrow_open = arg_start - 1;
    if !source[..arrow_open].ends_with(',') {
        return None;
    }
    let ids_end = arrow_open - 1;
    let ids_start = ident_start(source, ids_end);
    let ids = &source[ids_start..ids_end];
    if !valid_ident(ids) {
        return None;
    }
    const CALL: &str = ".resolveWithRetry(";
    let call = source[..ids_start].rfind(CALL)?;
    let svc_start = ident_start(source, call);
    let svc = &source[svc_start..call];
    if !valid_ident(svc) || call + CALL.len() != ids_start {
        return None;
    }
    let expected = format!("({arg})))");
    let tail = marker + "getGlobalIdsFromUserIds".len();
    if !source[tail..].starts_with(&expected) {
        return None;
    }
    let end = tail + expected.len();
    let mut replacement = String::new();
    replacement.push_str(svc);
    replacement.push_str(".resolveWithRetry(");
    replacement.push_str(ids);
    replacement.push_str(",(async ");
    replacement.push_str(arg);
    replacement.push_str("=>{let __r=await ");
    replacement.push_str(mgr);
    replacement.push_str(".default.getGlobalIdsFromUserIds(");
    replacement.push_str(arg);
    replacement.push_str(")||{};const __m=");
    replacement.push_str(ids);
    replacement.push_str(".filter(__i=>!__r[__i]),__f=window.__swzGlobalIdFallback;if(__m.length&&typeof __f===\"function\")try{__r=Object.assign({},__r,await __f(__m))}catch(__e){}return __r}))");
    Some((svc_start, end, replacement))
}

/// First-run initialisation, run once the bridge is up: check what is installed
/// and start the installer in the background when the renderer is missing. The
/// window is already on the diagnostics page by then, which is what makes a
/// first launch show progress instead of nothing.
pub fn bootstrap_runtime(ctx: Arc<AppContext>) {
    let installed = match &*ctx.runtime.lock().unwrap() {
        RuntimeState::Ready(version) => Some(version.clone()),
        _ => None,
    };
    if let Some(version) = installed {
        eprintln!("[olaz] runtime: kiểm tra xong, renderer đã có");
        update_runtime_in_background(ctx, version);
        return;
    }
    if !crate::runtime::auto_fetch() {
        eprintln!("[olaz] runtime: thiếu renderer, OLAZ_AUTO_FETCH=0 — chỉ mở trang chẩn đoán");
        let checks = crate::runtime::preflight(&ctx.userdata);
        ctx.set_runtime(RuntimeState::Missing(
            "tự tải đang tắt (OLAZ_AUTO_FETCH=0)".to_string(),
            checks,
        ));
        return;
    }
    install_in_background(ctx, "lần chạy đầu".to_string());
}

/// Checks zalo.me for a newer runtime and stages it for the next launch. The
/// current renderer keeps serving this session; startup already happens before
/// the window opens, so the check never delays the first paint.
fn update_runtime_in_background(ctx: Arc<AppContext>, installed: String) {
    if std::env::var("OLAZ_ASAR").is_ok() || std::env::var("OLAZ_INSTALLER").is_ok() {
        return;
    }
    if !crate::runtime::auto_update() {
        return;
    }
    // `--serve-only` is used by tests and tooling; do not pull 323 MB there.
    if std::env::args().any(|arg| arg == "--serve-only") {
        return;
    }
    std::thread::spawn(move || {
        match crate::runtime::update_if_newer(&ctx.userdata, ctx.debug, &installed) {
            Ok(Some(version)) => {
                eprintln!("[olaz] runtime: Zalo {version} sẽ dùng từ lần chạy sau")
            }
            Ok(None) => {}
            Err(error) => eprintln!("[olaz] runtime: không kiểm tra/cập nhật được — {error}"),
        }
    });
}

/// Installs the renderer off the UI thread; whatever state it leaves behind is
/// what `/__olaz/runtime` reports and the diagnostics page polls.
fn install_in_background(ctx: Arc<AppContext>, reason: String) {
    if matches!(*ctx.runtime.lock().unwrap(), RuntimeState::Fetching(_)) {
        return;
    }
    ctx.set_runtime(RuntimeState::Fetching(crate::runtime::preflight(&ctx.userdata)));
    std::thread::spawn(move || {
        eprintln!("[olaz] runtime: thiếu renderer ({reason}) — cài ở nền, cửa sổ vẫn dùng được");
        match crate::runtime::ensure(&ctx.userdata, ctx.debug) {
            Ok(Some(installed)) => match Asar::open(&installed.asar) {
                Ok(archive) => ctx.adopt_runtime(archive, &installed.version),
                Err(error) => ctx.set_runtime(RuntimeState::Failed(
                    format!("app.asar vừa cài không đọc được: {error}"),
                    crate::runtime::preflight(&ctx.userdata),
                )),
            },
            Ok(None) => ctx.set_runtime(RuntimeState::Missing(
                "tự tải đang tắt (OLAZ_AUTO_FETCH=0)".to_string(),
                crate::runtime::preflight(&ctx.userdata),
            )),
            Err(error) => {
                eprintln!("[olaz] không lấy được runtime Zalo: {error}");
                ctx.set_runtime(RuntimeState::Failed(error, crate::runtime::preflight(&ctx.userdata)));
            }
        }
    });
}

pub struct Running {
    pub port: u16,
}

/// The loopback origin *is* the renderer's storage origin: WebKitGTK keys
/// localStorage and IndexedDB by scheme+host+**port**. Binding port 0 gave every
/// launch a new origin, so Zalo's IndexedDB (its message store when
/// `fail_sqlite3=true`) always looked empty — no cached conversations, and the
/// sync started from zero each time. One fixed port keeps that data.
const DEFAULT_PORT: u16 = 27_182;
const DEV_EVAL_MAX_CODE: usize = 64 * 1024;
const DEV_EVAL_TIMEOUT: Duration = Duration::from_secs(20);
const DEV_COMPOSE_TIMEOUT: Duration = Duration::from_secs(90);
const DEV_COMPOSE_MAX_JOB: u64 = 1024 * 1024;

pub fn start(ctx: Arc<AppContext>) -> std::io::Result<Running> {
    let preferred = std::env::var("OLAZ_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or(DEFAULT_PORT);
    // Never fall back to another port: WebKitGTK keys IndexedDB/localStorage by
    // scheme+host+port, so a different port is a different, empty profile. A
    // second launch (or a lingering process) used to silently move the origin
    // and make every cached message/file disappear.
    let port = preferred;
    let server = Server::http(("127.0.0.1", port)).map_err(|error| {
        std::io::Error::other(format!(
            "cổng {port} đang bận ({error}) — có thể một Olaz khác vẫn đang chạy. \
             Đóng tiến trình đó rồi mở lại; đổi cổng sẽ đổi origin và làm mất cache \
             IndexedDB/localStorage."
        ))
    })?;
    let server = Arc::new(server);
    // Bind the aux server before `set_port` bakes the boot script: the script
    // carries both ports (see the comment below on why the aux port exists).
    let mut aux = None;
    for candidate in (port + 1)..=(port + 16) {
        match Server::http(("127.0.0.1", candidate)) {
            Ok(server) => {
                aux = Some(Arc::new(server));
                ctx.set_aux_port(candidate);
                break;
            }
            Err(_) => continue,
        }
    }
    ctx.set_port(port);
    for _ in 0..12 {
        let server = server.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                handle(&ctx, request);
            }
        });
    }
    // A second loopback server for the routes that hold or stream a connection
    // for a long time: the long-poll (up to 20 s) and `/__olaz/file` (whole
    // media files). WebKitGTK budgets ~6 connections per host for a page, and
    // two webviews keep two long-polls pinned permanently — photo requests to
    // the main port then queue behind them and the bubble's `<img>` sits in
    // `complete === false` forever even though the file is on disk. The aux
    // port has its own connection budget, so images and polls never compete
    // with the page's bridge traffic. Any port next to the fixed main port
    // works: nothing keys storage or origin off it.
    if let Some(aux_server) = aux {
        for _ in 0..8 {
            let aux_server = aux_server.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                for request in aux_server.incoming_requests() {
                    handle(&ctx, request);
                }
            });
        }
    }
    Ok(Running { port })
}

/// The macOS dev channel watches files in `<userData>/dev` while
/// `OlazDevEval` is on. Linux had only the HTTP endpoint, so long-lived
/// experiments had to paste code into curl each time. This runs the same file
/// tools against the two renderer windows:
///
///   eval.js          -> eval.out.json          (main renderer)
///   worker-eval.js   -> worker-eval.out.json   (hidden shared worker)
///   open-conv.txt    -> opens that conversation once
///   compose.json     -> compose.out.txt        (composer action, optional files)
///
/// Started by default in debug builds (`OLAZ_DEV_EVAL=0` disables it, release
/// builds need `OLAZ_DEV_EVAL=1`); a production launch therefore has no file
/// watcher and no way to read these files.
pub fn start_dev_harness(ctx: Arc<AppContext>) {
    if !ctx.dev_eval {
        return;
    }
    let dir = ctx.userdata.join("dev");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("[olaz] dev harness: không tạo được {} — {error}", dir.display());
        return;
    }
    eprintln!("[olaz] dev harness: {} (eval.js, worker-eval.js, open-conv.txt, compose.json)", dir.display());
    std::thread::spawn(move || {
        let mut eval_mtime = None;
        let mut worker_mtime = None;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            open_dev_conversation(&ctx, &dir);
            run_dev_compose(&ctx, &dir);
            watch_dev_eval(&ctx, &dir, "eval.js", "main", "eval.out.json", &mut eval_mtime);
            watch_dev_eval(&ctx, &dir, "worker-eval.js", "shared-worker", "worker-eval.out.json", &mut worker_mtime);
        }
    });
}

fn watch_dev_eval(
    ctx: &Arc<AppContext>,
    dir: &std::path::Path,
    input: &str,
    target: &str,
    output: &str,
    seen: &mut Option<SystemTime>,
) {
    let path = dir.join(input);
    let Ok(modified) = std::fs::metadata(&path).and_then(|meta| meta.modified()) else {
        return;
    };
    if seen.is_some_and(|last| modified <= last) {
        return;
    }
    *seen = Some(modified);
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            write_dev_output(dir, output, &dev_error(format!("không đọc được {input}: {error}")));
            return;
        }
    };
    if source.len() > DEV_EVAL_MAX_CODE {
        write_dev_output(
            dir,
            output,
            &dev_error(format!("{input} quá {} KB", DEV_EVAL_MAX_CODE / 1024)),
        );
        return;
    }
    let result = ctx
        .dev_eval(target, &source, DEV_EVAL_TIMEOUT)
        .unwrap_or_else(dev_error);
    write_dev_output(dir, output, &result);
}

fn open_dev_conversation(ctx: &Arc<AppContext>, dir: &std::path::Path) {
    let path = dir.join("open-conv.txt");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    let id = text.trim();
    if id.is_empty() {
        return;
    }
    let id = serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_string());
    let code = format!(
        "const c = window.__zaloChat; if (!c || typeof c.open !== 'function') throw new Error('chat adapter chưa sẵn sàng'); return await c.open({id});"
    );
    if let Err(error) = ctx.dev_eval("main", &code, DEV_EVAL_TIMEOUT) {
        eprintln!("[olaz] dev open-conv: {error}");
    }
}

fn run_dev_compose(ctx: &Arc<AppContext>, dir: &std::path::Path) {
    let path = dir.join("compose.json");
    let Ok(meta) = std::fs::metadata(&path) else {
        return;
    };
    if meta.len() > DEV_COMPOSE_MAX_JOB {
        let _ = std::fs::remove_file(&path);
        write_dev_output(dir, "compose.out.txt", &dev_error("compose.json quá 1 MB".to_string()));
        return;
    }
    let Ok(data) = std::fs::read_to_string(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    let job: Value = match serde_json::from_str(&data) {
        Ok(job) => job,
        Err(error) => {
            write_dev_output(dir, "compose.out.txt", &dev_error(format!("compose.json không hợp lệ: {error}")));
            return;
        }
    };
    let method = job.get("method").and_then(Value::as_str).unwrap_or("");
    if method.is_empty() {
        write_dev_output(dir, "compose.out.txt", &dev_error("compose.json thiếu method".to_string()));
        return;
    }
    let args = job.get("args").cloned().unwrap_or_else(|| json!([]));
    if !args.is_array() {
        write_dev_output(dir, "compose.out.txt", &dev_error("compose.json: args phải là mảng".to_string()));
        return;
    }
    let files: Vec<String> = job
        .get("files")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let method = serde_json::to_string(method).unwrap_or_else(|_| "\"\"".to_string());
    let args = serde_json::to_string(&args).unwrap_or_else(|_| "[]".to_string());
    let files = serde_json::to_string(&files).unwrap_or_else(|_| "[]".to_string());
    let code = format!(
        "const d = window.__olazLinux; if (!d || typeof d.devCompose !== 'function') throw new Error('dev harness chưa nạp'); return await d.devCompose({method}, {args}, {files});"
    );
    let result = ctx
        .dev_eval("main", &code, DEV_COMPOSE_TIMEOUT)
        .unwrap_or_else(dev_error);
    write_dev_output(dir, "compose.out.txt", &result);
}

fn write_dev_output(dir: &std::path::Path, name: &str, text: &str) {
    if let Err(error) = std::fs::write(dir.join(name), text) {
        eprintln!("[olaz] dev harness: không ghi được {name} — {error}");
    }
}

fn dev_error(message: String) -> String {
    json!({ "ok": false, "error": message }).to_string()
}

fn handle(ctx: &Arc<AppContext>, mut request: Request) {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (url.clone(), String::new()),
    };

    if ctx.debug {
        eprintln!("[http] {} {path}", request.method());
    }

    // CORS preflight from the aux server's cross-origin callers (the page's
    // long-poll and media reads live on the aux port — see `start`).
    if *request.method() == Method::Options {
        reply_preflight(request);
        return;
    }

    // The main renderer and the hidden shared-worker page share this bridge;
    // task routing needs to know which one is asking.
    let window = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("x-olaz-window"))
        .map(|header| header.value.as_str().to_string())
        .unwrap_or_else(|| "main".to_string());

    let mut body = Vec::new();
    let _ = request.as_reader().take(256 * 1024 * 1024).read_to_end(&mut body);
    let json_body = || -> Result<Value, String> {
        serde_json::from_slice(&body).map_err(|e| format!("body không phải JSON: {e}"))
    };

    match (request.method().clone(), path.as_str()) {
        (Method::Get, "/poll") => {
            let cursor = param(&query, "cursor").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            let poller = param(&query, "win").unwrap_or_else(|| window.clone());
            let (cursor, mut events) = ctx.events.since(cursor, Duration::from_millis(20_000));
            // Control events (dev-eval) ride along first: they are queued
            // per-window and must survive a realtime-event flood that would
            // evict them from the broadcast ring.
            let mut control = ctx.events.drain_control(&poller);
            if !control.is_empty() && ctx.debug {
                eprintln!("[poll] {} nhận {} control event(s)", poller, control.len());
            }
            events.splice(0..0, control.drain(..));
            reply(request, 200, "application/json", json!({ "cursor": cursor, "events": events }).to_string());
        }
        (Method::Get, "/__olaz/boot.js") => {
            reply(request, 200, "text/javascript; charset=utf-8", ctx.boot_script());
        }
        (Method::Get, "/__olaz/index.html") => {
            reply(
                request,
                200,
                "text/html; charset=utf-8",
                selftest_page(&ctx.renderer_entry()),
            );
        }
        (Method::Get, "/__olaz/runtime") => {
            let state = ctx.runtime.lock().unwrap().json();
            reply(request, 200, "application/json", state.to_string());
        }
        (Method::Get, "/__olaz/fetch-runtime") | (Method::Post, "/__olaz/fetch-runtime") => {
            install_in_background(ctx.clone(), "yêu cầu từ trang chẩn đoán".to_string());
            let state = ctx.runtime.lock().unwrap().json();
            reply(request, 200, "application/json", state.to_string());
        }
        (Method::Get, "/__olaz/runtime/check") | (Method::Post, "/__olaz/runtime/check") => {
            let settled = matches!(
                *ctx.runtime.lock().unwrap(),
                RuntimeState::Fetching(_) | RuntimeState::Ready(_)
            );
            if !settled {
                if crate::runtime::auto_fetch() {
                    install_in_background(ctx.clone(), "kiểm tra lại".to_string());
                } else {
                    let checks = crate::runtime::preflight(&ctx.userdata);
                    ctx.set_runtime(RuntimeState::Missing(
                        "tự tải đang tắt (OLAZ_AUTO_FETCH=0)".to_string(),
                        checks,
                    ));
                }
            }
            let state = ctx.runtime.lock().unwrap().json();
            reply(request, 200, "application/json", state.to_string());
        }
        // Local-only control surface for the phone→PC message sync: the sync
        // machine lives in the renderer, so these routes drive it through the
        // dev-eval channel. Loopback-only, so no extra gating beyond the
        // renderer being up (an idle renderer answers "not ready").
        (Method::Get, "/__olaz/transfer") => {
            const STATE_CODE: &str = r#"
const di = window.__zaloDI;
let ui2 = null, m2 = null, ctl = null;
for (const r of di(window.__zaloRequire)) {
  const i = r.instance;
  if (!i || typeof i !== 'object') continue;
  if (i.name === 'sync-ui-service') ui2 = i;
  if (typeof i.isFeatureEnabledBySyncSource === 'function' && typeof i.sync === 'function') m2 = i;
  if (typeof i.needTransferMessages === 'function') ctl = i;
}
if (!m2) throw new Error('sync module chưa sẵn sàng');
return JSON.stringify({
  enabled: m2.isFeatureEnabledBySyncSource(6),
  running: m2.isSyncRunning(),
  canSync: m2.canSync(6),
  shouldTransfer: ctl ? ctl.shouldTransferMessages() : null,
  ui: ui2 && ui2.getState ? ui2.getState() : null,
});
"#;
            match ctx.dev_eval("main", STATE_CODE, DEV_EVAL_TIMEOUT) {
                Ok(result) => reply(request, 200, "application/json", result),
                Err(error) => reply(request, 503, "application/json", json!({ "ok": false, "error": error }).to_string()),
            }
        }
        (Method::Post, "/__olaz/transfer/start") => {
            match ctx.dev_eval(
                "main",
                r#"
const di = window.__zaloDI;
let m2 = null;
for (const r of di(window.__zaloRequire)) {
  const i = r.instance;
  if (i && typeof i === 'object' && typeof i.isFeatureEnabledBySyncSource === 'function' && typeof i.sync === 'function') { m2 = i; break; }
}
if (!m2) throw new Error('sync module chưa sẵn sàng');
m2.sync(6);
return JSON.stringify({ started: true, running: m2.isSyncRunning() });
"#,
                DEV_EVAL_TIMEOUT,
            ) {
                Ok(result) => reply(request, 200, "application/json", result),
                Err(error) => reply(request, 503, "application/json", json!({ "ok": false, "error": error }).to_string()),
            }
        }
        (Method::Post, "/__olaz/transfer/cancel") => {
            match ctx.dev_eval(
                "main",
                r#"
const di = window.__zaloDI;
let m2 = null;
for (const r of di(window.__zaloRequire)) {
  const i = r.instance;
  if (i && typeof i === 'object' && typeof i.isFeatureEnabledBySyncSource === 'function' && typeof i.sync === 'function') { m2 = i; break; }
}
if (!m2) throw new Error('sync module chưa sẵn sàng');
m2.cancelSyncIfRunning('USER_CANCELLED');
return JSON.stringify({ cancelled: true, running: m2.isSyncRunning() });
"#,
                DEV_EVAL_TIMEOUT,
            ) {
                Ok(result) => reply(request, 200, "application/json", result),
                Err(error) => reply(request, 503, "application/json", json!({ "ok": false, "error": error }).to_string()),
            }
        }
        (Method::Post, "/__olaz/dev/eval") => {
            if !ctx.dev_eval {
                reply(request, 403, "application/json", json!({ "error": "OLAZ_DEV_EVAL chưa bật" }).to_string());
                return;
            }
            let payload = match json_body() {
                Ok(value) => value,
                Err(error) => {
                    reply(request, 400, "application/json", json!({ "error": error }).to_string());
                    return;
                }
            };
            let target = match dev_target(payload.get("target").and_then(Value::as_str)) {
                Ok(target) => target,
                Err(error) => {
                    reply(request, 400, "application/json", json!({ "error": error }).to_string());
                    return;
                }
            };
            let code = payload.get("code").and_then(Value::as_str).unwrap_or("");
            if code.is_empty() || code.len() > DEV_EVAL_MAX_CODE {
                reply(
                    request,
                    400,
                    "application/json",
                    json!({ "error": format!("code rỗng hoặc quá {} KB", DEV_EVAL_MAX_CODE / 1024) }).to_string(),
                );
                return;
            }
            match ctx.dev_eval(target, code, DEV_EVAL_TIMEOUT) {
                Ok(result) => reply(
                    request,
                    200,
                    "application/json",
                    json!({ "ok": true, "target": target, "result": result }).to_string(),
                ),
                Err(error) => reply(
                    request,
                    504,
                    "application/json",
                    json!({ "ok": false, "error": error }).to_string(),
                ),
            }
        }
        (Method::Get, "/__olaz/dev/probe") | (Method::Post, "/__olaz/dev/probe") => {
            if !ctx.dev_eval {
                reply(request, 403, "application/json", json!({ "error": "OLAZ_DEV_EVAL chưa bật" }).to_string());
                return;
            }
            match ctx.dev_eval("main", DEV_PROBE_SCRIPT, DEV_EVAL_TIMEOUT) {
                Ok(result) => reply(
                    request,
                    200,
                    "application/json",
                    json!({ "ok": true, "target": "main", "result": result }).to_string(),
                ),
                Err(error) => reply(
                    request,
                    504,
                    "application/json",
                    json!({ "ok": false, "error": error }).to_string(),
                ),
            }
        }
        (Method::Post, "/__olaz/dev/eval-result") => {
            if !ctx.dev_eval {
                reply(request, 403, "application/json", json!({ "error": "OLAZ_DEV_EVAL chưa bật" }).to_string());
                return;
            }
            let payload = match json_body() {
                Ok(value) => value,
                Err(error) => {
                    reply(request, 400, "application/json", json!({ "error": error }).to_string());
                    return;
                }
            };
            let id = payload.get("id").and_then(Value::as_u64).unwrap_or(0);
            let result = payload.get("result").and_then(Value::as_str).unwrap_or("");
            if let Some(sender) = ctx.dev_eval_waiters.lock().unwrap().remove(&id) {
                let _ = sender.send(result.to_string());
            }
            reply(request, 200, "application/json", json!({ "ok": true }).to_string());
        }
        // Local media for the renderer. The page is served over loopback, so WebKit
        // refuses a `file://` subresource from it (verified: a `file://` image errors,
        // while the same bytes through this route decode). Zalo's image component
        // addresses the copies the download manager just saved as `file://`, and
        // `web/olaz-linux.js` rewrites those to this route.
        // Debug builds: the sync sub-worker forwards its own diagnostics here,
        // because it cannot reach the DAL worker prelude's postMessage channel.
        (Method::Get, p) if p.starts_with("/__olaz/log") => {
            if !ctx.dev_eval {
                reply(request, 403, "text/plain; charset=utf-8", "OLAZ_DEV_EVAL chưa bật".to_string());
                return;
            }
            let message = param(&query, "m")
                .map(|v| percent_encoding::percent_decode_str(&v).decode_utf8_lossy().into_owned())
                .unwrap_or_default();
            if !message.is_empty() {
                eprintln!("[zhost] {message}");
            }
            reply(request, 200, "text/plain; charset=utf-8", "ok".to_string());
        }
        (Method::Get, p) if p.starts_with("/__olaz/file") => {
            let Some(raw) = param(&query, "path") else {
                reply(request, 400, "text/plain; charset=utf-8", "thiếu path".to_string());
                return;
            };
            let path = percent_encoding::percent_decode_str(&raw).decode_utf8_lossy().into_owned();
            match serve_local_file(ctx, &path) {
                Some(local) => reply_local_file(request, local),
                None => reply(request, 404, "text/plain; charset=utf-8", "not found".to_string()),
            }
        }
        (Method::Get, p) if p.starts_with("/app/") || p == "/app" => {
            let relative = p.trim_start_matches("/app").trim_start_matches('/');
            let relative = if relative.is_empty() { "login.html".to_string() } else { relative.to_string() };
            match ctx.serve_page(&relative) {
                Some(bytes) => {
                    let mime = content_type(&relative);
                    let body = if mime.starts_with("text/html") { inject_boot(&bytes) } else { bytes };
                    reply_bytes(request, 200, mime, body)
                }
                None => {
                    let message = if ctx.asar.lock().unwrap().is_some() {
                        format!("pc-dist/{relative} không có trong app.asar")
                    } else {
                        "Chưa cấu hình app.asar (OLAZ_ASAR)".to_string()
                    };
                    reply(request, 404, "text/plain; charset=utf-8", message);
                }
            }
        }
        (Method::Get, "/") => {
            let header = Header::from_bytes(&b"Location"[..], ctx.entry().as_bytes()).unwrap();
            let response = Response::empty(302).with_header(header);
            let _ = request.respond(response);
        }
        (Method::Post, "/sync") | (Method::Post, "/__olaz/bridge-sync") => match json_body() {
            Ok(payload) => match ctx.bridge.call_sync_request(&payload) {
                Ok(value) => reply(request, 200, "application/json", json!({ "value": value }).to_string()),
                Err(error) => reply(request, 200, "application/json", json!({ "error": error }).to_string()),
            },
            Err(error) => reply(request, 400, "application/json", json!({ "error": error }).to_string()),
        },
        (Method::Post, p) if p.starts_with("/bridge/") => {
            let handler = p.trim_start_matches("/bridge/");
            let payload = match json_body() {
                Ok(value) => value,
                Err(error) => {
                    reply(request, 400, "application/json", json!({ "ok": false, "error": error }).to_string());
                    return;
                }
            };
            match handler {
                "zbridge" => match ctx.bridge.dispatch(&payload, &window) {
                    Ok(value) => reply(request, 200, "application/json", json!({ "ok": true, "value": value }).to_string()),
                    Err(error) => reply(request, 200, "application/json", json!({ "ok": false, "error": error }).to_string()),
                },
                "znet" => {
                    let value = match payload.get("op").and_then(|v| v.as_str()) {
                        Some("http") => crate::net::perform_http(ctx, &payload),
                        Some("ws.open") => match crate::net::ws_open(ctx, &payload) {
                            Ok(()) => Value::Null,
                            Err(error) => json!({ "error": error }),
                        },
                        Some("ws.send") => {
                            crate::net::ws_send(&payload);
                            Value::Null
                        }
                        Some("ws.close") => {
                            crate::net::ws_close(&payload);
                            Value::Null
                        }
                        Some(other) => json!({ "error": format!("unknown op {other}") }),
                        None => json!({ "error": "bad message" }),
                    };
                    reply(request, 200, "application/json", json!({ "ok": true, "value": value }).to_string());
                }
                "zlog" => {
                    let message = payload.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    if !message.is_empty() {
                        eprintln!("[renderer] {}", truncate(message, 2000));
                    }
                    reply(request, 200, "application/json", json!({ "ok": true, "value": null }).to_string());
                }
                "zstate" => {
                    if ctx.debug {
                        eprintln!("[state] {}", truncate(&payload.to_string(), 400));
                    }
                    // The notification shim reports readiness and asks for the
                    // permission the way it expects the native host to answer
                    // (macOS: UNUserNotificationCenter). Grant it immediately —
                    // desktop notifications are user-approved by the OS daemon.
                    let granted = json!({
                        "kind": "js",
                        "target": "main",
                        "code": "try{window.__zaloNotification&&window.__zaloNotification.setPermission('granted');window.__zaloNotification.permissionResult&&window.__zaloNotification.permissionResult('granted');}catch(e){}"
                    });
                    let kind = payload
                        .get("notification")
                        .and_then(|n| n.get("kind"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if kind == "ready" || kind == "requestPermission" {
                        ctx.events.push_control(
                            u64::MAX - kind.len() as u64,
                            "main",
                            granted,
                        );
                    }
                    reply(request, 200, "application/json", json!({ "ok": true, "value": null }).to_string());
                }
                other => {
                    reply(request, 404, "application/json", json!({ "ok": false, "error": format!("handler {other} chưa có") }).to_string());
                }
            }
        }
        _ => reply(request, 404, "text/plain; charset=utf-8", "not found".to_string()),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "…"
}

fn dev_target(value: Option<&str>) -> Result<&'static str, String> {
    match value.unwrap_or("main") {
        "main" => Ok("main"),
        // The page shim names its second window `shared-worker`; the HTTP API
        // accepts the shorter `worker` spelling and normalises it to that.
        "worker" | "shared-worker" => Ok("shared-worker"),
        other => Err(format!("target không hợp lệ: {other} (dùng main hoặc worker)")),
    }
}

fn param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

fn reply(request: Request, status: u16, content_type: &str, body: String) {
    reply_bytes(request, status, content_type, body.into_bytes());
}

fn reply_bytes(request: Request, status: u16, content_type: &str, body: Vec<u8>) {
    let content = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap();
    // The renderer and its bridge share one loopback origin, so this is only a
    // guard for the sync XHR path if the origin ever differs — and the CORS the
    // aux server's cross-origin calls need (see `start`).
    let cors = Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap();
    let response = Response::from_data(body).with_status_code(status).with_header(content).with_header(cors);
    let _ = request.respond(response);
}

/// Answers a CORS preflight for the aux server: the page polls and reads media
/// from a second loopback port, so every non-simple request starts with one.
fn reply_preflight(request: Request) {
    let cors = |name: &[u8], value: &[u8]| Header::from_bytes(name, value).unwrap();
    let response = Response::empty(204)
        .with_header(cors(&b"Access-Control-Allow-Origin"[..], &b"*"[..]))
        .with_header(cors(&b"Access-Control-Allow-Methods"[..], &b"GET, POST, OPTIONS"[..]))
        .with_header(cors(&b"Access-Control-Allow-Headers"[..], &b"*"[..]))
        .with_header(cors(&b"Access-Control-Max-Age"[..], &b"86400"[..]));
    let _ = request.respond(response);
}

/// Injects the boot bundle into a page served out of `app.asar`.
///
/// Tauri's `initialization_script` does not run for a `WebviewUrl::External`
/// page on WebKitGTK (verified: no `/poll` from the window), so the script is
/// delivered by the document itself, at the top of `<head>` — that still runs
/// before any of the renderer's own scripts. `WKUserScript` on macOS bypassed
/// the page's CSP the same way, so the CSP meta tag is dropped for these pages.
fn inject_boot(html: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(html).into_owned();
    let text = strip_csp(&text);
    const TAG: &str = "<script src=\"/__olaz/boot.js\"></script>";
    let lower = text.to_ascii_lowercase();
    let insert_at = lower
        .find("<head")
        .and_then(|index| lower[index..].find('>').map(|end| index + end + 1))
        .or_else(|| lower.find("<html").and_then(|index| lower[index..].find('>').map(|end| index + end + 1)));
    let mut out = String::with_capacity(text.len() + TAG.len());
    match insert_at {
        Some(index) => {
            out.push_str(&text[..index]);
            out.push_str(TAG);
            out.push_str(&text[index..]);
        }
        None => {
            out.push_str(TAG);
            out.push_str(&text);
        }
    }
    out.into_bytes()
}

fn strip_csp(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(offset) = lower[cursor..].find("<meta") {
        let start = cursor + offset;
        let Some(end) = lower[start..].find('>') else { break };
        let end = start + end + 1;
        if !lower[start..end].contains("content-security-policy") {
            out.push_str(&html[cursor..end]);
        }
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    out
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "jxl" => "image/jxl",
        "bmp" => "image/bmp",
        "heic" => "image/heic",
        "mov" => "video/quicktime",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// A file the page may display: only inside the profile (the official client's
/// `ZaloPC` / `ZaloDownloads`) or the user's own media folders — a loopback URL
/// must never read an arbitrary path.
fn serve_local_file(ctx: &AppContext, path: &str) -> Option<LocalFile> {
    if !path.starts_with('/') {
        return None;
    }
    let target = std::fs::canonicalize(path).ok()?;
    if !target.is_file() {
        return None;
    }
    let home = home_dir();
    let mut roots = vec![ctx.userdata.clone()];
    for (key, fallback) in [
        ("XDG_DOWNLOAD_DIR", "Downloads"),
        ("XDG_PICTURES_DIR", "Pictures"),
        ("XDG_DOCUMENTS_DIR", "Documents"),
        ("XDG_VIDEOS_DIR", "Videos"),
        ("XDG_MUSIC_DIR", "Music"),
    ] {
        let root = std::env::var(key).map(PathBuf::from).unwrap_or_else(|_| home.join(fallback));
        roots.push(root);
    }
    let allowed = roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .any(|root| target.starts_with(root));
    if !allowed {
        return None;
    }
    let length = std::fs::metadata(&target).ok()?.len();
    let mime = media_type(&target)?;
    Some(LocalFile {
        path: target,
        mime,
        length,
    })
}

struct LocalFile {
    path: PathBuf,
    mime: &'static str,
    length: u64,
}

/// Answers `/__olaz/file` with `Range` support.
///
/// WebKitGTK's media element asks for byte ranges before it plays anything: a server
/// that only ever answers `200` + the whole body makes a video bubble show its
/// thumbnail and then refuse to play, and seeking is impossible. Images do not care,
/// so both shapes go through here.
fn reply_local_file(request: Request, local: LocalFile) {
    let range = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Range"))
        .map(|header| header.value.as_str().to_string());
    let if_none_match = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("If-None-Match"))
        .map(|header| header.value.as_str().to_string());
    // Media files are written atomically and never edited in place, so a strong
    // validator from size+mtime is stable. Without it WebKit re-downloads every
    // bubble image on each remount — the visible "lag" when scrolling back.
    let etag = std::fs::metadata(&local.path)
        .map(|meta| {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);
            format!("\"{:x}-{:x}\"", local.length, mtime)
        })
        .unwrap_or_default();
    let cache_headers = |mut response: Response<std::io::Cursor<Vec<u8>>>| {
        if let Ok(header) = Header::from_bytes(
            &b"Cache-Control"[..],
            &b"private, max-age=31536000, immutable"[..],
        ) {
            response.add_header(header);
        }
        if !etag.is_empty() {
            if let Ok(header) =
                Header::from_bytes(&b"ETag"[..], etag.as_bytes())
            {
                response.add_header(header);
            }
        }
        response
    };
    let headers = |mut response: Response<_>| {
        response.add_header(accept_ranges());
        response
    };
    // A conditional request without a Range wants `304` when nothing changed.
    if range.is_none() && !etag.is_empty() {
        if let Some(value) = &if_none_match {
            if value.split(',').any(|tag| tag.trim() == etag) {
                let not_modified = Response::empty(304)
                    .with_header(accept_ranges())
                    .with_header(Header::from_bytes(&b"ETag"[..], etag.as_bytes()).unwrap());
                let _ = request.respond(not_modified);
                return;
            }
        }
    }
    match range.as_deref().and_then(|value| parse_range(value, local.length)) {
        // Unsatisfiable: the spec wants `416` plus the size so the client can retry.
        Some(None) => {
            let content_range =
                Header::from_bytes(&b"Content-Range"[..], format!("bytes */{}", local.length).as_bytes()).unwrap();
            let response = Response::empty(416).with_header(content_range).with_header(accept_ranges());
            let _ = request.respond(response);
        }
        Some(Some((start, end))) => {
            let body = match read_range(&local.path, start, end) {
                Some(body) => body,
                None => {
                    let _ = request.respond(Response::empty(500));
                    return;
                }
            };
            let content_range = Header::from_bytes(
                &b"Content-Range"[..],
                format!("bytes {start}-{end}/{}", local.length).as_bytes(),
            )
            .unwrap();
            let _ = request.respond(cache_headers(headers(
                Response::from_data(body)
                    .with_status_code(206)
                    .with_header(content_header(local.mime))
                    .with_header(content_range)
                    .with_header(cors_header()),
            )));
        }
        None => match std::fs::read(&local.path) {
            Ok(body) => {
                let _ = request.respond(cache_headers(headers(
                    Response::from_data(body)
                        .with_header(content_header(local.mime))
                        .with_header(cors_header()),
                )));
            }
            Err(_) => {
                let _ = request.respond(Response::empty(500));
            }
        },
    }
}

fn read_range(path: &Path, start: u64, end: u64) -> Option<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let length = (end - start + 1) as usize;
    let mut body = vec![0u8; length];
    file.read_exact(&mut body).ok()?;
    Some(body)
}

/// One `Range` header, in the three shapes a media element sends.
///
/// `Ok(None)` for a malformed or multi-range request (answer `200`, the whole body),
/// `Ok(Some(None))` for a range past the end (answer `416`), and
/// `Ok(Some(Some((start, end))))` for the bytes to send — both ends inclusive.
fn parse_range(value: &str, length: u64) -> Option<Option<(u64, u64)>> {
    let spec = value.trim().strip_prefix("bytes=")?;
    // Multi-range answers need a multipart body; a full response is always valid.
    if spec.contains(',') {
        return None;
    }
    let (from, to) = spec.split_once('-')?;
    let (from, to) = (from.trim(), to.trim());
    let (start, end) = if from.is_empty() {
        // `bytes=-500`: the last 500 bytes.
        let suffix: u64 = to.parse().ok()?;
        if suffix == 0 || length == 0 {
            return Some(None);
        }
        (length.saturating_sub(suffix), length - 1)
    } else {
        let start: u64 = from.parse().ok()?;
        let end = if to.is_empty() { length.saturating_sub(1) } else { to.parse().ok()? };
        (start, end.min(length.saturating_sub(1)))
    };
    if start > end || start >= length {
        return Some(None);
    }
    Some(Some((start, end)))
}

fn accept_ranges() -> Header {
    Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap()
}

fn content_header(mime: &'static str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], mime.as_bytes()).unwrap()
}

fn cors_header() -> Header {
    Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap()
}

/// Downloaded media often has no extension (the official client names files by
/// id), and a cached variant can carry the extension of the *URL* the bytes came
/// from — which is not necessarily the format they are in. The magic bytes win
/// whenever they identify something: a JPEG stored as `.jxl` was served as
/// `image/jxl`, which WebKitGTK has no decoder for, so the `<img>` never painted.
fn media_type(path: &Path) -> Option<&'static str> {
    use std::io::Read;
    let mut head = Vec::with_capacity(16);
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(16).read_to_end(&mut head);
    }
    if let Some(sniffed) = sniff_media_type(&head) {
        return Some(sniffed);
    }
    Some(content_type(&path.to_string_lossy()))
}

/// Format from the first bytes of a file, or `None` when they identify nothing.
fn sniff_media_type(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if head.starts_with(b"\x89PNG") {
        return Some("image/png");
    }
    if head.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if head.starts_with(&[0xFF, 0x0A]) || (head.len() >= 12 && &head[4..8] == b"JXL ") {
        return Some("image/jxl");
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        return Some("video/mp4");
    }
    if head.starts_with(b"RIFF") && head.len() >= 12 && &head[8..12] == b"AVI " {
        return Some("video/x-msvideo");
    }
    if head.starts_with(b"ID3") || head.starts_with(&[0xFF, 0xFB]) {
        return Some("audio/mpeg");
    }
    if head.starts_with(b"%PDF") {
        return Some("application/pdf");
    }
    None
}

pub fn userdata_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("OLAZ_USERDATA") {
        return PathBuf::from(explicit);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".local/share"));
    base.join("olaz")
}

/// Self-install the desktop entry and icons so the window manager can match
/// the running window (`app_id`/`WM_CLASS` = `olaz-linux`, which the bundled
/// `Olaz.desktop` already declares via `StartupWMClass`). Without this,
/// launching the AppImage directly leaves the window unowned: no icon in the
/// dock and flaky minimize. Only runs from an AppImage (`$APPIMAGE` set); the
/// icon files come from the mounted image's own `usr/share/icons`. Idempotent.
pub fn install_desktop_entry() {
    let Ok(appimage) = std::env::var("APPIMAGE") else { return };
    let Some(exe) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|bin| bin.to_path_buf()))
    else {
        return;
    };
    let Some(share) = exe.parent().map(|usr| usr.join("share")) else { return };
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/share"));

    let sizes = ["512x512", "256x256@2x", "256x256", "128x128@2x", "128x128", "32x32"];
    let mut icon_installed = false;
    for size in sizes {
        let source = share.join(format!("icons/hicolor/{size}/apps/olaz-linux.png"));
        if !source.is_file() {
            continue;
        }
        let target = data.join(format!("icons/hicolor/{size}/apps/olaz-linux.png"));
        if target.is_file() && std::fs::read(&source).is_ok_and(|bytes| std::fs::read(&target).is_ok_and(|cur| cur == bytes)) {
            icon_installed = true;
            continue;
        }
        if std::fs::create_dir_all(target.parent().unwrap()).is_ok()
            && std::fs::copy(&source, &target).is_ok()
        {
            icon_installed = true;
        }
    }
    if !icon_installed {
        eprintln!("[olaz] desktop integration: không copy được icon");
        return;
    }

    let entry = data.join("applications/olaz-linux.desktop");
    let desktop = format!(
        "[Desktop Entry]\nCategories=Network;\nComment=Client Zalo cho Linux\nExec=\"{}\"\n\
         StartupWMClass=olaz-linux\nIcon=olaz-linux\nName=Olaz\nTerminal=false\nType=Application\n\
         StartupNotify=true\nX-GNOME-UsesNotifications=true\n",
        appimage
    );
    let changed = std::fs::read_to_string(&entry).map(|cur| cur != desktop).unwrap_or(true);
    if changed {
        if let Err(error) = std::fs::create_dir_all(entry.parent().unwrap()).and_then(|_| std::fs::write(&entry, desktop)) {
            eprintln!("[olaz] desktop integration: không ghi được {}: {error}", entry.display());
            return;
        }
        eprintln!("[olaz] desktop integration: đã cài {} (Exec={})", entry.display(), appimage);
    }
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// `OLAZ_ASAR` wins; otherwise look where a previous run may have unpacked the
/// official runtime (`<userData>/ZaloRuntime/Contents/Resources/app.asar`).
fn find_asar() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("OLAZ_ASAR") {
        let path = PathBuf::from(explicit);
        return if path.is_dir() { Some(path.join("app.asar")) } else { Some(path) };
    }
    let candidates = [
        userdata_dir().join("ZaloRuntime/Contents/Resources/app.asar"),
        PathBuf::from("app.asar"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

/// `getClientVersion:function(){return 694}` in the official main bundle — the
/// number official main puts in the User-Agent and the page query
/// (`OfficialZaloInstall.clientVersion(fromMainJS:)` on macOS).
fn client_version_of(main_js: &[u8]) -> Option<String> {
    let source = String::from_utf8_lossy(main_js);
    let marker = "getClientVersion:function(){return ";
    let start = source.find(marker)? + marker.len();
    let digits: String = source[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    (!digits.is_empty()).then_some(digits)
}

/// Injected before every page of the renderer, in the same order as
/// `OfficialClientHost.init` on macOS: networking first, then the members the
/// preload provided, then the feature adapters.
pub fn init_script(ctx: &AppContext, port: u16) -> String {
    let mut script = String::new();
    script.push_str(&format!(
        "window.__OLAZ_LINUX = {{ port: {port}, auxPort: {}, debug: {}, devEval: {} }};\n",
        ctx.aux_port(),
        ctx.debug,
        ctx.dev_eval
    ));
    script.push_str(include_str!("../../web/olaz-linux.js"));
    script.push('\n');
    script.push_str(&format!(
        "window.__zaloCwd = {};\n",
        Value::String(ctx.userdata.to_string_lossy().into_owned())
    ));
    script.push_str(HOST_SCRIPTS[0].1);
    script.push('\n');
    script.push_str(&ctx.bridge.spec_script());
    script.push('\n');
    for (name, source) in HOST_SCRIPTS.iter().skip(1) {
        if *name == "sqlite-host" {
            // Storage runs on IndexedDB here (see `Bridge::new`), so the SQLite
            // broker would only be dead weight.
            continue;
        }
        script.push_str(source);
        script.push_str(&format!("\n//# olaz-linux: {name}.js\n"));
    }
    // Blob/object-URL members of `$zFileManager` live in the page (see the file):
    // they read through `window.__zaloBridge`, which the scripts above installed.
    script.push_str(include_str!("../../web/file-manager.js"));
    script
}

/// The shared host scripts, embedded so the AppImage is self-contained.
const HOST_SCRIPTS: &[(&str, &str)] = &[
    ("network-shim", include_str!("../../../Sources/Olaz/HostScripts/network-shim.js")),
    ("local-modules", include_str!("../../../Sources/Olaz/HostScripts/local-modules.js")),
    ("bridge", include_str!("../../../Sources/Olaz/HostScripts/bridge.js")),
    ("sqlite-host", include_str!("../../../Sources/Olaz/HostScripts/sqlite-host.js")),
    ("state-adapter", include_str!("../../../Sources/Olaz/HostScripts/state-adapter.js")),
    ("chat-adapter", include_str!("../../../Sources/Olaz/HostScripts/chat-adapter.js")),
    ("media-store-adapter", include_str!("../../../Sources/Olaz/HostScripts/media-store-adapter.js")),
    ("card-actions-adapter", include_str!("../../../Sources/Olaz/HostScripts/card-actions-adapter.js")),
    ("sticker-adapter", include_str!("../../../Sources/Olaz/HostScripts/sticker-adapter.js")),
    ("composer-adapter", include_str!("../../../Sources/Olaz/HostScripts/composer-adapter.js")),
    ("message-actions-adapter", include_str!("../../../Sources/Olaz/HostScripts/message-actions-adapter.js")),
    ("transfer-adapter", include_str!("../../../Sources/Olaz/HostScripts/transfer-adapter.js")),
    ("conversations-adapter", include_str!("../../../Sources/Olaz/HostScripts/conversations-adapter.js")),
    ("conversation-actions-adapter", include_str!("../../../Sources/Olaz/HostScripts/conversation-actions-adapter.js")),
    ("contacts-adapter", include_str!("../../../Sources/Olaz/HostScripts/contacts-adapter.js")),
    ("group-create-adapter", include_str!("../../../Sources/Olaz/HostScripts/group-create-adapter.js")),
    ("notification-adapter", include_str!("../../../Sources/Olaz/HostScripts/notification-adapter.js")),
    ("zcloud-adapter", include_str!("../../../Sources/Olaz/HostScripts/zcloud-adapter.js")),
    ("poll-adapter", include_str!("../../../Sources/Olaz/HostScripts/poll-adapter.js")),
    ("call-adapter", include_str!("../../../Sources/Olaz/HostScripts/call-adapter.js")),
];

/// The browser-side half of macOS `EngineProbe`: the React fiber tree (what
/// the official client actually mounted), feature detection and a DOM summary.
/// Served by `/__olaz/dev/probe`, which is the Linux equivalent of the old
/// `Olaz --engine-probe` final report.
const DEV_PROBE_SCRIPT: &str = r##"
const el = document.getElementById('app');
const apis = JSON.stringify(Object.fromEntries([
  'requestIdleCallback', 'cancelIdleCallback', 'BroadcastChannel', 'SharedWorker', 'OffscreenCanvas',
  'createImageBitmap', 'showOpenFilePicker', 'CompressionStream', 'structuredClone'
].map((k) => [k, typeof window[k]]).concat([
  ['performance.memory', typeof performance.memory],
  ['navigator.userAgentData', typeof navigator.userAgentData],
  ['navigator.locks', typeof navigator.locks],
  ['window.chrome', typeof window.chrome],
  ['navigator.connection', typeof navigator.connection]
])));
let fibers = 'no #app';
if (el) {
  const key = Object.keys(el).find((k) => k.startsWith('__reactContainer') || k.startsWith('_reactRootContainer'));
  const root = el._reactRootContainer ? el._reactRootContainer._internalRoot : (key ? el[key] : null);
  const fiber = root && root.current ? root.current : root;
  const lines = [];
  const name = (f) => (f.type && (f.type.displayName || f.type.name)) || (typeof f.type === 'string' ? f.type : 'tag' + f.tag);
  const walk = (f, d) => {
    let n = 0;
    while (f && lines.length < 160) {
      const portal = f.tag === 4 ? (() => {
        const c = f.stateNode && f.stateNode.containerInfo;
        return c ? ' [portal → ' + c.nodeName + (c.id ? '#' + c.id : '') + '.' +
          String(c.className || '').slice(0, 40) + ' connected=' + c.isConnected + ' kids=' +
          c.childElementCount + ' parent=' + (c.parentNode ? c.parentNode.nodeName + '.' +
          String(c.parentNode.className || '').slice(0, 30) : 'none') + ']' : '';
      })() : '';
      const susp = portal + (f.tag === 13 ? (f.memoizedState ? ' [SUSPENDED]' : ' [suspense ok]') : '');
      const off = f.tag === 22 || f.tag === 23 ? ' [offscreen ' + JSON.stringify(f.memoizedProps && f.memoizedProps.mode) + ']' : '';
      if (d < 40) lines.push('  '.repeat(Math.min(d, 20)) + name(f) + susp + off + (f.lanes ? ' lanes=' + f.lanes : ''));
      if (f.child) walk(f.child, d + 1);
      f = f.sibling;
      if (++n > 30) break;
    }
  };
  walk(fiber, 0);
  fibers = 'ROOT pendingLanes=' + (root && root.pendingLanes) + ' callbackNode=' + !!(root && root.callbackNode) + '\n' + lines.join('\n');
}
const walkDom = (node, d) => {
  if (!node || d > 7) return '';
  const cs = getComputedStyle(node);
  const tag = node.tagName.toLowerCase() + (node.id ? '#' + node.id : '') +
    (node.className && typeof node.className === 'string' ? '.' + node.className.trim().split(/\s+/).slice(0, 3).join('.') : '');
  const flags = (cs.display === 'none' ? ' [none]' : '') + (cs.visibility === 'hidden' ? ' [hidden]' : '') +
    (cs.opacity === '0' ? ' [op0]' : '');
  return '  '.repeat(d) + tag + flags + '\n' +
    Array.from(node.children).slice(0, 12).map((child) => walkDom(child, d + 1)).join('');
};
const summary = [
  'ELEMENTS ' + document.querySelectorAll('*').length + ' rootKids ' + (el ? el.children.length : 0),
  ...Array.from(document.querySelectorAll('[class*=qr], [class*=login]')).slice(0, 25).map((e) =>
    e.tagName + '.' + String(e.className).slice(0, 60) + ' ' +
    Math.round(e.getBoundingClientRect().width) + 'x' + Math.round(e.getBoundingClientRect().height)),
  JSON.stringify({
    title: document.title,
    text: document.body ? document.body.innerText.slice(0, 300) : '',
    imgs: Array.from(document.images).map((i) => i.src.slice(0, 60)),
    canvas: document.querySelectorAll('canvas').length
  }),
  document.body ? walkDom(document.body, 0).slice(0, 6000) : ''
].join('\n');
return { apis, fibers, summary };
"##;

/// Shown when no `app.asar` is configured: verifies the bridge end to end
/// without VNG's bundle, which is what makes this port testable at all.
fn selftest_page(login_url: &str) -> String {
    let page = r#"<!doctype html>
<html lang="vi"><head><meta charset="utf-8"><title>Olaz Linux — chẩn đoán</title>
<style>
 body{font:14px/1.5 system-ui,sans-serif;background:#101418;color:#e7edf3;margin:0;padding:32px}
 h1{font-size:18px;margin:0 0 4px} p{color:#93a4b3;margin:0 0 20px}
 pre{background:#161c22;border:1px solid #253040;border-radius:8px;padding:16px;white-space:pre-wrap}
 .ok{color:#63d489} .bad{color:#ff7b72}
 code{color:#8cc8ff}
 .rt{background:#161c22;border:1px solid #253040;border-radius:8px;padding:16px;margin:0 0 20px}        .rt .muted{color:#93a4b3;margin-top:8px}
 button{font:inherit;margin-top:10px;padding:8px 14px;border-radius:8px;border:1px solid #2f6f4f;background:#1c3b2c;color:#c8f0d8;cursor:pointer}
 button:disabled{opacity:.6;cursor:default}
 button.secondary{margin-left:8px;border-color:#2b3a4a;background:#1b232c;color:#b9c8d6}
 ul.checks{list-style:none;margin:10px 0 0;padding:0}
 ul.checks li{margin:2px 0;color:#93a4b3}
</style><script src="/__olaz/boot.js"></script></head>
<body>
<h1>Olaz Linux — trang chẩn đoán</h1>
<div id="rt" class="rt">đang kiểm tra runtime…</div>
<p>Trang này kiểm tra bridge Rust ↔ host script:</p>
<pre id="out">đang chạy…</pre>
<script>
// Runtime: the window opens here on every launch — startup only checks what is
// installed, then installs in the background if needed. `fetch` is proxied to
// the Zalo origin by the shim; the raw one talks to this loopback host.
const rt = document.getElementById('rt');
const raw = (window.__olazLinux && window.__olazLinux.fetch) || ((p, i) => fetch(p, i));
const esc = (text) => String(text == null ? '' : text)
  .replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
let fetchStarted = 0;
function checksHtml(checks) {
  if (!checks || !checks.length) return '';
  return '<ul class="checks">' + checks.map((c) =>
    '<li class="' + (c.ok ? 'ok' : 'bad') + '">' + (c.ok ? '✓' : '✗') + ' ' + esc(c.name) +
    ' — ' + esc(c.detail) + '</li>').join('') + '</ul>';
}
function addButton(label, action, secondary) {
  const button = document.createElement('button');
  button.textContent = label;
  if (secondary) button.className = 'secondary';
  button.onclick = async () => {
    button.disabled = true;
    try { await action(); } catch (e) {}
    setTimeout(showRuntime, 600);
  };
  rt.appendChild(button);
}
async function showRuntime() {
  let status;
  try {
    status = await (await raw('/__olaz/runtime')).json();
  } catch (e) {
    rt.textContent = 'không đọc được trạng thái runtime: ' + e.message;
    setTimeout(showRuntime, 2000);
    return;
  }
  if (status.state === 'ready') {
    rt.innerHTML = '<span class="ok">✓</span> runtime Zalo ' + esc(status.version) + ' — đang mở renderer…';
    location.assign('/app/login.html');
    return;
  }
  if (status.state === 'fetching') {
    fetchStarted = fetchStarted || Date.now();
    const seconds = Math.round((Date.now() - fetchStarted) / 1000);
    rt.innerHTML = '⏳ đang tải và cài renderer Zalo từ VNG (' + seconds + ' giây, khoảng 323 MB, chỉ một lần)' +
      '<div class="muted">Cửa sổ tự chuyển sang Zalo khi xong — không phải khởi động lại. ' +
      'Bridge bên dưới vẫn kiểm tra được trong lúc chờ.</div>' + checksHtml(status.checks);
    setTimeout(showRuntime, 1000);
    return;
  }
  fetchStarted = 0;
  const failed = status.state === 'failed';
  rt.innerHTML = (failed ? '<span class="bad">✗</span> cài đặt lỗi: ' : '<span class="bad">✗</span> chưa có runtime: ') +
    esc(status.message || 'chưa rõ lý do') + checksHtml(status.checks) +
    '<div class="muted">Bản macOS cũng tự tải Zalo PC ở lần chạy đầu. Ở đây cài bằng nút dưới, ' +
    'hoặc <code>OLAZ_ASAR=/đường/dẫn/app.asar</code>, hoặc <code>olaz-linux --fetch-runtime</code> trong terminal. ' +
    'Giải nén cần <code>p7zip-full</code>.</div>';
  addButton(failed ? 'Thử cài lại' : 'Tải và cài Zalo PC từ VNG', () => raw('/__olaz/fetch-runtime', { method: 'POST' }));
  addButton('Kiểm tra lại', () => raw('/__olaz/runtime/check', { method: 'POST' }), true);
}
showRuntime();

const out = document.getElementById('out');
const lines = [];
const log = (ok, text) => lines.push((ok ? '✓ ' : '✗ ') + text);
const missing = () => Promise.reject(new Error('bridge chưa nạp'));
const bridge = window.__zaloBridge || { callAsync: missing, callSync: () => { throw new Error('bridge chưa nạp'); } };
(async () => {
  log(!!window.__OLAZ_LINUX, 'boot bundle đã chạy (port ' + ((window.__OLAZ_LINUX || {}).port || '?') + ')');
  log(!!window.__zaloBridgeSpec, 'spec: ' + (window.__zaloBridgeSpec ? Object.keys(window.__zaloBridgeSpec.ipc || {}).length + ' thành viên IPC' : 'thiếu'));
  log(!!bridge, 'bridge.js đã nạp');
  try {
    const p = await bridge.callAsync('$zelectron.getPath', ['userData']);
    log(typeof p === 'string' && p.length > 0, 'getPath(userData) → ' + p);
  } catch (e) { log(false, 'getPath: ' + e.message); }
  try {
    const info = await bridge.callAsync('$znode.fs.stat', [window.__zaloCwd]);
    log(info && info.isDirectory && info.isDirectory(), 'fs.stat thư mục dữ liệu → ' + info.size + ' B');
  } catch (e) { log(false, 'fs.stat: ' + e.message); }
  try {
    bridge.callSync('$zelectron.storeWrite', ['__olaz_selftest', { at: Date.now() }]);
    const back = bridge.callSync('$zelectron.storeRead', ['__olaz_selftest']);
    log(back && typeof back.at === 'number', 'electron-store đọc/ghi đồng bộ');
  } catch (e) { log(false, 'store: ' + e.message); }
  try {
    const h = bridge.callSync('$znode.crypto.__hashCreate', ['sha256']);
    bridge.callSync('$znode.crypto.__hashUpdate', [h, 'olaz']);
    const digest = bridge.callSync('$znode.crypto.__hashDigest', [h, 'hex']);
    log(digest === 'b9c3e1f0a1c9a5f0b0c1e2d3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d0e1f2a3' || /^[0-9a-f]{64}$/.test(digest), 'crypto sha256 → ' + digest.slice(0, 16) + '…');
  } catch (e) { log(false, 'crypto: ' + e.message); }
  try {
    const res = await Promise.race([
      fetch('https://chat.zalo.me/', { method: 'GET' }),
      new Promise((_, reject) => setTimeout(() => reject(new Error('quá 5s')), 5000)),
    ]);
    log(res.status > 0, 'mạng qua proxy Rust → HTTP ' + res.status);
  } catch (e) { log(false, 'mạng: ' + e.message); }
  out.innerHTML = lines.map((l) => '<span class="' + (l[0] === '✓' ? 'ok' : 'bad') + '">' + l + '</span>').join('\n');
  document.title = 'Olaz — ' + lines.filter((l) => l[0] === '✓').length + '/' + lines.length + ' đạt';
  // Reported to the host log so the shell can be checked without a screenshot.
  // `fetch` is proxied by network-shim here; the shim's own poster is raw.
  try { if (window.__olazLinux && window.__olazLinux.post) window.__olazLinux.post('SELFTEST ' + lines.join(' | ')); } catch {}
})().catch((e) => { out.textContent = 'lỗi: ' + e.message; });
</script>
</body></html>"#;
    // The renderer reads `type`/`client_version` from this URL; without them it
    // asks for a QR as an unknown client and the phone rejects the code.
    page.replace(
        "location.assign('/app/login.html');",
        &format!("location.assign({});", Value::String(login_url.to_string())),
    )
}

#[cfg(test)]
mod local_file_tests {
    use super::{parse_range, sniff_media_type};

    #[test]
    fn range_parses_the_shapes_a_media_element_sends() {
        // A closed range, clamped to the file: 100 bytes, `bytes=10-999`.
        assert_eq!(parse_range("bytes=10-999", 100), Some(Some((10, 99))));
        // Open-ended: everything from 40 on.
        assert_eq!(parse_range("bytes=40-", 100), Some(Some((40, 99))));
        // Suffix: the last 25 bytes.
        assert_eq!(parse_range("bytes=-25", 100), Some(Some((75, 99))));
        // Whole file.
        assert_eq!(parse_range("bytes=0-99", 100), Some(Some((0, 99))));
    }

    #[test]
    fn range_past_the_end_is_unsatisfiable() {
        assert_eq!(parse_range("bytes=100-", 100), Some(None));
        assert_eq!(parse_range("bytes=200-300", 100), Some(None));
        assert_eq!(parse_range("bytes=-0", 100), Some(None));
        assert_eq!(parse_range("bytes=0-", 0), Some(None));
    }

    #[test]
    fn range_ignores_what_it_cannot_answer() {
        // A multi-range answer needs a multipart body; the caller sends the whole file.
        assert_eq!(parse_range("bytes=0-1,5-6", 100), None);
        assert_eq!(parse_range("items=0-1", 100), None);
        assert_eq!(parse_range("bytes=abc-def", 100), None);
        assert_eq!(parse_range("bytes", 100), None);
    }

    #[test]
    fn magic_bytes_beat_a_misleading_extension() {
        // A JPEG cached under a `.jxl` name (the URL said jxl, the CDN sent JPEG) used to
        // be served as `image/jxl`, which WebKitGTK cannot decode.
        assert_eq!(sniff_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_media_type(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(sniff_media_type(b"GIF89a"), Some("image/gif"));
        let mut mp4 = vec![0u8, 0, 0, 0x1C];
        mp4.extend_from_slice(b"ftypmp42");
        assert_eq!(sniff_media_type(&mp4), Some("video/mp4"));
        // An expired link answers HTML: nothing identifies it, so the extension decides.
        assert_eq!(sniff_media_type(b"<!DOCTYPE html>"), None);
    }
}

#[cfg(test)]
mod sync_patch_tests {
    use super::{
        expose_webpack_require, patch_sync_asset, patch_sync_global_id_resolver, patch_sync_insert_error,
        patch_sync_insert_success, patch_worker_pool_timeout,
    };

    #[test]
    fn dal_worker_gets_the_database_prelude() {
        let out = patch_sync_asset("dal-worker.abc.js", b"official".to_vec());
        assert!(out.starts_with(b"\n(() => {"));
        assert!(out.ends_with(b"\nofficial"));
        assert!(String::from_utf8_lossy(&out).contains("__swReq"));
    }

    #[test]
    fn sync_sub_worker_exposes_its_require() {
        let input = b"x}i.e=function(e){var t=[],n=a[e];".to_vec();
        let out = String::from_utf8(expose_webpack_require(input)).unwrap();
        assert!(out.contains("}self.__swReq=i;i.e=function"));
    }

    #[test]
    fn main_startup_resolver_gets_the_global_id_fallback() {
        let input = b"cI.oneToOnePCNoised2gNoised=async e=>{return await cI.resolveWithRetry(e,(e=>eo.default.getGlobalIdsFromUserIds(e)))};".to_vec();
        let out = String::from_utf8(patch_sync_global_id_resolver(input)).unwrap();
        assert!(out.contains("__swzGlobalIdFallback"));
        assert!(out.contains("cI.resolveWithRetry(e,(async e=>"));
    }

    #[test]
    fn insert_error_is_forwarded() {
        let input = b"catch(n){throw this.logger.zsymb(18,\"cEXelp\",\"Error when insert messages to db\",n),n}".to_vec();
        let out = String::from_utf8(patch_sync_insert_error(input)).unwrap();
        assert!(out.contains("insertToDb-failed"));
    }

    #[test]
    fn sync_worker_pool_gets_a_linux_task_budget() {
        let input = b"class wl{constructor(e,t=2,s=1,r=0,i=1e4){this.route=e,this.taskTimeout=i,this.workers=[]}}".to_vec();
        let out = String::from_utf8(patch_sync_asset("app/lazy/chunk.abc.js", input)).unwrap();
        assert!(out.contains("this.taskTimeout=Math.max(i,1.2e5),this.workers=[]"));
        assert!(!out.contains("this.taskTimeout=i,"));
    }

    #[test]
    fn insert_counts_are_forwarded() {
        let input = b"xconst a=await this.insertMessage(e);y".to_vec();
        let out = String::from_utf8(patch_sync_insert_success(input)).unwrap();
        assert!(out.contains("insert-msgs"));
        assert!(out.contains("insertMessage(e);self.__zhostFwd"));
        assert!(!out.contains(",,"));
        // A stray paren/comma here makes the whole sub-worker fail to parse,
        // which surfaces as MSG_IMPORT_FAILED at sync time.
        assert_eq!(out.matches('(').count(), out.matches(')').count());
        assert_eq!(out.matches('{').count(), out.matches('}').count());
    }

    #[test]
    fn sync_sub_worker_gets_a_log_forwarder() {
        let out = String::from_utf8(patch_sync_asset("sync-v2-sub-worker.abc.js", b"official".to_vec())).unwrap();
        assert!(out.starts_with("\n(() => {"));
        assert!(out.contains("self.__zhostFwd"));
        assert!(out.contains("/__olaz/log"));
        assert!(out.ends_with("official"));
    }

    #[test]
    fn worker_pool_patch_leaves_other_bundles_alone() {
        let input = b"const a=1;this.workers=[]".to_vec();
        assert_eq!(patch_worker_pool_timeout(input.clone()), input);
    }

    #[test]
    fn resolver_patch_leaves_an_unknown_shape_alone() {
        let input = b"cI.resolveWithRetry(e,e=>eo.default.getGlobalIdsFromUserIds(e))".to_vec();
        let out = patch_sync_global_id_resolver(input.clone());
        assert_eq!(out, input, "a call without the arrow-argument parentheses must not be patched");
    }
}

#[cfg(test)]
mod dev_harness_tests {
    use super::dev_target;

    #[test]
    fn dev_target_defaults_to_main() {
        assert_eq!(dev_target(None).unwrap(), "main");
        assert_eq!(dev_target(Some("main")).unwrap(), "main");
    }

    #[test]
    fn dev_target_accepts_both_worker_names() {
        assert_eq!(dev_target(Some("worker")).unwrap(), "shared-worker");
        assert_eq!(dev_target(Some("shared-worker")).unwrap(), "shared-worker");
    }

    #[test]
    fn dev_target_rejects_unknown_targets() {
        assert!(dev_target(Some("renderer")).is_err());
    }
}

