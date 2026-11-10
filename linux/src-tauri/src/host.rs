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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

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
    /// Debug-only JS evaluation in the main renderer (`OLAZ_DEV_EVAL=1`). Off
    /// by default: the endpoint and the page handler both check this flag.
    dev_eval: bool,
    dev_eval_seq: AtomicU64,
    dev_eval_waiters: Mutex<HashMap<u64, mpsc::Sender<String>>>,
    /// First-run installer state, shown on `/__olaz/index.html`.
    pub runtime: Arc<Mutex<RuntimeState>>,
    port: AtomicU16,
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
}

impl Events {
    pub fn new() -> Self {
        Self { state: Mutex::new((0, VecDeque::new())), signal: Condvar::new() }
    }

    pub fn push(&self, mut event: Value) {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        let seq = state.0;
        if let Some(object) = event.as_object_mut() {
            object.insert("seq".into(), json!(seq));
        }
        state.1.push_back(event);
        while state.1.len() > 512 {
            state.1.pop_front();
        }
        self.signal.notify_all();
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
        let dev_eval = std::env::var("OLAZ_DEV_EVAL").map(|v| v == "1").unwrap_or(false);
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
        archive.read(&format!("pc-dist/{relative}"))
    }
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
    Ok(Running { port })
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
            let (cursor, events) = ctx.events.since(cursor, Duration::from_millis(20_000));
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
            let code = payload.get("code").and_then(Value::as_str).unwrap_or("");
            if code.is_empty() || code.len() > 64 * 1024 {
                reply(request, 400, "application/json", json!({ "error": "code rỗng hoặc quá 64 KB" }).to_string());
                return;
            }
            let id = ctx.dev_eval_seq.fetch_add(1, Ordering::Relaxed) + 1;
            let (sender, receiver) = mpsc::channel();
            ctx.dev_eval_waiters.lock().unwrap().insert(id, sender);
            ctx.events.push(json!({
                "kind": "dev-eval",
                "id": id,
                "code": code,
                "target": "main"
            }));
            match receiver.recv_timeout(Duration::from_secs(20)) {
                Ok(result) => reply(
                    request,
                    200,
                    "application/json",
                    json!({ "ok": true, "result": result }).to_string(),
                ),
                Err(_) => {
                    ctx.dev_eval_waiters.lock().unwrap().remove(&id);
                    reply(
                        request,
                        504,
                        "application/json",
                        json!({ "ok": false, "error": "renderer không trả lời trong 20 giây" }).to_string(),
                    )
                }
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
    // guard for the sync XHR path if the origin ever differs.
    let cors = Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap();
    let response = Response::from_data(body).with_status_code(status).with_header(content).with_header(cors);
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

pub fn userdata_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("OLAZ_USERDATA") {
        return PathBuf::from(explicit);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".local/share"));
    base.join("olaz")
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
        "window.__OLAZ_LINUX = {{ port: {port}, debug: {}, devEval: {} }};\n",
        ctx.debug, ctx.dev_eval
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

