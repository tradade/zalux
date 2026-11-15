//! The Electron-preload bridge, reimplemented in Rust.
//!
//! `HostScripts/bridge.js` drives `window.$z*`: it resolves each member to
//! `async`/`sync`/`event` modes from `window.__zaloBridgeSpec` and forwards the
//! call here — the same split `Sources/Olaz/ZaloRuntime/ZaloBridge.swift` uses on
//! macOS. `HostScripts/local-modules.js` already implements the parts that are
//! pure JavaScript (`path`, `os`, electron-store, crypto objects), so this side
//! only has to provide their primitives.
//!
//! Two intentional departures from the macOS build:
//! * sync members are reached through the loopback server, never a
//!   URL-scheme handler (see `host.rs`);
//! * storage is forced to IndexedDB (`sqlite-available → false`), which WebKitGTK
//!   implements natively, instead of porting Zalo's SQLCipher DAL host.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use reqwest::cookie::CookieStore;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256, Sha384, Sha512};

use crate::host::Events;

/// Machine-readable channel table generated from the official `preload-render.js`.
const BRIDGE_TABLE: &str = include_str!("../../../reanalysis/bridge-table.json");

/// Globals the official preload exposed (`bridge-spec.md` §0.3).
const GLOBALS: &[&str] = &[
    "MetriczGlobal", "$zenv", "$zcommon", "$znode", "$zwindow", "$zresource", "$zlogger", "$zupdater",
    "$zapp", "$zdownload", "$zdb", "$zsessionManager", "$zmulti", "$zcall", "$zsharedWorker",
    "$zelectron", "$zelectronNative", "$zuri", "$zscreencap", "$zlogin", "$zperf", "$zconfig",
    "$zMetric", "$zFileManager", "$zpc", "$zFeatures", "$zcloud", "$zMsgChannel", "$zscript",
    "$zInAppPayment", "$capp", "$zbackup", "$zpreload", "$zsqlitebw", "$zsafeStorage",
    "$zMigrateDrive", "$zTFModels", "$znetwork", "$zprotocol", "$znoti",
    // preload-shared-worker exposed the native SQLite binding under this bare
    // global; `shared-worker.js` reads it at load time, before any call.
    "$zsqlite",
];

/// Members implemented here (everything else falls back to `bridge.js`'s
/// logging stub, which settles its promise — never hangs the renderer).
const MEMBERS: &[(&str, &str)] = &[
    ("$zelectron.storeRead", "sync"),
    ("$zelectron.storeWrite", "sync"),
    ("$zelectron.storePath", "sync"),
    ("$zelectron.getPathSync", "sync"),
    ("$zelectron.getAppPathSync", "sync"),
    ("$zelectron.getInfoScreenOfMouseSync", "sync"),
    ("$zelectron.getPath", "async"),
    ("$zelectron.getAppVersion", "async"),
    ("$zelectron.setAppCookie", "async"),
    ("$zelectron.setCookie", "async"),
    ("$zelectron.clearCookie", "async"),
    ("$zelectronNative.clipboardText", "sync"),
    ("$zelectronNative.clipboardWriteText", "sync"),
    ("$zelectronNative.clipboardFormats", "sync"),
    ("$zelectronNative.clipboardWriteImage", "sync"),
    ("$olaz.jxl.decode", "async"),
    ("$olaz.jxl.info", "async"),
    ("$olaz.photo.cache", "async"),
    ("$zFeatures.mainless.execRaw", "async"),
    ("$zFeatures.genThumb.writeSavePathFilePhoto", "async"),
    ("$zFileManager.getSentryConfig", "sync"),
    ("$zFileManager.saveSentryConfig", "async"),
    ("$zFileManager.getViewerkey", "async"),
    ("$zFileManager.writeRecentId", "async"),
    ("$zFileManager.writeZLog", "async"),
    ("$zFileManager.writeZLogBetween", "async"),
    ("$zFileManager.readZLog", "async"),
    ("$zFileManager.scandirLog", "async"),
    ("$zFileManager.getExtraLogFiles", "async"),
    ("$zFileManager.getQosLog", "async"),
    ("$zFileManager.getActionLog", "async"),
    ("$zFileManager.readUptimeLog", "async"),
    ("$zFileManager.removeQosLog", "async"),
    ("$zFileManager.removeActionLog", "async"),
    ("$zFileManager.writeActionLog", "async"),
    ("$zFileManager.writeUptimeLog", "async"),
    ("$zFileManager.truncateFileUpto", "async"),
    ("$zFileManager.cleanupLogFileDescriptors", "async"),
    ("$zFileManager.initBinaryLogFile", "async"),
    ("$zFileManager.sealCalfLog", "async"),
    ("$zFileManager.mergeCalfLogGroup", "async"),
    ("$zFileManager.writeLogRange", "async"),
    ("$zFileManager.trustBlob", "async"),
    ("$zFileManager.getInstallSource", "async"),
    ("$zFileManager.getFileArrayBuffer", "async"),
    ("$zFileManager.readFileAsync", "async"),
    ("$zFileManager.svcStatAsync", "async"),
    ("$zFileManager.svcReadPath", "async"),
    ("$zFileManager.writeBufferToPath", "async"),
    ("$zFileManager.getMd5LargeFileObject", "async"),
    ("$zwindow.pickFolder", "async"),
    ("$znode.fs.readFileSync", "sync"),
    ("$znode.fs.writeFileSync", "sync"),
    ("$znode.fs.appendFileSync", "sync"),
    ("$znode.fs.statSync", "sync"),
    ("$znode.fs.lstatSync", "sync"),
    ("$znode.fs.existsSync", "sync"),
    ("$znode.fs.accessSync", "sync"),
    ("$znode.fs.mkdirSync", "sync"),
    ("$znode.fs.unlinkSync", "sync"),
    ("$znode.fs.readdirSync", "sync"),
    ("$znode.fs.stat", "async"),
    ("$znode.fs.lstat", "async"),
    ("$znode.fs.mkdir", "async"),
    ("$znode.fs.readdir", "async"),
    ("$znode.fs.unlink", "async"),
    ("$znode.fs.access", "async"),
    ("$znode.fs.rmdir", "async"),
    ("$znode.fs.link", "async"),
    ("$znode.fs.readFile", "async"),
    ("$znode.fs.writeFile", "async"),
    ("$znode.fs.exists", "async"),
    ("$znode.fsPromise.stat", "async"),
    ("$znode.fsPromise.lstat", "async"),
    ("$znode.fsPromise.mkdir", "async"),
    ("$znode.fsPromise.readdir", "async"),
    ("$znode.fsPromise.unlink", "async"),
    ("$znode.fsPromise.link", "async"),
    ("$znode.fsPromise.access", "async"),
    ("$znode.fsPromise.rmdir", "async"),
    ("$znode.fsPromise.rm", "async"),
    ("$znode.fsPromise.readFile", "async"),
    ("$znode.fsPromise.writeFile", "async"),
    ("$znode.fsExtra.copySync", "sync"),
    ("$znode.fsExtra.ensureDirSync", "sync"),
    ("$znode.fsExtra.renameSync", "sync"),
    ("$znode.fsExtra.existsSync", "sync"),
    ("$znode.fsExtra.statSync", "sync"),
    ("$znode.fsExtra.removeSync", "sync"),
    ("$znode.fsExtra.copy", "async"),
    ("$znode.fsExtra.move", "async"),
    ("$znode.fsExtra.ensureDir", "async"),
    ("$znode.fsExtra.rename", "async"),
    ("$znode.fsExtra.remove", "async"),
    ("$znode.crypto.__hashCreate", "sync"),
    ("$znode.crypto.__hashUpdate", "sync"),
    ("$znode.crypto.__hashDigest", "sync"),
    ("$znode.crypto.__cipherCreate", "sync"),
    ("$znode.crypto.__cipherUpdate", "sync"),
    ("$znode.crypto.__cipherFinal", "sync"),
    ("$znode.crypto.__cipherOption", "sync"),
    ("$znode.crypto.hash", "sync"),
    ("$znode.crypto.randomBytes", "sync"),
    ("$znode.crypto.createHash", "sync"),
    ("$znode.crypto.encrypt", "sync"),
    ("$znode.crypto.decrypt", "sync"),
    ("$znode.crypto.createCipheriv", "sync"),
    ("$znode.crypto.createDecipheriv", "sync"),
    ("$znode.os.__info", "value"),
    // Volume stats: the renderer asks `$znode.nativelibs.fileUtils().getDiskUsage("/")`
    // for the free-space banner and `$znode.diskSpace(dir)` before downloads/export.
    ("$znode.nativelibs.fileUtils.getDiskUsage", "sync"),
    ("$znode.nativelibs.fileUtilsAddon.getDiskUsage", "sync"),
    ("$znode.diskSpace", "async"),
];

/// CryptoJS preset the official `zsafe-storage` cipher uses (`bridge-spec.md` §2.1).
const SAFE_STORAGE_KEY: &str = "d9c07b9de9915af401607d3f360211ac";
const SAFE_STORAGE_IV: [u8; 16] = [155, 72, 37, 195, 9, 217, 80, 42, 145, 102, 171, 75, 214, 176, 134, 18];
const ZPROTO_KDF_INFO: &[u8] = b"ZprotocolSecureKDF";

enum CryptoState {
    Hash {
        alg: String,
        buf: Vec<u8>,
    },
    Cipher {
        encrypt: bool,
        alg: String,
        key: Vec<u8>,
        iv: Vec<u8>,
        buf: Vec<u8>,
        tag: Option<Vec<u8>>,
        aad: Vec<u8>,
    },
}

/// Electron main's `sharedWorkerManager` (macOS: `SharedWorkerRouter`): the
/// hidden `shared-worker.html` window registers the task types it can run, and
/// tasks/results travel between the main window and that window.
#[derive(Default)]
struct WorkerState {
    ready: bool,
    /// task type → window that registered it (`main` / `shared-worker`)
    handlers: HashMap<String, String>,
    /// Routed task key → the window that asked, the window that runs it, and
    /// the requester's original id. The key is prefixed with the requester
    /// because main and the shared worker both number their own tasks from 1.
    pending: HashMap<String, PendingTask>,
    last_remote_config: Option<Value>,
    viewer_key: Option<Value>,
}

#[derive(Clone)]
struct PendingTask {
    requester: String,
    handler: String,
    request_id: Value,
    task_type: String,
    /// When the task was handed to the handler, so a reply can say how long it took.
    started: std::time::Instant,
}

pub struct Bridge {
    userdata: PathBuf,
    client_version: Mutex<String>,
    /// The installed Zalo runtime's app version, used in page queries and
    /// `getAppVersion` — it follows the runtime, not this wrapper's version.
    app_version: Mutex<String>,
    debug: bool,
    pub events: Arc<Events>,
    store: Mutex<HashMap<String, Value>>,
    crypto: Mutex<HashMap<u64, CryptoState>>,
    handles: AtomicU64,
    session: Mutex<Option<Value>>,
    jar: Arc<reqwest::cookie::Jar>,
    /// Every cookie the renderer set, so the session survives a restart —
    /// Electron kept them in Chromium's profile; here they are ours to keep.
    cookie_log: Mutex<Vec<(String, String)>>,
    cookie_file: PathBuf,
    base_url: Mutex<String>,
    unhandled: Mutex<HashMap<String, u64>>,
    secrets: Mutex<HashMap<String, Vec<u8>>>,
    worker: Mutex<WorkerState>,
    /// The Tauri app handle, set once the windows exist — window-control IPC
    /// channels (`minimize`, `toggle-maximize`, …) and notification clicks
    /// drive the real window through it.
    app: Mutex<Option<tauri::AppHandle>>,
    noti_seq: AtomicU64,
    /// zcall-v2: cầu nối với trình gọi native (helper) — xem `call.rs`.
    call: crate::call::CallHandle,
}

impl Bridge {
    pub fn new(userdata: PathBuf, client_version: String, debug: bool, events: Arc<Events>) -> Self {
        let cookie_file = userdata.join("cookies.json");
        let encrypted_file = userdata.join("cookies.enc");
        let cookie_log: Vec<(String, String)> = crate::session::load_cookie_log(&cookie_file, &encrypted_file);
        let jar = reqwest::cookie::Jar::default();
        for (url, cookie) in &cookie_log {
            if let Ok(parsed) = reqwest::Url::parse(url) {
                jar.add_cookie_str(cookie, &parsed);
            }
        }
        let call = crate::call::start(events.clone(), debug);
        Self {
            userdata,
            client_version: Mutex::new(client_version),
            app_version: Mutex::new(crate::runtime::PINNED_VERSION.to_string()),
            debug,
            events,
            store: Mutex::new(HashMap::new()),
            crypto: Mutex::new(HashMap::new()),
            handles: AtomicU64::new(1),
            session: Mutex::new(None),
            jar: Arc::new(jar),
            cookie_log: Mutex::new(cookie_log),
            cookie_file,
            base_url: Mutex::new(String::new()),
            unhandled: Mutex::new(HashMap::new()),
            secrets: Mutex::new(HashMap::new()),
            worker: Mutex::new(WorkerState::default()),
            app: Mutex::new(None),
            noti_seq: AtomicU64::new(1),
            call,
        }
    }

    /// Called once the Tauri windows exist; window-control IPC and
    /// notification clicks act through this handle.
    pub fn set_app_handle(&self, app: tauri::AppHandle) {
        *self.app.lock().unwrap() = Some(app);
    }

    /// Bản sao handle ứng dụng (focus cửa sổ khi click notification gọi đến).
    pub fn app_handle(&self) -> Option<tauri::AppHandle> {
        self.app.lock().unwrap().clone()
    }

    fn main_window(&self) -> Option<tauri::WebviewWindow> {
        use tauri::Manager;
        self.app
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|app| app.get_webview_window("main"))
    }

    /// Cửa sổ nhận lệnh window-control: popup viewer gửi kèm nhãn riêng
    /// (`__OLAZ_LINUX_WINDOW` → header `x-olaz-window`); mặc định là main.
    fn target_window(&self, asking: &str) -> Option<tauri::WebviewWindow> {
        use tauri::Manager;
        let app = self.app.lock().unwrap().clone()?;
        if asking.is_empty() || asking == "main" {
            app.get_webview_window("main")
        } else {
            app.get_webview_window(asking).or_else(|| app.get_webview_window("main"))
        }
    }

    /// Chạy closure trên GTK main thread và đợi kết quả. Clipboard (như mọi GDK
    /// call) phải chạy ở đó; các member bridge đến từ HTTP thread của tiny_http.
    fn on_main<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let app = self.app.lock().unwrap().clone()?;
        let (tx, rx) = std::sync::mpsc::channel();
        app.run_on_main_thread(move || {
            let _ = tx.send(f());
        })
        .ok()?;
        rx.recv().ok()
    }

    /// GTK clipboard (X11/Wayland) — thay `NSPasteboard` của bản macOS
    /// (`BridgeImplementation.swift:65`).
    fn clipboard_read_text(&self) -> String {
        self.on_main(|| {
            gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD)
                .wait_for_text()
                .map(|text| text.to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    fn clipboard_write_text(&self, text: &str) {
        self.on_main({
            let text = text.to_string();
            move || {
                gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_text(&text);
            }
        });
    }

    /// Ảnh vào clipboard qua gdk-pixbuf (PNG/JPEG tự nhận bởi loader).
    fn clipboard_write_image(&self, bytes: &[u8]) -> bool {
        let bytes = bytes.to_vec();
        self.on_main(move || {
            use gtk::prelude::PixbufLoaderExt;
            let loader = gtk::gdk_pixbuf::PixbufLoader::new();
            if loader.write(&bytes).is_err() {
                return false;
            }
            let _ = loader.close();
            match loader.pixbuf() {
                Some(pixbuf) => {
                    gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_image(&pixbuf);
                    true
                }
                None => false,
            }
        })
        .unwrap_or(false)
    }

    pub fn set_base_url(&self, base: String) {
        *self.base_url.lock().unwrap() = base;
    }

    pub fn base_url(&self) -> String {
        self.base_url.lock().unwrap().clone()
    }

    /// `ZaloPC-darwin-23v<clientVersion>` — the renderer derives its device id from
    /// `md5(navigator.userAgent)`, so this must not say Linux.
    pub fn client_version_ua(&self) -> String {
        format!("ZaloPC-darwin-23v{}", self.client_version.lock().unwrap())
    }

    /// A renderer page URL with the query main normally appends (see §2.4 of
    /// `reanalysis/bridge-spec.md`). `login.html` opened without it makes the
    /// renderer read `zpw_type`/`zpw_ver` from an empty localStorage, so `reqqr`
    /// goes out as `type=null&client_version=null` and the server issues a code
    /// the phone rejects as invalid.
    pub fn page_url(&self, page: &str) -> String {
        let client = self.client_version.lock().unwrap().clone();
        let app = self.app_version.lock().unwrap().clone();
        format!(
            "{page}?type=23&client_version={client}&app_version={app}&about_version=&new_install=0&fail_sqlite3=true"
        )
    }

    /// A runtime installed while running can carry a newer `clientVersion`.
    pub fn set_client_version(&self, version: Option<String>) {
        if let Some(version) = version {
            *self.client_version.lock().unwrap() = version;
        }
    }

    /// Keeps the page query and `getAppVersion` in step with the runtime that is
    /// actually being served (a promoted update changes this between launches).
    pub fn set_app_version(&self, version: &str) {
        if !version.is_empty() {
            *self.app_version.lock().unwrap() = version.to_string();
        }
    }

    fn app_version(&self) -> String {
        self.app_version.lock().unwrap().clone()
    }

    // ---------------------------------------------------------------- cookies

    pub fn store_cookie(&self, url: &str, set_cookie: &str) {
        if let Ok(parsed) = reqwest::Url::parse(url) {
            self.jar.add_cookie_str(set_cookie, &parsed);
            self.remember_cookie(url, set_cookie);
        }
    }

    /// Seals the cookie into `cookies.enc` (atomic rename) so the next launch
    /// starts logged in — machine-bound AES-256-GCM, see `session.rs`. One
    /// entry per (url, name): later values replace earlier ones.
    fn remember_cookie(&self, url: &str, cookie: &str) {
        let name = cookie.split('=').next().unwrap_or_default().trim().to_string();
        if name.is_empty() {
            return;
        }
        let host = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        // The whole update (dedup + push + file write) holds the lock: two
        // concurrent Set-Cookie replies must land in file order, never as a
        // stale snapshot overwriting a newer one.
        let mut log = self.cookie_log.lock().unwrap();
        log.retain(|(saved_url, saved)| {
            let saved_host = reqwest::Url::parse(saved_url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default();
            let saved_name = saved.split('=').next().unwrap_or_default().trim();
            !(saved_host == host && saved_name == name)
        });
        log.push((url.to_string(), cookie.to_string()));
        crate::session::save_cookie_log(&self.cookie_file.with_extension("enc"), &self.cookie_file, &log);
    }

    /// True once `zpw_sek` was set by a completed QR login — the same test main
    /// makes on `cookies.get({url: authDomain})` to choose `index.html` over `login.html`.
    pub fn has_session(&self) -> bool {
        self.cookie_log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, cookie)| cookie.starts_with("zpw_sek="))
    }

    pub fn cookie_header(&self, url: &str) -> String {
        let Ok(parsed) = reqwest::Url::parse(url) else { return String::new() };
        if let Some(value) = self.jar.cookies(&parsed) {
            if let Ok(text) = value.to_str() {
                if !text.is_empty() {
                    return text.to_string();
                }
            }
        }
        // Cookies the renderer sets explicitly are recorded per host (`zalo.me`,
        // `chat.zalo.me`), and reqwest stores them host-only. The realtime hosts
        // (`ws1-msg.chat.zalo.me`) then get no cookie, the upgrade is anonymous,
        // and the server answers with a plain `HTTP/1.1 200 OK` body — which
        // tungstenite reports as "Reserved bits are non-zero" and the renderer
        // keeps reconnecting. Match the recorded cookies by domain suffix.
        let Some(host) = parsed.host_str().map(|h| h.to_ascii_lowercase()) else {
            return String::new();
        };
        let mut chosen: Vec<(String, String, usize)> = Vec::new();
        for (saved_url, cookie) in self.cookie_log.lock().unwrap().iter() {
            let pair = cookie.split(';').next().unwrap_or_default();
            let Some((name, value)) = pair.split_once('=') else { continue };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let domain = cookie
                .split(';')
                .skip(1)
                .find_map(|attr| {
                    let (key, value) = attr.trim().split_once('=')?;
                    key.trim().eq_ignore_ascii_case("domain").then(|| {
                        value.trim().trim_start_matches('.').to_ascii_lowercase()
                    })
                })
                .or_else(|| {
                    reqwest::Url::parse(saved_url)
                        .ok()
                        .and_then(|url| url.host_str().map(|h| h.to_ascii_lowercase()))
                })
                .unwrap_or_default();
            if domain.is_empty() || (host != domain && !host.ends_with(&format!(".{domain}"))) {
                continue;
            }
            // Same name from a parent and a child domain: keep the most specific,
            // and for the same domain the last value recorded.
            match chosen.iter_mut().find(|(saved, _, _)| saved == name) {
                Some(existing) if domain.len() >= existing.2 => {
                    *existing = (name.to_string(), value.trim().to_string(), domain.len());
                }
                Some(_) => {}
                None => chosen.push((name.to_string(), value.trim().to_string(), domain.len())),
            }
        }
        chosen
            .into_iter()
            .map(|(name, value, _)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn set_cookie_for(&self, name: &str, value: &str, domain: &str, days: u64) {
        // Max-Age rather than Expires: the cookie parser wants an HTTP date for
        // Expires, and a malformed one makes it drop the whole cookie.
        let max_age = days * 86_400;
        let header = format!("{name}={value}; Domain={domain}; Path=/; Max-Age={max_age}; Secure; SameSite=None");
        let root = domain.trim_start_matches("chat.");
        for url in [format!("https://{domain}/"), format!("https://{root}/")] {
            if let Ok(parsed) = reqwest::Url::parse(&url) {
                self.jar.add_cookie_str(&header, &parsed);
            }
        }
        self.remember_cookie(&format!("https://{domain}/"), &header);
    }

    // ---------------------------------------------------------------- spec

    /// `window.__zaloBridgeSpec`, injected before `bridge.js`.
    pub fn spec_script(&self) -> String {
        let table: Value = serde_json::from_str(BRIDGE_TABLE).unwrap_or(Value::Null);
        let ipc = table.get("table").cloned().unwrap_or_else(|| json!({}));

        let mut globals: Vec<String> = GLOBALS.iter().map(|g| g.to_string()).collect();
        if let Some(map) = ipc.as_object() {
            for path in map.keys() {
                if let Some(global) = path.split('.').next() {
                    if !globals.iter().any(|g| g == global) {
                        globals.push(global.to_string());
                    }
                }
            }
        }
        globals.sort();

        let mut members = Map::new();
        for (path, mode) in MEMBERS {
            members.insert((*path).to_string(), Value::String((*mode).to_string()));
        }
        let mut values = Map::new();
        values.insert("$znode.os.__info".into(), os_info());

        let spec = json!({
            "globals": globals,
            "members": members,
            "values": values,
            "ipc": ipc,
        });
        format!("window.__zaloBridgeSpec = {spec};")
    }

    // ---------------------------------------------------------------- entry points

    /// Blocking calls: `{path, args}` or `{channel, args}` from `bridge.js`.
    pub fn call_sync_request(&self, request: &Value) -> Result<Value, String> {
        let args = request.get("args").and_then(|a| a.as_array()).cloned().unwrap_or_default();
        if let Some(channel) = request.get("channel").and_then(|c| c.as_str()) {
            self.channel(channel, &args, true, "main")
        } else if let Some(path) = request.get("path").and_then(|p| p.as_str()) {
            self.member(path, &args, true)
        } else {
            Err("bad sync request".into())
        }
    }

    /// `zbridge` message handler: `{kind, path|channel, args}`.
    pub fn dispatch(&self, message: &Value, window: &str) -> Result<Value, String> {
        let kind = message.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let args = message.get("args").and_then(|a| a.as_array()).cloned().unwrap_or_default();
        if kind == "subscribe" || kind == "provide" {
            return Ok(undefined());
        }
        if let Some(channel) = message.get("channel").and_then(|c| c.as_str()) {
            return self.channel(channel, &args, false, window);
        }
        if let Some(path) = message.get("path").and_then(|p| p.as_str()) {
            return self.member(path, &args, false);
        }
        Err("bad bridge message".into())
    }

    /// Path-keyed members (the ones `local-modules.js` does not implement itself).
    fn member(&self, path: &str, args: &[Value], sync: bool) -> Result<Value, String> {
        trace_call(path, args);
        match path {
            "$zelectron.storeRead" => Ok(self.store_read(&str_arg(args, 0).unwrap_or_default())),
            "$zelectron.storeWrite" => {
                self.store_write(&str_arg(args, 0).unwrap_or_default(), args.get(1).cloned().unwrap_or(json!({})));
                Ok(undefined())
            }
            "$zelectron.storePath" => Ok(Value::String(self.store_path(&str_arg(args, 0).unwrap_or_default()))),
            "$zelectron.getPathSync" | "$zelectron.getPath" => {
                Ok(Value::String(self.path_named(&str_arg(args, 0).unwrap_or_default())))
            }
            "$zelectron.getAppPathSync" => Ok(Value::String(self.userdata.join("app").to_string_lossy().into_owned())),
            "$zelectron.getInfoScreenOfMouseSync" => Ok(json!({ "width": 1920, "height": 1080, "x": 0, "y": 0 })),
            "$zelectron.getAppVersion" => Ok(Value::String(self.app_version())),
            "$zelectron.setAppCookie" => {
                let value = str_arg(args, 0).unwrap_or_default();
                for domain in ["zaloapp.com", "zalo.me", "chat.zalo.me"] {
                    self.set_cookie_for("zpw_sek", &value, domain, 7);
                }
                Ok(undefined())
            }
            "$zelectron.setCookie" => {
                let name = str_arg(args, 0).unwrap_or_default();
                let value = str_arg(args, 1).unwrap_or_default();
                let domain = str_arg(args, 3).unwrap_or_else(|| "zalo.me".into());
                self.set_cookie_for(&name, &value, &domain, 365);
                Ok(undefined())
            }
            "$zelectron.clearCookie" => Ok(undefined()),
            // Clipboard thật (GTK) thay vì trả rỗng — copy text/ảnh trong chat
            // của renderer đi qua `$zelectronNative.*` (Electron `clipboard`).
            "$zelectronNative.clipboardText" => Ok(Value::String(self.clipboard_read_text())),
            "$zelectronNative.clipboardWriteText" => {
                self.clipboard_write_text(&str_arg(args, 0).unwrap_or_default());
                Ok(undefined())
            }
            "$zelectronNative.clipboardFormats" => Ok(json!(if self.clipboard_read_text().is_empty() {
                Vec::<String>::new()
            } else {
                vec!["text/plain".to_string()]
            })),
            // args = [mime, base64] — ảnh canvas đã nén từ `olaz-linux.js`.
            "$zelectronNative.clipboardWriteImage" => {
                let data = str_arg(args, 1).unwrap_or_default();
                let decoded = base64::engine::general_purpose::STANDARD.decode(data.as_bytes());
                Ok(json!(match decoded {
                    Ok(bytes) => self.clipboard_write_image(&bytes),
                    Err(_) => false,
                }))
            }
            "$zFileManager.getSentryConfig" => Ok(Value::Null),
            // Main answered the write with nothing; `null` would look like a value.
            "$zFileManager.saveSentryConfig" => Ok(undefined()),
            "$zFileManager.getViewerkey" => {
                let dir = str_arg(args, 0).map(|path| self.abs(&path)).unwrap_or_else(|| self.userdata.clone());
                std::fs::read(dir.join("z_u.txt"))
                    .map(|bytes| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
                    .map_err(|_| "ENOENT: z_u.txt not found".to_string())
            }
            "$zFileManager.writeRecentId" => {
                let uid = str_arg(args, 0).unwrap_or_default();
                std::fs::write(self.userdata.join("z_u.txt"), uid)
                    .map(|_| undefined())
                    .map_err(|error| io_error(&error, "z_u.txt"))
            }
            "$zFileManager.writeZLog" | "$zFileManager.writeZLogBetween" => {
                let name = str_arg(args, 0).unwrap_or_else(|| "z.log".into());
                let append = path.ends_with("Between")
                    || args
                        .get(2)
                        .and_then(|options| options.get("overwrite"))
                        .and_then(|flag| flag.as_bool())
                        .map(|overwrite| !overwrite)
                        .unwrap_or(true);
                self.write_log(&name, &arg_bytes(args, 1).unwrap_or_default(), append)
            }
            "$zFileManager.readZLog" => {
                let name = str_arg(args, 0).unwrap_or_default();
                Ok(match self.log_path(&name) {
                    Some(path) => match std::fs::read(path) {
                        Ok(bytes) => Value::String(String::from_utf8_lossy(&bytes).into_owned()),
                        Err(_) => undefined(),
                    },
                    None => undefined(),
                })
            }
            "$zFileManager.scandirLog" => {
                let dir = self.userdata.join("cal");
                let mut names: Vec<Value> = std::fs::read_dir(dir)
                    .map(|entries| {
                        entries
                            .filter_map(|entry| entry.ok())
                            .map(|entry| Value::String(entry.file_name().to_string_lossy().into_owned()))
                            .collect()
                    })
                    .unwrap_or_default();
                names.sort_by(|a, b| a.as_str().unwrap_or("").cmp(b.as_str().unwrap_or("")));
                Ok(Value::Array(names))
            }
            "$zFileManager.getExtraLogFiles" => Ok(json!([])),
            "$zFileManager.getQosLog" | "$zFileManager.getActionLog" | "$zFileManager.readUptimeLog" => {
                Ok(Value::String(String::new()))
            }
            "$zFileManager.removeQosLog"
            | "$zFileManager.removeActionLog"
            | "$zFileManager.writeActionLog"
            | "$zFileManager.writeUptimeLog"
            | "$zFileManager.truncateFileUpto"
            | "$zFileManager.cleanupLogFileDescriptors"
            | "$zFileManager.initBinaryLogFile"
            | "$zFileManager.sealCalfLog"
            | "$zFileManager.mergeCalfLogGroup"
            | "$zFileManager.writeLogRange"
            | "$zFileManager.trustBlob" => Ok(undefined()),
            "$zFileManager.getInstallSource" => Err("ENOENT: install source".into()),
            "$zFileManager.getFileArrayBuffer" => {
                let target = self.abs(&str_arg(args, 0).unwrap_or_default());
                std::fs::read(&target)
                    .map(|bytes| bytes_out(bytes, true))
                    .map_err(|error| io_error(&error, &target.display().to_string()))
            }
            "$zFileManager.readFileAsync" => {
                let target = self.abs(&str_arg(args, 0).unwrap_or_default());
                let encoding = str_arg(args, 1);
                std::fs::read(&target)
                    .map(|bytes| match encoding {
                        Some(_) => Value::String(String::from_utf8_lossy(&bytes).into_owned()),
                        None => bytes_out(bytes, false),
                    })
                    .map_err(|error| io_error(&error, &target.display().to_string()))
            }
            "$zFileManager.svcStatAsync" => {
                let target = self.abs(&str_arg(args, 0).unwrap_or_default());
                std::fs::metadata(&target)
                    .map(|meta| json!({ "size": meta.len() }))
                    .map_err(|error| io_error(&error, &target.display().to_string()))
            }
            "$zFileManager.svcReadPath" => {
                let target = self.abs(&str_arg(args, 0).unwrap_or_default());
                let options = args.get(1).cloned().unwrap_or_else(|| json!({}));
                let start = options.get("start").and_then(|value| value.as_u64()).unwrap_or(0);
                let end = options.get("end").and_then(|value| value.as_u64());
                ranged_read(&target, start, end).map(|bytes| bytes_out(bytes, false))
            }
            "$zFileManager.writeBufferToPath" => {
                let target = self.abs(&str_arg(args, 0).unwrap_or_default());
                let data = arg_bytes(args, 1).unwrap_or_default();
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&target, data)
                    .map(|_| Value::Bool(true))
                    .map_err(|error| io_error(&error, &target.display().to_string()))
            }
            "$zFileManager.getMd5LargeFileObject" => {
                let target = self.abs(&str_arg(args, 0).unwrap_or_default());
                file_md5(&target)
                    .map(Value::String)
                    .map_err(|error| io_error(&error, &target.display().to_string()))
            }
            "$zwindow.pickFolder" => {
                let title = str_arg(args, 0).unwrap_or_default();
                let start = str_arg(args, 1).unwrap_or_default();
                Ok(match pick_folder(&title, &start)? {
                    Some(path) => Value::String(path),
                    None => undefined(),
                })
            }
            "$znode.crypto.hash" => {
                let data = arg_bytes(args, 0).unwrap_or_default();
                let alg = str_arg(args, 1).unwrap_or_else(|| "sha256".into());
                let encoded = str_arg(args, 3);
                Ok(encode_digest(&hash_digest(&alg, &data)?, encoded.as_deref()))
            }
            // Renderer-side JPEG XL decode: Zalo's photo component calls
            // `$zFeatures.libjxl.decodeToJpeg`, which is a native addon that
            // ships only for macOS/Windows. Linux answers with `jxl-oxide` so a
            // message photo can reach an `<img>` instead of being dropped.
            "$olaz.jxl.decode" => {
                let data = arg_bytes(args, 0).ok_or_else(|| "jxl: thiếu dữ liệu".to_string())?;
                let quality = int_arg(args, 1).unwrap_or(90).clamp(1, 100) as u8;
                // Already a JPEG (the download manager rewrites photo files): hand it back
                // untouched so both photo paths keep working.
                let jpeg = if crate::download::is_jxl(&data) {
                    crate::download::jxl_to_jpeg(&data, quality)
                        .ok_or_else(|| "jxl: không giải mã được".to_string())?
                } else {
                    data
                };
                Ok(bytes_out(jpeg, true))
            }
            "$olaz.jxl.info" => {
                let data = arg_bytes(args, 0).ok_or_else(|| "jxl: thiếu dữ liệu".to_string())?;
                crate::download::jxl_size(&data)
                    .map(|(width, height)| json!({ "width": width, "height": height }))
                    .ok_or_else(|| "jxl: không đọc được kích thước".to_string())
            }
            // The photo bubble's `<img>` only loads a cached file; its bytes otherwise
            // come from the official multi-source downloader, whose module chain fails
            // on Linux (`resolveMedia` → reason 'error'). Fetching the CDN copy here is
            // the fallback that turns a blurred LQIP into the real photo.
            "$olaz.photo.cache" => self.cache_photo(&args.first().cloned().unwrap_or(Value::Null)),
            "$znode.crypto.randomBytes" => {
                let count = int_arg(args, 0).unwrap_or(0).max(0) as usize;
                let mut bytes = vec![0u8; count];
                rand::thread_rng().fill_bytes(&mut bytes);
                Ok(bytes_out(bytes, true))
            }
            "$znode.crypto.encrypt" | "$znode.crypto.decrypt" => {
                let encrypt = path.ends_with("encrypt");
                // Preload: `encrypt(input, alg, key, iv = Buffer.alloc(16, 0), opts)`.
                // The renderer passes `null` for that default; treating it as an
                // empty IV made every zcid/login-parameter encryption fail with
                // "AES-CBC cần IV 16 byte (nhận 0)".
                let options = args.get(4).and_then(|value| value.as_object());
                let opt = |name: &str| options.and_then(|map| map.get(name)).and_then(|value| value.as_str());
                let input = args.first().map(|value| decode_with(value, opt("input"))).unwrap_or_default();
                let alg = str_arg(args, 1).unwrap_or_else(|| "aes-256-cbc".into());
                let key = args.get(2).map(|value| decode_with(value, opt("key"))).unwrap_or_default();
                let iv = match args.get(3) {
                    None | Some(Value::Null) => vec![0u8; 16],
                    Some(value) => decode_with(value, opt("iv").or_else(|| opt("key"))),
                };
                let (out, _) = cipher_run(&alg, encrypt, &key, &iv, &input, None, &[])?;
                Ok(match opt("output") {
                    Some(encoding) => encode_with(&out, Some(encoding)),
                    None => bytes_out(out, false),
                })
            }
            "$znode.crypto.createHash" => self.crypto_hash_create(&str_arg(args, 0).unwrap_or_default()),
            "$znode.crypto.createCipheriv" | "$znode.crypto.createDecipheriv" => self.crypto_cipher_create(
                path.ends_with("createCipheriv"),
                &str_arg(args, 0).unwrap_or_default(),
                &arg_bytes(args, 1).unwrap_or_default(),
                &arg_bytes(args, 2).unwrap_or_default(),
            ),
            "$znode.crypto.__hashCreate" => self.crypto_hash_create(&str_arg(args, 0).unwrap_or_default()),
            "$znode.crypto.__hashUpdate" => {
                let handle = int_arg(args, 0).unwrap_or(0) as u64;
                let data = arg_bytes(args, 1).unwrap_or_default();
                let mut crypto = self.crypto.lock().unwrap();
                match crypto.get_mut(&handle) {
                    Some(CryptoState::Hash { buf, .. }) => {
                        buf.extend_from_slice(&data);
                        Ok(undefined())
                    }
                    _ => Err("hash handle không tồn tại".into()),
                }
            }
            "$znode.crypto.__hashDigest" => {
                let handle = int_arg(args, 0).unwrap_or(0) as u64;
                let encoded = str_arg(args, 1);
                let state = self.crypto.lock().unwrap().remove(&handle);
                match state {
                    Some(CryptoState::Hash { alg, buf }) => Ok(encode_digest(&hash_digest(&alg, &buf)?, encoded.as_deref())),
                    _ => Err("hash handle không tồn tại".into()),
                }
            }
            "$znode.crypto.__cipherCreate" => self.crypto_cipher_create(
                str_arg(args, 0).as_deref() != Some("decipher"),
                &str_arg(args, 1).unwrap_or_default(),
                &arg_bytes(args, 2).unwrap_or_default(),
                &arg_bytes(args, 3).unwrap_or_default(),
            ),
            "$znode.crypto.__cipherUpdate" => {
                let handle = int_arg(args, 0).unwrap_or(0) as u64;
                let data = arg_bytes(args, 1).unwrap_or_default();
                let mut crypto = self.crypto.lock().unwrap();
                match crypto.get_mut(&handle) {
                    Some(CryptoState::Cipher { buf, .. }) => {
                        buf.extend_from_slice(&data);
                        Ok(bytes_out(Vec::new(), false))
                    }
                    _ => Err("cipher handle không tồn tại".into()),
                }
            }
            "$znode.crypto.__cipherFinal" => {
                let handle = int_arg(args, 0).unwrap_or(0) as u64;
                let encoded = str_arg(args, 1);
                let state = self.crypto.lock().unwrap().remove(&handle);
                match state {
                    Some(CryptoState::Cipher { encrypt, alg, key, iv, buf, tag, aad }) => {
                        let (out, _) = cipher_run(&alg, encrypt, &key, &iv, &buf, tag.as_deref(), &aad)?;
                        Ok(match encoded.as_deref() {
                            Some("hex") => Value::String(hex_encode(&out)),
                            Some("base64") => Value::String(base64::engine::general_purpose::STANDARD.encode(&out)),
                            _ => bytes_out(out, false),
                        })
                    }
                    _ => Err("cipher handle không tồn tại".into()),
                }
            }
            "$znode.crypto.__cipherOption" => {
                let handle = int_arg(args, 0).unwrap_or(0) as u64;
                let option = str_arg(args, 1).unwrap_or_default();
                let mut crypto = self.crypto.lock().unwrap();
                let Some(CryptoState::Cipher { tag, aad, .. }) = crypto.get_mut(&handle) else {
                    return Err("cipher handle không tồn tại".into());
                };
                match option.as_str() {
                    "getAuthTag" => Ok(tag.clone().map(|t| bytes_out(t, true)).unwrap_or_else(undefined)),
                    "authTag" => {
                        *tag = Some(arg_bytes(args, 2).unwrap_or_default());
                        Ok(undefined())
                    }
                    "aad" => {
                        *aad = arg_bytes(args, 2).unwrap_or_default();
                        Ok(undefined())
                    }
                    _ => Ok(undefined()),
                }
            }
            "$znode.nativelibs.fileUtils.getDiskUsage"
            | "$znode.nativelibs.fileUtilsAddon.getDiskUsage"
            | "$znode.diskSpace" => disk_usage(&str_arg(args, 0).unwrap_or_default()),
            "$znode.fs.exists" => Ok(Value::Bool(self.abs(&str_arg(args, 0).unwrap_or_default()).exists())),
            other if other.starts_with("$znode.fs.") || other.starts_with("$znode.fsPromise.") || other.starts_with("$znode.fsExtra.") => {
                self.fs_call(other, args)
            }
            other => {
                let mut unhandled = self.unhandled.lock().unwrap();
                let count = unhandled.entry(other.to_string()).or_insert(0);
                *count += 1;
                let first = *count == 1;
                drop(unhandled);
                if self.debug && first {
                    eprintln!("[bridge] member chưa cài: {other} (sync={sync})");
                }
                Ok(undefined())
            }
        }
    }

    fn crypto_hash_create(&self, alg: &str) -> Result<Value, String> {
        let handle = self.handles.fetch_add(1, Ordering::Relaxed);
        self.crypto
            .lock()
            .unwrap()
            .insert(handle, CryptoState::Hash { alg: alg.to_string(), buf: Vec::new() });
        Ok(json!(handle))
    }

    fn crypto_cipher_create(&self, encrypt: bool, alg: &str, key: &[u8], iv: &[u8]) -> Result<Value, String> {
        let handle = self.handles.fetch_add(1, Ordering::Relaxed);
        self.crypto.lock().unwrap().insert(
            handle,
            CryptoState::Cipher {
                encrypt,
                alg: alg.to_string(),
                key: key.to_vec(),
                iv: iv.to_vec(),
                buf: Vec::new(),
                tag: None,
                aad: Vec::new(),
            },
        );
        Ok(json!(handle))
    }

    // ---------------------------------------------------------------- filesystem

    fn abs(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.userdata.join(path)
        }
    }

    fn fs_call(&self, path: &str, args: &[Value]) -> Result<Value, String> {
        let op = path.rsplit('.').next().unwrap_or("");
        let target = self.abs(&str_arg(args, 0).unwrap_or_default());
        let display = target.display().to_string();
        let not_found = || format!("ENOENT: no such file or directory, {op} '{display}'");

        match op {
            "statSync" | "stat" | "lstatSync" | "lstat" => {
                let link = op.starts_with("lstat");
                let metadata = if link { std::fs::symlink_metadata(&target) } else { std::fs::metadata(&target) };
                match metadata {
                    Ok(meta) => Ok(stats_value(&meta)),
                    Err(err) => Err(io_error(&err, &display)),
                }
            }
            "existsSync" | "accessSync" | "access" => {
                if target.exists() {
                    Ok(Value::Bool(true))
                } else if op == "accessSync" || op == "access" {
                    Err(not_found())
                } else {
                    Ok(Value::Bool(false))
                }
            }
            "readFileSync" | "readFile" => std::fs::read(&target)
                .map(|bytes| bytes_out(bytes, false))
                .map_err(|err| io_error(&err, &display)),
            "writeFileSync" | "writeFile" => {
                let data = arg_bytes(args, 1).unwrap_or_default();
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&target, data).map(|_| undefined()).map_err(|err| io_error(&err, &display))
            }
            "appendFileSync" => {
                use std::io::Write;
                let data = arg_bytes(args, 1).unwrap_or_default();
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&target)
                    .and_then(|mut file| file.write_all(&data))
                    .map(|_| undefined())
                    .map_err(|err| io_error(&err, &display))
            }
            "mkdirSync" | "mkdir" => {
                let recursive = args.get(1).map(|v| v.get("recursive").and_then(|r| r.as_bool()).unwrap_or(false)).unwrap_or(false);
                let result = if recursive { std::fs::create_dir_all(&target) } else { std::fs::create_dir(&target) };
                result.map(|_| undefined()).map_err(|err| io_error(&err, &display))
            }
            "ensureDirSync" | "ensureDir" => {
                if target.is_dir() {
                    Ok(undefined())
                } else {
                    std::fs::create_dir_all(&target).map(|_| undefined()).map_err(|err| io_error(&err, &display))
                }
            }
            "readdirSync" | "readdir" => std::fs::read_dir(&target)
                .map(|entries| {
                    let mut names: Vec<Value> = entries
                        .filter_map(|entry| entry.ok())
                        .map(|entry| Value::String(entry.file_name().to_string_lossy().into_owned()))
                        .collect();
                    names.sort_by(|a, b| a.as_str().unwrap_or("").cmp(b.as_str().unwrap_or("")));
                    Value::Array(names)
                })
                .map_err(|err| io_error(&err, &display)),
            "unlinkSync" | "unlink" | "removeSync" | "remove" | "rm" | "rmdir" => {
                let result = if target.is_dir() {
                    if op == "rmdir" { std::fs::remove_dir(&target) } else { std::fs::remove_dir_all(&target) }
                } else if target.exists() {
                    std::fs::remove_file(&target)
                } else {
                    Ok(())
                };
                result.map(|_| undefined()).map_err(|err| io_error(&err, &display))
            }
            "copySync" | "copy" | "move" => {
                let destination = self.abs(&str_arg(args, 1).unwrap_or_default());
                let result = copy_tree(&target, &destination).and_then(|()| {
                    if op == "move" {
                        std::fs::remove_dir_all(&target).or_else(|_| std::fs::remove_file(&target))
                    } else {
                        Ok(())
                    }
                });
                result.map(|_| undefined()).map_err(|err| io_error(&err, &display))
            }
            "renameSync" | "rename" => {
                let destination = self.abs(&str_arg(args, 1).unwrap_or_default());
                std::fs::rename(&target, &destination).map(|_| undefined()).map_err(|err| io_error(&err, &display))
            }
            "link" => {
                let destination = self.abs(&str_arg(args, 1).unwrap_or_default());
                std::fs::hard_link(&target, &destination).map(|_| undefined()).map_err(|err| io_error(&err, &display))
            }
            other => Err(format!("ENOTSUP: fs.{other} chưa được cài trên Linux")),
        }
    }

    fn path_named(&self, name: &str) -> String {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp"));
        let xdg = |key: &str, fallback: &str| {
            std::env::var(key)
                .map(PathBuf::from)
                .unwrap_or_else(|_| home.join(fallback))
        };
        let path = match name {
            "userData" | "appData" => self.userdata.clone(),
            "home" => home.clone(),
            "temp" => PathBuf::from("/tmp"),
            "exe" => std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/usr/bin/olaz-linux")),
            "desktop" => xdg("XDG_DESKTOP_DIR", "Desktop"),
            "downloads" => xdg("XDG_DOWNLOAD_DIR", "Downloads"),
            "documents" => xdg("XDG_DOCUMENTS_DIR", "Documents"),
            "pictures" => xdg("XDG_PICTURES_DIR", "Pictures"),
            "music" => xdg("XDG_MUSIC_DIR", "Music"),
            "videos" => xdg("XDG_VIDEOS_DIR", "Videos"),
            "cache" => std::env::var("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home.join(".cache")),
            "logs" => self.userdata.join("logs"),
            "db" => self.userdata.join("sqlite"),
            _ => self.userdata.clone(),
        };
        path.to_string_lossy().into_owned()
    }

    // ---------------------------------------------------------------- file manager

    /// `$zFileManager` log files live in `<userData>/cal/`, same as the macOS host.
    /// Only the basename is used: the renderer passes arbitrary names, not paths.
    fn log_path(&self, name: &str) -> Option<PathBuf> {
        let base = Path::new(name).file_name()?.to_string_lossy().into_owned();
        Some(self.userdata.join("cal").join(base))
    }

    fn write_log(&self, name: &str, data: &[u8], append: bool) -> Result<Value, String> {
        let Some(path) = self.log_path(name) else {
            return Err(format!("EINVAL: tên log không hợp lệ ({name})"));
        };
        let display = path.display().to_string();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| io_error(&error, &display))?;
        }
        let result = if append {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut file| file.write_all(data))
        } else {
            std::fs::write(&path, data)
        };
        result.map(|_| undefined()).map_err(|error| io_error(&error, &display))
    }

    /// `$zFeatures.genThumb.writeSavePathFilePhoto` — where the preload would write
    /// the generated thumbnail with `fs.createWriteStream`, which WebKitGTK has no
    /// Node stream for. The photo bubble's first cache candidate is
    /// `<ZaloDownloads>/resource/<conv>/fileThumb/<checksum>.jpg`, so that is what
    /// gets written; returning its path is what lets the bubble swap its LQIP for
    /// the real `<img>`.
    fn write_thumb(
        &self,
        user_id: &str,
        name: &str,
        bytes: &[u8],
        message: &Value,
    ) -> Result<Value, String> {
        let to_uid = match message.get("toUid") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Number(number)) => number.to_string(),
            _ => String::new(),
        };
        let (user_id, to_uid) = (path_component(user_id), path_component(&to_uid));
        let file = path_component(name);
        if user_id.is_empty() || to_uid.is_empty() || file.is_empty() {
            return Err("EINVAL: thumbnail thiếu userId/toUid/checksum".into());
        }
        // `<share>/olaz/ZaloPC/<ownId>/ZaloDownloads/resource/<conv>/…` — where the
        // renderer already keeps synced photos, so the cache check finds this too.
        let zalo_pc = bridge_zalo_pc(&self.userdata);
        let dir = zalo_pc
            .join(&user_id)
            .join("ZaloDownloads")
            .join("resource")
            .join(&to_uid)
            .join("fileThumb");
        let path = dir.join(format!("{file}.jpg"));
        let display = path.display().to_string();
        std::fs::create_dir_all(&dir).map_err(|error| io_error(&error, &display))?;
        let jpeg = if crate::download::is_jxl(bytes) {
            crate::download::jxl_to_jpeg(bytes, 88)
                .ok_or_else(|| format!("EINVAL: không giải mã được jxl ({display})"))?
        } else {
            bytes.to_vec()
        };
        std::fs::write(&path, &jpeg).map_err(|error| io_error(&error, &display))?;
        Ok(Value::String(display))
    }

    /// `$olaz.photo.cache` — download one message photo and write the four
    /// `resource/<conv>/Cache/<stem>_t|_n|_h|_o` variants the bubble probes.
    /// The official `loadPC` downloader throws inside its module chain here, so a
    /// photo that never downloaded keeps its placeholder even though its CDN URL is
    /// known. Returns `{state:'cached'}` when the files were already there.
    fn cache_photo(&self, request: &Value) -> Result<Value, String> {
        let url = request
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let stem = path_component(request.get("stem").and_then(Value::as_str).unwrap_or_default());
        let conv = path_component(request.get("convId").and_then(Value::as_str).unwrap_or_default());
        if !url.starts_with("https://") || stem.is_empty() || conv.is_empty() {
            return Err("EINVAL: photo.cache cần https url + stem + convId".into());
        }
        let user = request
            .get("userId")
            .and_then(Value::as_str)
            .map(path_component)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                self.session
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|session| session.get("userId").and_then(Value::as_str))
                    .map(path_component)
                    .filter(|value| !value.is_empty())
            })
            .ok_or_else(|| "ENOSESSION: photo.cache chưa biết userId".to_string())?;
        let zalo_pc = bridge_zalo_pc(&self.userdata);
        let resource = zalo_pc
            .join(&user)
            .join("ZaloDownloads")
            .join("resource")
            .join(&conv);
        // Each quality has its own cache path: the thumbnail is `<stem>_t<ext>` under
        // `Cache/`, every other quality folds `md5(url)` in under `picture/`. The caller
        // sends the quality→URL map so each file lands where the renderer probes.
        let qualities = request
            .get("qualities")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|entry| {
                        let pair = entry.as_array()?;
                        let quality = pair.first()?.as_str()?.to_string();
                        let value = pair.get(1)?.as_str()?.to_string();
                        (value.starts_with("https://")).then_some((quality, value))
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|list| !list.is_empty())
            .unwrap_or_else(|| vec![("thumbUrl".to_string(), url.clone())]);
        let variants =
            crate::download::photo_variant_paths(&resource, &stem, &qualities);
        let present = variants
            .iter()
            .all(|path| std::fs::metadata(path).map(|meta| meta.len() > 0).unwrap_or(false));
        if present {
            return Ok(Self::photo_cache_reply("cached", &variants));
        }
        let client = reqwest::blocking::Client::builder()
            .user_agent(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/108.0.0.0 Safari/537.36",
            )
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| format!("ENET: {error}"))?;
        let response = client
            .get(&url)
            .header("referer", "https://chat.zalo.me/")
            .send()
            .map_err(|error| format!("ENET: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("EHTTP {}: {}", status.as_u16(), url));
        }
        let bytes = response
            .bytes()
            .map_err(|error| format!("ENET: {error}"))?
            .to_vec();
        if bytes.is_empty() {
            return Err("EEMPTY: ảnh rỗng".into());
        }
        let files = crate::download::write_photo_cache(&resource, &stem, &qualities, &bytes)
            .ok_or_else(|| "EDECODE: không giải mã được ảnh".to_string())?;
        if self.debug {
            eprintln!(
                "[photo] {conv} {stem} {} B → {}",
                bytes.len(),
                files.last().map(|p| p.display().to_string()).unwrap_or_default()
            );
        }
        Ok({
            let mut reply = Self::photo_cache_reply("ok", &files);
            reply["bytes"] = Value::from(bytes.len() as u64);
            reply
        })
    }

    /// Reply shape for `$olaz.photo.cache`: `{state, path, paths}` — `path` is the
    /// full-quality variant for callers that just open one file, `paths` carries
    /// all four cache entries (`_t|_n|_h|_o`) for callers that choose.
    fn photo_cache_reply(state: &str, files: &[std::path::PathBuf]) -> Value {
        let best = files.get(3).or_else(|| files.first());
        json!({
            "state": state,
            "path": best.map(|path| path.display().to_string()).unwrap_or_default(),
            "paths": files
                .iter()
                .map(|path| Value::String(path.display().to_string()))
                .collect::<Vec<_>>(),
        })
    }

    // ---------------------------------------------------------------- electron-store

    fn store_path(&self, name: &str) -> String {
        let safe: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
            .collect();
        let safe = if safe.is_empty() { "config".to_string() } else { safe };
        self.userdata.join(format!("{safe}.json")).to_string_lossy().into_owned()
    }

    fn store_read(&self, name: &str) -> Value {
        let path = self.store_path(name);
        if let Some(cached) = self.store.lock().unwrap().get(&path) {
            return cached.clone();
        }
        let value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or_else(|| json!({}));
        self.store.lock().unwrap().insert(path, value.clone());
        value
    }

    fn store_write(&self, name: &str, value: Value) {
        let path = self.store_path(name);
        self.store.lock().unwrap().insert(path.clone(), value.clone());
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into());
        let _ = std::fs::write(path, text);
    }

    // ---------------------------------------------------------------- IPC channels

    /// `ipcRenderer.invoke/send/sendSync` — this plays Electron's main process.
    fn channel(&self, channel: &str, args: &[Value], sync: bool, window: &str) -> Result<Value, String> {
        trace_call(channel, args);
        // Which window asked is the whole point of the task router; names only,
        // never payloads (same policy as the rest of OLAZ_DEBUG logging).
        if self.debug && (channel.starts_with("db-") || channel == "new-session" || channel == "login-success") {
            eprintln!("[db] {channel} từ {window}");
        }
        if self.debug && (channel.starts_with("shared-worker") || channel == "receive-viewerkey") {
            eprintln!("[worker] {channel} từ {window} ({})", args.len());
        }
        match channel {
            // Window controls from the renderer's custom titlebar: the macOS
            // host answers these in Swift (NSWindow); here they drive the
            // Tauri window — of the window that asked (popup viewers have
            // their own titlebar controls), not always the main one.
            "minimize" => {
                if let Some(window) = self.target_window(window) {
                    let _ = window.minimize();
                }
                Ok(undefined())
            }
            "toggle-maximize" => {
                if let Some(window) = self.target_window(window) {
                    if window.is_maximized().unwrap_or(false) {
                        let _ = window.unmaximize();
                    } else {
                        let _ = window.maximize();
                    }
                }
                Ok(undefined())
            }
            "check-maximize" => Ok(json!(
                self.target_window(window)
                    .and_then(|window| window.is_maximized().ok())
                    .unwrap_or(false)
            )),
            "focus-window" => {
                if let Some(window) = self.target_window(window) {
                    let _ = window.unminimize();
                    let _ = window.show();
                    let _ = window.set_focus();
                }
                Ok(undefined())
            }
            "hide-window" => {
                if let Some(window) = self.target_window(window) {
                    let _ = window.hide();
                }
                Ok(undefined())
            }
            "close-window" => {
                if let Some(window) = self.target_window(window) {
                    let _ = window.close();
                }
                Ok(undefined())
            }

            // Link ngoài: renderer gọi window.open / openUrlInSandBox (shim
            // `olaz-linux.js` đẩy về đây) — mở bằng trình duyệt hệ thống, đúng
            // như `NSWorkspace.shared.open` trên macOS.
            "open-external" => {
                let url = str_arg(args, 0).unwrap_or_default();
                if let Some(sanitized) = external_url(&url) {
                    open_external(&sanitized);
                }
                Ok(undefined())
            }

            // "Lưu về máy" của media viewer / file download: Electron
            // `dialog.showSaveDialog` → GTK save dialog, trả `{canceled, filePath}`.
            "show-save-dialog" | "show-save-dialog-sync" => {
                let options = args
                    .iter()
                    .find(|value| value.is_object())
                    .cloned()
                    .unwrap_or(json!({}));
                if self.debug {
                    let args_text = json!(args).to_string();
                    let brief: String = args_text.chars().take(400).collect();
                    eprintln!("[save-dialog] args: {brief}");
                }
                let title = options
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Lưu về máy")
                    .to_string();
                let mut default_path = options
                    .get("defaultPath")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if default_path.is_empty() {
                    // Renderer không mang fileName/url (media item của viewer
                    // khác bên Electron) — gợi ý bằng file lần tải gần nhất.
                    if let Some(path) = crate::download::last_saved_path() {
                        default_path = path;
                    }
                }
                let picked = save_file_dialog(&title, &default_path)?;
                Ok(json!({ "canceled": picked.is_none(), "filePath": picked }))
            }

            // Desktop notifications: the notification-adapter replaces
            // `window.Notification` with a shim that ends up here. Show a
            // native notification (libnotify → GNOME), remember it by id and
            // bridge click/close back to the page's `_noti_on-*` listeners.
            "_noti_is-supported" => Ok(Value::Bool(true)),
            "_noti_create" => {
                let title = str_arg(args, 0).unwrap_or_default();
                let opts = args.get(1).cloned().unwrap_or(json!({}));
                let body = opts.get("body").and_then(Value::as_str).unwrap_or("").to_string();
                let require_interaction = opts.get("requireInteraction").and_then(Value::as_bool).unwrap_or(false);
                let click_id = format!(
                    "olaz-{}",
                    self.noti_seq.fetch_add(1, Ordering::SeqCst)
                );
                let app_handle = self.app.lock().unwrap().clone();
                let events = self.events.clone();
                let click_id_for_thread = click_id.clone();
                let click_id_for_close = click_id.clone();
                let notify_event = move |channel: &str, nid: &str| {
                    events.push(json!({
                        "kind": "send", "channel": channel, "args": vec![json!(nid)]
                    }));
                };
                // The handle is consumed by `wait_for_action` on its own
                // thread, so the page's `_noti_close` cannot reach it —
                // notifications auto-expire instead (GNOME keeps dismissed
                // ones in its tray, and the click action still works).
                let timeout = if require_interaction {
                    notify_rust::Timeout::Never
                } else {
                    notify_rust::Timeout::Milliseconds(15_000)
                };
                let handle = notify_rust::Notification::new()
                    .summary(title.as_str())
                    .body(body.as_str())
                    .icon("olaz-linux")
                    .action("default", "Mở")
                    .timeout(timeout)
                    .show();
                match handle {
                    Ok(handle) => {
                        std::thread::spawn(move || {
                            handle.wait_for_action(|action| {
                                match action {
                                    "default" => {
                                        notify_event("_noti_on-click", &click_id_for_thread);
                                        if let Some(app) = app_handle.as_ref() {
                                            use tauri::Manager;
                                            if let Some(window) = app.get_webview_window("main") {
                                                let _ = window.unminimize();
                                                let _ = window.show();
                                                let _ = window.set_focus();
                                            }
                                        }
                                    }
                                    "__closed" => notify_event("_noti_on-close", &click_id_for_close),
                                    _ => {}
                                }
                            });
                        });
                        Ok(Value::String(click_id))
                    }
                    Err(error) => {
                        if self.debug {
                            eprintln!("[noti] tạo notification lỗi: {error}");
                        }
                        Ok(Value::Null)
                    }
                }
            }
            "_noti_close" => Ok(undefined()),
            "_electron_get-path" | "_electron_get-path-sync" => {
                Ok(Value::String(self.path_named(&str_arg(args, 0).unwrap_or_default())))
            }
            "_electron_get-app-path" | "_electron_get-app-path-sync" => {
                Ok(Value::String(self.userdata.join("app").to_string_lossy().into_owned()))
            }
            "_electron_get-app-version" => Ok(Value::String(self.app_version())),
            "_electron_set-app-cookie" => {
                let value = str_arg(args, 0).unwrap_or_default();
                for domain in ["zaloapp.com", "zalo.me", "chat.zalo.me"] {
                    self.set_cookie_for("zpw_sek", &value, domain, 7);
                }
                Ok(undefined())
            }
            "_electron_set-cookie" => {
                let name = str_arg(args, 0).unwrap_or_default();
                let value = str_arg(args, 1).unwrap_or_default();
                let domain = str_arg(args, 3).unwrap_or_else(|| "zalo.me".into());
                self.set_cookie_for(&name, &value, &domain, 365);
                Ok(undefined())
            }
            "_electron_clear-cookie" => Ok(undefined()),

            // IndexedDB mode: WebKitGTK has IndexedDB; Zalo's SQLCipher DAL host
            // would have to be ported to speak its MessagePort protocol.
            // `$zFileManager` trust/dialog plumbing. The file helpers above read
            // paths directly (as the macOS host does), so this only has to say
            // yes to the renderer's pre-read check.
            "is_trust_path" => Ok(Value::Bool(true)),
            "trust_path_from_blob" | "request_flush_zlog" | "set_readonly_file" => Ok(undefined()),
            "verify_zalo_pc_hardlink_support" => Ok(Value::Bool(true)),

            // Photo thumbnails. The bubble asks for a cached thumb first; when none
            // exists it decodes the message photo in the mainless worker, checks the
            // md5, then writes a JPEG next to the conversation. macOS runs those in
            // `mainless-worker.js`; both tasks are plain image work, so they live here.
            "exec_raw" => {
                exec_raw(&str_arg(args, 0).unwrap_or_default(), args.get(1).cloned().unwrap_or(Value::Null))
            }
            "gen-thumb_write-savepath-file-photo" => {
                let bytes = arg_bytes(args, 2)
                    .ok_or_else(|| "EINVAL: thiếu buffer thumbnail".to_string())?;
                self.write_thumb(
                    &str_arg(args, 0).unwrap_or_default(),
                    &str_arg(args, 1).unwrap_or_default(),
                    &bytes,
                    &args.get(3).cloned().unwrap_or(Value::Null),
                )
            }

            "sqlite-available" => Ok(Value::Bool(false)),
            "fail-load-sqlite3" => Ok(Value::Bool(true)),
            "is-migrate-full-done" => Ok(Value::Bool(false)),
            "get-migrate-local-info" => Ok(json!({ "disable_migrate": true })),
            "update-migrate-local-info" => Ok(undefined()),
            "migratedb:check_need_wait_bu" => Ok(Value::Bool(false)),
            "migratedb:signal_restart" | "migratedb:signal_done" => Ok(undefined()),
            "z-db-corruption-detector" => Ok(json!(0)),
            "get-name-of-target-script-for-dal-channel-from-host" => Ok(Value::String("preload-sqlite".into())),

            // Media downloads. The official loader (`CloudService.downloadWithMultiSrc`: the
            // photo `loadPC`, video, files, and the Zalo Cloud fallback behind them) calls
            // `$zdownload.sendDownloadRequest` and waits for a `new-download-file-resp` push on
            // this same channel. Electron main answered it; with no answer every image stays
            // unresolved and ZaloDownloads never receives a file.
            "new-download-channel" => {
                if let Some(request) = args.first() {
                    crate::download::request(
                        self.events.clone(),
                        self.jar.clone(),
                        self.client_version_ua(),
                        self.debug,
                        request,
                    );
                }
                Ok(undefined())
            }
            // Download config / extension filter: acknowledged, nothing to schedule.
            "download-config-change" | "invalid-file-exts-change" => Ok(undefined()),
            // Full-screen photo viewer page (`$zresource.getPhotoviewerUrl`).
            "get-photo-viewer-url" => Ok(Value::String(format!("{}/app/popup-viewer.html", self.base_url()))),

            // Page transitions main used to perform.
            "login-success" => {
                let query = format!(
                    "type=23&client_version={}&app_version={}&about_version=&new_install=0&already_login=y&fail_sqlite3=true&new_install_success=1",
                    *self.client_version.lock().unwrap(),
                    self.app_version()
                );
                self.events.push(json!({ "kind": "navigate", "url": format!("/app/index.html?{query}") }));
                self.push("login-success", &[]);
                Ok(undefined())
            }
            "login" => {
                let url = self.page_url("/app/login.html");
                self.events.push(json!({ "kind": "navigate", "url": url }));
                self.push("signal_USER_NOT_LOGGED_IN", &[]);
                Ok(undefined())
            }

            // Session / database plumbing (ZaloMainProcess on macOS).
            // Every page announces itself on load. Replay the stored session so a
            // page that subscribed after the last broadcast (the hidden shared
            // worker) still runs SecureLocalStorage.init(userId, UIN).
            "renderer-ready" => {
                let session = self.session.lock().unwrap().clone();
                if let Some(session) = session {
                    self.push("new-session", &[session]);
                }
                Ok(undefined())
            }
            "new-session" => {
                let session = args.first().cloned().unwrap_or(Value::Null);
                let previous = self.session.lock().unwrap().clone();
                *self.session.lock().unwrap() = Some(session.clone());
                // Đổi tài khoản: helper của tài khoản cũ phải dừng và quên hết
                // (`CallBridge.reset()` trên macOS).
                if previous != Some(session.clone()) {
                    self.call.reset();
                }
                // Electron's main broadcast the session back after `authenticate`;
                // the renderer's session-manager listens on this same channel and only
                // then dispatches `Authenticating`, where SecureLocalStorage is
                // initialised with the session UIN (its cipher key). Without the echo
                // the storage stays locked and every secure item read throws
                // "SecureLocalStorage instance hasn't been initialized yet!".
                self.push("new-session", &[session]);
                Ok(undefined())
            }
            "db-authenticated" => {
                let session = args.first().cloned().unwrap_or(Value::Null);
                *self.session.lock().unwrap() = Some(session.clone());
                self.push("db-authenticated", &[session]);
                Ok(undefined())
            }
            "db-client-connected" => {
                let session = self.session.lock().unwrap().clone();
                if let Some(session) = session {
                    self.push("db-authenticated", &[session]);
                }
                Ok(undefined())
            }
            "db-start-close-dbs" => {
                let payload = args.first().cloned().unwrap_or(Value::Null);
                self.push("db-start-close-dbs", &[payload, args.get(1).cloned().unwrap_or(Value::Null)]);
                Ok(undefined())
            }

            // $zperf.getCpuInfos → Electron app.getAppMetrics(). The renderer
            // reduces over this array; returning undefined rejected its promise.
            "get-cpu-infos" => {
                let pid = std::process::id() as i64;
                Ok(json!([{
                    "pid": pid,
                    "type": "Browser",
                    "cpu": { "percentCPUUsage": 0.0, "idleWakeupsPerSecond": 0 },
                    "memory": { "workingSetSize": 0, "peakWorkingSetSize": 0 },
                }]))
            }

            // -------------------------------------------------- shared worker
            // The viewer key decrypts the local data stores. Electron main kept
            // it and relayed it to the hidden worker window, whose
            // SecureLocalStorage needs it (that missing relay is why the page
            // reported "SecureLocalStorage instance hasn't been initialized yet").
            "receive-viewerkey" => {
                let key = args.first().cloned().unwrap_or(Value::Null);
                self.worker.lock().unwrap().viewer_key = Some(key.clone());
                self.push_to("shared-worker", "receive-viewerkey", &[key]);
                Ok(undefined())
            }
            "shared-worker-show" | "shared-worker-hide" | "shared-worker-ping"
            | "shared-worker-dispose" | "shared-worker-start" => Ok(undefined()),
            "shared-worker-handle" => {
                let task_type = str_arg(args, 0).unwrap_or_default();
                if task_type.is_empty() {
                    return Ok(undefined());
                }
                if self.debug {
                    eprintln!("[worker] shared-worker-handle {task_type} từ {window}");
                }
                let replay = {
                    let mut worker = self.worker.lock().unwrap();
                    worker.handlers.insert(task_type, window.to_string());
                    if window == "shared-worker" {
                        worker.ready = true;
                        worker.last_remote_config.clone()
                    } else {
                        None
                    }
                };
                if let Some(config) = replay {
                    self.push_to("shared-worker", "shared-worker-remote-config-change", &[config]);
                }
                Ok(undefined())
            }
            "shared-worker-run" => {
                let Some(task) = args.first().and_then(|value| value.as_object()) else {
                    return Ok(undefined());
                };
                let request_id = task.get("id").cloned().unwrap_or(Value::Null);
                // Task ids are only unique inside one renderer; both main and
                // the hidden worker start at 1. Prefix with the sender, like
                // Electron's sharedWorkerManager, so a response cannot be routed
                // to the other window's task with the same numeric id.
                let key = format!("{window}:{}", id_string(&request_id));
                let task_type = task.get("type").and_then(|value| value.as_str()).unwrap_or_default().to_string();
                let handler = {
                    let mut worker = self.worker.lock().unwrap();
                    if !worker.ready {
                        None
                    } else {
                        worker.handlers.get(&task_type).cloned().map(|handler| {
                            worker.pending.insert(
                                key.clone(),
                                PendingTask {
                                    requester: window.to_string(),
                                    handler: handler.clone(),
                                    request_id: request_id.clone(),
                                    task_type: task_type.clone(),
                                    started: std::time::Instant::now(),
                                },
                            );
                            handler
                        })
                    }
                };
                if self.debug {
                    eprintln!(
                        "[worker] shared-worker-run {task_type} từ {window} → {}",
                        if handler.is_some() { "handler" } else { "unsupported" }
                    );
                }
                match handler {
                    Some(handler) => {
                        let mut routed = task.clone();
                        routed.insert("id".to_string(), Value::String(key));
                        self.push_to(&handler, "shared-worker-run", &[Value::Object(routed)]);
                    }
                    None => {
                        let error = if self.worker.lock().unwrap().ready {
                            "Task type is not supported"
                        } else {
                            "Worker is not ready"
                        };
                        self.push_to(window, "shared-worker-resp", &[json!({ "id": request_id, "error": error })]);
                    }
                }
                Ok(undefined())
            }
            "shared-worker-resp" => {
                let Some(message) = args.first() else { return Ok(undefined()) };
                let key = message.get("id").map(id_string).unwrap_or_default();
                let entry = self.worker.lock().unwrap().pending.remove(&key);
                if let Some(entry) = entry {
                    if self.debug {
                        let elapsed = entry.started.elapsed().as_millis();
                        let error = message.get("error").map(|value| value.to_string());
                        match error {
                            Some(error) => eprintln!(
                                "[worker] shared-worker-resp {} sau {elapsed} ms error={}",
                                entry.task_type,
                                error.chars().take(180).collect::<String>()
                            ),
                            None => eprintln!(
                                "[worker] shared-worker-resp {} sau {elapsed} ms"
                                , entry.task_type
                            ),
                        }
                    }
                    let mut routed = message.clone();
                    if let Some(object) = routed.as_object_mut() {
                        object.insert("id".to_string(), entry.request_id);
                    }
                    self.push_to(&entry.requester, "shared-worker-resp", &[routed]);
                } else if self.debug {
                    // A reply that arrives after the caller gave up (or for another sender) is
                    // invisible otherwise; it is the difference between "slow" and "lost".
                    eprintln!("[worker] shared-worker-resp bỏ qua task không chờ: {key}");
                }
                Ok(undefined())
            }
            "shared-worker-progress" => {
                let Some(message) = args.first() else { return Ok(undefined()) };
                let key = message.get("id").map(id_string).unwrap_or_default();
                let entry = self.worker.lock().unwrap().pending.get(&key).cloned();
                if let Some(entry) = entry {
                    let mut routed = message.clone();
                    if let Some(object) = routed.as_object_mut() {
                        object.insert("id".to_string(), entry.request_id);
                        object
                            .entry("type".to_string())
                            .or_insert(Value::String(entry.task_type));
                    }
                    self.push_to(&entry.requester, "shared-worker-progress", &[routed]);
                }
                Ok(undefined())
            }
            "shared-worker-abort" => {
                let Some(message) = args.first() else { return Ok(undefined()) };
                let key = format!(
                    "{window}:{}",
                    message.get("id").map(id_string).unwrap_or_default()
                );
                let entry = self.worker.lock().unwrap().pending.remove(&key);
                if let Some(entry) = entry {
                    self.push_to(&entry.handler, "shared-worker-abort", &[json!({ "id": key })]);
                    self.push_to(
                        &entry.requester,
                        "shared-worker-resp",
                        &[json!({ "id": entry.request_id, "error": "Aborted" })],
                    );
                }
                Ok(undefined())
            }
            "shared-worker-update-params" => {
                let Some(message) = args.first() else { return Ok(undefined()) };
                let key = format!(
                    "{window}:{}",
                    message.get("id").map(id_string).unwrap_or_default()
                );
                let entry = self.worker.lock().unwrap().pending.get(&key).cloned();
                if let Some(entry) = entry {
                    let mut routed = message.clone();
                    if let Some(object) = routed.as_object_mut() {
                        object.insert("id".to_string(), Value::String(key));
                    }
                    self.push_to(&entry.handler, "shared-worker-update-params", &[routed]);
                }
                Ok(undefined())
            }
            "shared-worker-remote-config-change" => {
                let config = args
                    .first()
                    .and_then(|value| value.get("config"))
                    .cloned()
                    .or_else(|| args.first().cloned())
                    .unwrap_or(Value::Null);
                self.worker.lock().unwrap().last_remote_config = Some(config.clone());
                self.push_to("shared-worker", "shared-worker-remote-config-change", &[config]);
                Ok(undefined())
            }

            // zsafe-storage: electron-store "zsafe-storage", key path zsskd.<key>.
            "zsafestorage-execute" => self.safe_storage(args),

            // Trusted-device protocol cipher.
            "zproto-encrypt-with-keychain" => self.zproto_encrypt(args),
            "zproto-decrypt-with-keychain" => self.zproto_decrypt(args),
            "zproto-config-sync-to-main" => Ok(undefined()),

            // Window chrome / notifications: acknowledged, no native surface yet.
            "minimize" | "toggle-maximize" | "check-maximize" | "focus-window" | "main-window-expand"
            | "set-login-size" | "close-window" | "hide-window" | "badge-count"
            | "app-started" | "get-auth-domain-ready" | "get-auth-domain" | "script-connected"
            | "script-terminated" | "open-channel-to" | "port-delivered" | "get-imei" => Ok(undefined()),
            // zcall-v2: renderer → trình gọi native. `args` = `[json, optional]`
            // từ shim preload (`HostScripts/call-adapter.js`): `json` là message
            // đúng như main Electron `JSON.stringify`, `_optional` đã stripped
            // và cờ `optional` nói riêng có khởi động helper hay không.
            "call-send-to-native" => {
                if let Some(json) = str_arg(args, 0) {
                    let optional = args.get(1).and_then(Value::as_bool).unwrap_or(false);
                    self.call.send_to_native(json, optional);
                }
                Ok(undefined())
            }
            "call-init" => {
                self.call.init(str_arg(args, 0));
                Ok(undefined())
            }
            // Banner in-page từ chối cuộc gọi đến (helper nội bộ): gửi signal
            // 405/12437 lên server qua renderer, đúng như helper thật.
            "call-decline" => {
                self.call.decline();
                Ok(undefined())
            }
            other => {
                let mut unhandled = self.unhandled.lock().unwrap();
                let count = unhandled.entry(format!("ipc:{other}")).or_insert(0);
                *count += 1;
                let first = *count == 1;
                drop(unhandled);
                if self.debug && first {
                    eprintln!("[bridge] channel chưa cài: {other} (sync={sync}) → undefined");
                }
                Ok(undefined())
            }
        }
    }

    fn push(&self, channel: &str, args: &[Value]) {
        self.events.push(json!({ "kind": "send", "channel": channel, "args": args }));
    }

    /// Push only to one window: the hidden shared-worker page and the main
    /// renderer share this queue, so task traffic must name its destination.
    fn push_to(&self, target: &str, channel: &str, args: &[Value]) {
        self.events.push(json!({ "kind": "send", "channel": channel, "args": args, "target": target }));
    }

    fn safe_storage(&self, args: &[Value]) -> Result<Value, String> {
        let method = str_arg(args, 0).unwrap_or_default();
        let key = str_arg(args, 1).unwrap_or_default();
        match method.as_str() {
            "encryptString" => {
                let text = str_arg(args, 1).unwrap_or_default();
                let (out, _) = cipher_run("aes-256-cbc", true, SAFE_STORAGE_KEY.as_bytes(), &SAFE_STORAGE_IV, text.as_bytes(), None, &[])?;
                Ok(Value::String(base64::engine::general_purpose::STANDARD.encode(out)))
            }
            "decryptString" => {
                let data = base64::engine::general_purpose::STANDARD
                    .decode(key.as_bytes())
                    .map_err(|e| format!("bad base64: {e}"))?;
                let (out, _) = cipher_run("aes-256-cbc", false, SAFE_STORAGE_KEY.as_bytes(), &SAFE_STORAGE_IV, &data, None, &[])?;
                Ok(Value::String(String::from_utf8_lossy(&out).into_owned()))
            }
            "get" => {
                let store = self.store_read("zsafe-storage");
                let value = store.get("zsskd").and_then(|v| v.get(&key)).cloned();
                Ok(value.unwrap_or(Value::Null))
            }
            "set" => {
                let mut store = self.store_read("zsafe-storage");
                if !store.is_object() {
                    store = json!({});
                }
                let map = store.as_object_mut().unwrap();
                let entry = map.entry("zsskd".to_string()).or_insert_with(|| json!({}));
                if !entry.is_object() {
                    *entry = json!({});
                }
                entry.as_object_mut().unwrap().insert(key, args.get(2).cloned().unwrap_or(Value::Null));
                self.store_write("zsafe-storage", store);
                Ok(undefined())
            }
            other => Err(format!("zsafeStorage.{other} chưa được cài")),
        }
    }

    fn secret(&self, identifier: &str) -> Vec<u8> {
        let id: String = identifier
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '_' })
            .collect();
        if let Some(key) = self.secrets.lock().unwrap().get(&id) {
            return key.clone();
        }
        let path = self.userdata.join(format!("zprotocol-{id}.key"));
        let key = std::fs::read(&path).unwrap_or_else(|_| {
            let mut bytes = vec![0u8; 32];
            rand::thread_rng().fill_bytes(&mut bytes);
            let _ = std::fs::write(&path, &bytes);
            bytes
        });
        let mut secrets = self.secrets.lock().unwrap();
        secrets.insert(id, key.clone());
        key
    }

    fn zproto_encrypt(&self, args: &[Value]) -> Result<Value, String> {
        let identifier = str_arg(args, 0).unwrap_or_default();
        let items = args.get(1).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let key = self.secret(&identifier);
        let mut out = Vec::new();
        for item in &items {
            let plain = value_bytes(item).unwrap_or_default();
            let mut salt = [0u8; 16];
            let mut iv = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut salt);
            rand::thread_rng().fill_bytes(&mut iv);
            let derived = hkdf_sha256(&key, &salt, ZPROTO_KDF_INFO, 64);
            let (enc_key, mac_key) = derived.split_at(32);
            let (cipher_text, _) = cipher_run("aes-256-cbc", true, enc_key, &iv, &plain, None, &[])?;
            let mut mac_input = cipher_text.clone();
            mac_input.extend_from_slice(&iv);
            mac_input.extend_from_slice(&salt);
            let tag = hmac_sha256(mac_key, &mac_input);
            let mut blob = Vec::with_capacity(1 + 16 + 16 + cipher_text.len() + 32);
            blob.push(0x49); // raw key mode (macOS uses 0x4B only with Keychain)
            blob.extend_from_slice(&iv);
            blob.extend_from_slice(&salt);
            blob.extend_from_slice(&cipher_text);
            blob.extend_from_slice(&tag);
            out.push(bytes_out(blob, false));
        }
        Ok(Value::Array(out))
    }

    fn zproto_decrypt(&self, args: &[Value]) -> Result<Value, String> {
        let identifier = str_arg(args, 0).unwrap_or_default();
        let items = args.get(1).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let key = self.secret(&identifier);
        let mut out = Vec::new();
        for item in &items {
            let blob = value_bytes(item).unwrap_or_default();
            if blob.len() < 65 {
                return Err("zproto: dữ liệu mã hoá quá ngắn".into());
            }
            let iv = &blob[1..17];
            let salt = &blob[17..33];
            let cipher_text = &blob[33..blob.len() - 32];
            let tag = &blob[blob.len() - 32..];
            let derived = hkdf_sha256(&key, salt, ZPROTO_KDF_INFO, 64);
            let (enc_key, mac_key) = derived.split_at(32);
            let mut mac_input = cipher_text.to_vec();
            mac_input.extend_from_slice(iv);
            mac_input.extend_from_slice(salt);
            if hmac_sha256(mac_key, &mac_input) != tag {
                return Err("zproto: HMAC không khớp".into());
            }
            let (plain, _) = cipher_run("aes-256-cbc", false, enc_key, iv, cipher_text, None, &[])?;
            out.push(bytes_out(plain, false));
        }
        Ok(Value::Array(out))
    }
}

// ------------------------------------------------------------------ value helpers

pub fn undefined() -> Value {
    json!({ "__undefined": true })
}

fn bytes_out(data: Vec<u8>, array_buffer: bool) -> Value {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    if array_buffer {
        json!({ "__bytes": encoded, "__arraybuffer": true })
    } else {
        json!({ "__bytes": encoded })
    }
}

pub fn value_bytes(value: &Value) -> Option<Vec<u8>> {
    if let Some(encoded) = value.get("__bytes").and_then(|v| v.as_str()) {
        return base64::engine::general_purpose::STANDARD.decode(encoded).ok();
    }
    value.as_str().map(|text| text.as_bytes().to_vec())
}

fn arg_bytes(args: &[Value], index: usize) -> Option<Vec<u8>> {
    args.get(index).and_then(value_bytes)
}

/// The photo-thumbnail pipeline fails inside the renderer without saying why, so
/// the members it depends on are logged as they arrive. Everything else on the
/// bridge stays quiet — a photo bubble is not worth a line per call.
fn trace_call(name: &str, args: &[Value]) {
    const WATCH: [&str; 5] = [
        "exec_raw",
        "gen-thumb_write-savepath-file-photo",
        "$zFileManager.svcReadPath",
        "$zFileManager.svcStatAsync",
        "$zFileManager.getFileArrayBuffer",
    ];
    let all = std::env::var("OLAZ_TRACE_ALL").map(|value| value == "1").unwrap_or(false);
    if !all && !WATCH.contains(&name) {
        return;
    }
    let first = args.first().map(Value::to_string).unwrap_or_default();
    let brief: String = first.chars().take(240).collect();
    eprintln!("[bridge] call {name} {brief}");
}

/// `$zFeatures.mainless.execRaw` tasks the photo-thumbnail pipeline needs.
///
/// `rmd5` keys the thumbnail cache (`thumb-file-v2` compares it with the checksum
/// the message carries); `genThumbPicBuffer` decodes the message photo and hands
/// back a JPEG that fits 600×600. Everything else stays unsupported so the
/// renderer sees the same rejection macOS gives for tasks Linux has no worker for.
fn exec_raw(task: &str, params: Value) -> Result<Value, String> {
    let params: Vec<Value> = params.as_array().cloned().unwrap_or_default();
    match task {
        "rmd5" => {
            let bytes = arg_bytes(&params, 0).ok_or_else(|| "EINVAL: rmd5 thiếu buffer".to_string())?;
            Ok(Value::String(hex_encode(&md5::Md5::digest(&bytes))))
        }
        "genThumbPicBuffer" => {
            let request = params.first().cloned().unwrap_or(Value::Null);
            let options = request.get("options").cloned().unwrap_or(Value::Null);
            let source = options
                .get("buffer")
                .and_then(value_bytes)
                .or_else(|| {
                    request
                        .get("inputPath")
                        .and_then(Value::as_str)
                        .and_then(|path| std::fs::read(path).ok())
                })
                .ok_or_else(|| "EINVAL: genThumbPicBuffer thiếu buffer".to_string())?;
            let picture = decode_picture(&source)?;
            let (width, height) = (picture.width(), picture.height());
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 75)
                .encode_image(&picture.thumbnail(600, 600))
                .map_err(|error| format!("EIO: jpeg {error}"))?;
            Ok(json!({
                "buffer": bytes_out(jpeg, true),
                "tDimension": { "width": width, "height": height },
            }))
        }
        other => Err(format!("NotSupported: {other}")),
    }
}

/// JPEG XL (what the CDN serves) or an image the `image` crate already knows.
fn decode_picture(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    if crate::download::is_jxl(bytes) {
        let decoder = jxl_oxide::integration::JxlDecoder::new(std::io::Cursor::new(bytes))
            .map_err(|error| format!("EINVAL: jxl {error}"))?;
        return image::DynamicImage::from_decoder(decoder)
            .map_err(|error| format!("EINVAL: jxl {error}"));
    }
    image::load_from_memory(bytes).map_err(|error| format!("EINVAL: ảnh {error}"))
}

/// One path component made of characters a Zalo id can produce; anything else is
/// dropped so a renderer-supplied value cannot walk out of the downloads folder.
fn path_component(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// `<userData>/ZaloPC` — the root the official client keeps `ZaloDownloads` under.
///
/// Deriving it from `userdata.parent()` only works while userData is
/// `<XDG_DATA_HOME>/olaz`; `OLAZ_USERDATA` (the test harness) and any distro that moves
/// the profile put it elsewhere, and a cache written to the wrong tree is one the
/// renderer never reads. Ask `host::userdata_dir()` instead, and fall back to the
/// sibling layout for a userData that a previous version created that way.
fn bridge_zalo_pc(userdata: &Path) -> PathBuf {
    let sibling = userdata
        .parent()
        .map(|share| share.join("olaz").join("ZaloPC"))
        .unwrap_or_else(|| userdata.join("ZaloPC"));
    let resolved = crate::host::userdata_dir().join("ZaloPC");
    // `OLAZ_USERDATA` is the explicit override — believe it over the derived sibling.
    let explicit = std::env::var("OLAZ_USERDATA").map(|value| !value.is_empty()).unwrap_or(false);
    if explicit || !sibling.is_dir() || resolved == sibling {
        return resolved;
    }
    sibling
}

fn id_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The preload accepted Node encodings for crypto inputs (`utf8`, `hex`,
/// `base64`, `base64url`, `binary`, `utf16le`).
fn decode_with(value: &Value, encoding: Option<&str>) -> Vec<u8> {
    if let Some(encoded) = value.get("__bytes").and_then(|v| v.as_str()) {
        return base64::engine::general_purpose::STANDARD.decode(encoded).unwrap_or_default();
    }
    let Some(text) = value.as_str() else { return Vec::new() };
    match encoding.unwrap_or("utf8").to_ascii_lowercase().as_str() {
        "hex" => (0..text.len() / 2)
            .filter_map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok())
            .collect(),
        "base64" => base64::engine::general_purpose::STANDARD.decode(text).unwrap_or_default(),
        "base64url" => base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text).unwrap_or_default(),
        "binary" | "latin1" | "ascii" => text.bytes().collect(),
        "ucs2" | "ucs-2" | "utf16le" | "utf-16le" => {
            text.encode_utf16().flat_map(|unit| unit.to_le_bytes()).collect()
        }
        _ => text.as_bytes().to_vec(),
    }
}

fn encode_with(data: &[u8], encoding: Option<&str>) -> Value {
    match encoding.map(str::to_ascii_lowercase).as_deref() {
        Some("hex") => Value::String(hex_encode(data)),
        Some("base64") => Value::String(base64::engine::general_purpose::STANDARD.encode(data)),
        Some("base64url") => Value::String(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)),
        Some("binary") | Some("latin1") | Some("ascii") => {
            Value::String(data.iter().map(|byte| char::from(*byte)).collect())
        }
        Some(_) => Value::String(String::from_utf8_lossy(data).into_owned()),
        None => bytes_out(data.to_vec(), false),
    }
}

fn str_arg(args: &[Value], index: usize) -> Option<String> {
    match args.get(index)? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// Chỉ http/https được mở ra ngoài (giống lọc scheme của bản macOS trong
/// `OfficialClientHost.decidePolicyFor`); URL khác (javascript:, file:, …) bị bỏ.
pub fn external_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Mở URL bằng trình duyệt hệ thống (`xdg-open` — thay `NSWorkspace.shared.open`).
/// Thread riêng để reap tiến trình con, không khoá thread bridge.
pub fn open_external(url: &str) {
    match std::process::Command::new("xdg-open").arg(url).spawn() {
        Ok(child) => {
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
        }
        Err(error) => eprintln!("[open-external] xdg-open lỗi: {error}"),
    }
}

#[cfg(test)]
mod external_link_tests {
    use super::external_url;

    #[test]
    fn only_http_https_passes_the_external_filter() {
        assert_eq!(
            external_url("  https://zalo.me/blog  "),
            Some("https://zalo.me/blog".to_string())
        );
        assert_eq!(external_url("http://example.com"), Some("http://example.com".to_string()));
        assert_eq!(external_url("javascript:alert(1)"), None);
        assert_eq!(external_url("file:///etc/passwd"), None);
        assert_eq!(external_url("data:text/html,x"), None);
        assert_eq!(external_url(""), None);
    }
}

fn int_arg(args: &[Value], index: usize) -> Option<i64> {
    match args.get(index)? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn io_error(error: &std::io::Error, path: &str) -> String {
    use std::io::ErrorKind::*;
    let code = match error.kind() {
        NotFound => "ENOENT",
        PermissionDenied => "EACCES",
        AlreadyExists => "EEXIST",
        _ => "EIO",
    };
    format!("{code}: {} ({path})", error)
}

fn stats_value(metadata: &std::fs::Metadata) -> Value {
    use std::os::unix::fs::PermissionsExt;
    let millis = |time: std::io::Result<std::time::SystemTime>| {
        time.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    };
    let mtime = millis(metadata.modified());
    let atime = millis(metadata.accessed());
    let ctime = millis(metadata.created());
    json!({
        "__stats": true,
        "size": metadata.len(),
        "mode": metadata.permissions().mode(),
        "mtimeMs": mtime, "ctimeMs": ctime, "atimeMs": atime, "birthtimeMs": ctime,
        "isFile": metadata.is_file(),
        "isDirectory": metadata.is_dir(),
        "isSymbolicLink": metadata.file_type().is_symlink(),
    })
}

/// `$zFileManager.svcReadPath(p, {start, end})` — one byte range of a local file,
/// used by the media cache to read previews/thumbnails without loading the whole file.
/// `$zwindow.pickFolder(title, defaultPath)` — native GTK folder chooser. The
/// bridge handler runs on a loopback-server thread, so the dialog is scheduled
/// on the GTK main context and that thread waits for the answer.
fn pick_folder(title: &str, start: &str) -> Result<Option<String>, String> {
    use gtk::prelude::*;
    use std::sync::mpsc;

    let (sender, receiver) = mpsc::channel();
    let title = title.to_string();
    let start = start.to_string();
    gtk::glib::MainContext::default().invoke(move || {
        let dialog = gtk::FileChooserDialog::new(
            if title.is_empty() { None } else { Some(title.as_str()) },
            None::<&gtk::Window>,
            gtk::FileChooserAction::SelectFolder,
        );
        dialog.add_button("Hủy", gtk::ResponseType::Cancel);
        dialog.add_button("Chọn", gtk::ResponseType::Accept);
        if !start.is_empty() && Path::new(&start).is_dir() {
            let _ = dialog.set_current_folder(&start);
        }
        let accepted = dialog.run() == gtk::ResponseType::Accept;
        let picked = if accepted {
            dialog.filename().map(|path| path.to_string_lossy().into_owned())
        } else {
            None
        };
        dialog.close();
        let _ = sender.send(picked);
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(600))
        .map_err(|_| "hộp thoại chọn thư mục không phản hồi".to_string())
}

/// `show-save-dialog` — GTK save dialog thay `dialog.showSaveDialog` của
/// Electron; `defaultPath` là file đã có (media cache) thì lấy tên + thư mục.
fn save_file_dialog(title: &str, default_path: &str) -> Result<Option<String>, String> {
    use gtk::prelude::*;
    use std::sync::mpsc;

    let (sender, receiver) = mpsc::channel();
    let title = title.to_string();
    let default_path = default_path.to_string();
    gtk::glib::MainContext::default().invoke(move || {
        let dialog = gtk::FileChooserDialog::new(
            if title.is_empty() { None } else { Some(title.as_str()) },
            None::<&gtk::Window>,
            gtk::FileChooserAction::Save,
        );
        dialog.add_button("Hủy", gtk::ResponseType::Cancel);
        dialog.add_button("Lưu", gtk::ResponseType::Accept);
        dialog.set_do_overwrite_confirmation(true);
        // `defaultPath` là đường dẫn ĐỀ XUẤT (thường chưa tồn tại trên đĩa) —
        // luôn lấy tên file và thư mục cha từ nó, không kiểm tra is_file.
        let path = Path::new(&default_path);
        if let Some(dir) = path.parent().filter(|dir| dir.is_dir()) {
            let _ = dialog.set_current_folder(dir);
        }
        if let Some(name) = path.file_name() {
            let name = name.to_string_lossy();
            if !name.is_empty() {
                dialog.set_current_name(&name);
            }
        }
        let accepted = dialog.run() == gtk::ResponseType::Accept;
        let picked = if accepted {
            dialog.filename().map(|path| path.to_string_lossy().into_owned())
        } else {
            None
        };
        dialog.close();
        let _ = sender.send(picked);
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(600))
        .map_err(|_| "hộp thoại lưu không phản hồi".to_string())
}

fn file_md5(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = md5::Md5::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

fn ranged_read(path: &Path, start: u64, end: Option<u64>) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let display = path.display().to_string();
    let mut file = std::fs::File::open(path).map_err(|error| io_error(&error, &display))?;
    let len = file.metadata().map_err(|error| io_error(&error, &display))?.len();
    let start = start.min(len);
    let end = end.unwrap_or(len).min(len).max(start);
    file.seek(SeekFrom::Start(start)).map_err(|error| io_error(&error, &display))?;
    let mut bytes = vec![0u8; (end - start) as usize];
    file.read_exact(&mut bytes).map_err(|error| io_error(&error, &display))?;
    Ok(bytes)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = to.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::copy(from, to).map(|_| ())
    }
}

// ------------------------------------------------------------------ crypto

type HmacSha256 = Hmac<Sha256>;

fn hash_digest(alg: &str, data: &[u8]) -> Result<Vec<u8>, String> {
    let name = alg.to_ascii_lowercase().replace('-', "");
    Ok(match name.as_str() {
        "md5" => md5::Md5::digest(data).to_vec(),
        "sha1" => sha1::Sha1::digest(data).to_vec(),
        "sha256" => Sha256::digest(data).to_vec(),
        "sha384" => Sha384::digest(data).to_vec(),
        "sha512" => Sha512::digest(data).to_vec(),
        other => return Err(format!("NotSupportedError: hash {other}")),
    })
}

fn encode_digest(digest: &[u8], encoding: Option<&str>) -> Value {
    match encoding {
        Some("hex") => Value::String(hex_encode(digest)),
        Some("base64") => Value::String(base64::engine::general_purpose::STANDARD.encode(digest)),
        _ => bytes_out(digest.to_vec(), false),
    }
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], length: usize) -> Vec<u8> {
    let mut extract = <HmacSha256 as Mac>::new_from_slice(salt).expect("hkdf salt");
    extract.update(ikm);
    let prk = extract.finalize().into_bytes();
    let mut output = Vec::with_capacity(length);
    let mut previous: Vec<u8> = Vec::new();
    let mut counter = 1u8;
    while output.len() < length {
        let mut expand = <HmacSha256 as Mac>::new_from_slice(&prk).expect("hkdf prk");
        expand.update(&previous);
        expand.update(info);
        expand.update(&[counter]);
        previous = expand.finalize().into_bytes().to_vec();
        output.extend_from_slice(&previous);
        counter = counter.wrapping_add(1);
    }
    output.truncate(length);
    output
}

/// AES-CBC (PKCS#7) and AES-GCM, the two modes Zalo's renderer uses.
fn cipher_run(
    alg: &str,
    encrypt: bool,
    key: &[u8],
    iv: &[u8],
    data: &[u8],
    tag: Option<&[u8]>,
    aad: &[u8],
) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
    let name = alg.to_ascii_lowercase();
    if name.ends_with("-gcm") {
        gcm_run(encrypt, key, iv, data, tag, aad)
    } else if name.ends_with("-cbc") {
        cbc_run(encrypt, key, iv, data).map(|out| (out, None))
    } else {
        Err(format!("NotSupportedError: cipher {alg}"))
    }
}

pub(crate) fn cbc_run(encrypt: bool, key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    use cbc::cipher::generic_array::GenericArray;
    use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};

    if iv.len() != 16 {
        return Err(format!("OperationError: AES-CBC cần IV 16 byte (nhận {})", iv.len()));
    }
    let invalid = || "OperationError: AES key/IV không hợp lệ".to_string();

    if encrypt {
        let mut buffer = pkcs7_pad(data);
        macro_rules! run {
            ($ty:ty) => {{
                let mut cipher = <$ty>::new_from_slices(key, iv).map_err(|_| invalid())?;
                for chunk in buffer.chunks_mut(16) {
                    cipher.encrypt_block_mut(GenericArray::from_mut_slice(chunk));
                }
            }};
        }
        match key.len() {
            16 => run!(cbc::Encryptor<aes::Aes128>),
            24 => run!(cbc::Encryptor<aes::Aes192>),
            32 => run!(cbc::Encryptor<aes::Aes256>),
            _ => return Err(invalid()),
        }
        Ok(buffer)
    } else {
        if data.is_empty() || data.len() % 16 != 0 {
            return Err("OperationError: AES-CBC ciphertext không phải bội số của 16".into());
        }
        let mut buffer = data.to_vec();
        macro_rules! run {
            ($ty:ty) => {{
                let mut cipher = <$ty>::new_from_slices(key, iv).map_err(|_| invalid())?;
                for chunk in buffer.chunks_mut(16) {
                    cipher.decrypt_block_mut(GenericArray::from_mut_slice(chunk));
                }
            }};
        }
        match key.len() {
            16 => run!(cbc::Decryptor<aes::Aes128>),
            24 => run!(cbc::Decryptor<aes::Aes192>),
            32 => run!(cbc::Decryptor<aes::Aes256>),
            _ => return Err(invalid()),
        }
        pkcs7_unpad(buffer)
    }
}

fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let padding = 16 - (data.len() % 16);
    let mut buffer = data.to_vec();
    buffer.extend(std::iter::repeat(padding as u8).take(padding));
    buffer
}

fn pkcs7_unpad(mut data: Vec<u8>) -> Result<Vec<u8>, String> {
    let Some(&padding) = data.last() else { return Ok(data) };
    let padding = padding as usize;
    if padding == 0 || padding > 16 || padding > data.len() {
        return Err("OperationError: padding PKCS#7 không hợp lệ".into());
    }
    data.truncate(data.len() - padding);
    Ok(data)
}

fn gcm_run(
    encrypt: bool,
    key: &[u8],
    iv: &[u8],
    data: &[u8],
    tag: Option<&[u8]>,
    aad: &[u8],
) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes128Gcm, Aes256Gcm, KeyInit, Nonce};

    if iv.len() != 12 {
        return Err(format!("OperationError: AES-GCM cần IV 12 byte (nhận {})", iv.len()));
    }
    let nonce = Nonce::from_slice(iv);
    let payload = Payload { msg: data, aad };

    if encrypt {
        macro_rules! run {
            ($ty:ty) => {{
                let cipher = <$ty>::new_from_slice(key).map_err(|_| "OperationError: key GCM".to_string())?;
                cipher.encrypt(nonce, payload).map_err(|_| "OperationError: GCM encrypt".to_string())
            }};
        }
        let combined = match key.len() {
            16 => run!(Aes128Gcm),
            32 => run!(Aes256Gcm),
            _ => return Err("OperationError: AES-GCM chỉ hỗ trợ key 16/32 byte".into()),
        }?;
        // aes-gcm appends the tag; Node exposes it separately through getAuthTag().
        let split = combined.len().saturating_sub(16);
        Ok((combined[..split].to_vec(), Some(combined[split..].to_vec())))
    } else {
        let mut combined = data.to_vec();
        combined.extend_from_slice(tag.unwrap_or(&[]));
        macro_rules! run {
            ($ty:ty) => {{
                let cipher = <$ty>::new_from_slice(key).map_err(|_| "OperationError: key GCM".to_string())?;
                cipher.decrypt(nonce, Payload { msg: &combined, aad }).map_err(|_| "OperationError: GCM auth failed".to_string())
            }};
        }
        let plain = match key.len() {
            16 => run!(Aes128Gcm),
            32 => run!(Aes256Gcm),
            _ => return Err("OperationError: AES-GCM chỉ hỗ trợ key 16/32 byte".into()),
        }?;
        Ok((plain, None))
    }
}

/// `$znode.os.__info`: the snapshot `local-modules.js` reads for `$znode.os`
/// (it reports `darwin` itself — the renderer's device id depends on it).
fn os_info() -> Value {
    let mut hostname = [0u8; 256];
    let hostname = unsafe {
        if libc::gethostname(hostname.as_mut_ptr() as *mut libc::c_char, hostname.len()) == 0 {
            let end = hostname.iter().position(|&b| b == 0).unwrap_or(hostname.len());
            String::from_utf8_lossy(&hostname[..end]).into_owned()
        } else {
            "Mac".to_string()
        }
    };
    let memory = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix("MemTotal:")?;
                rest.split_whitespace().next()?.parse::<u64>().ok().map(|kb| kb * 1024)
            })
        })
        .unwrap_or(8_589_934_592);
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    json!({
        "arch": "arm64",
        "release": "24.0.0",
        "version": "Darwin Kernel Version 24.0.0",
        "hostname": hostname,
        "homedir": std::env::var("HOME").unwrap_or_else(|_| "/".into()),
        "tmpdir": "/tmp",
        "totalmem": memory,
        "freemem": memory / 2,
        "uptime": 0,
        "cpus": cpus,
        "cpuModel": "Apple M1",
        "username": std::env::var("USER").unwrap_or_else(|_| "olaz".into()),
        "networkInterfaces": "[]",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbc_roundtrip_matches_pkcs7() {
        let key = [7u8; 32];
        let iv = [3u8; 16];
        let (encrypted, _) = cipher_run("aes-256-cbc", true, &key, &iv, b"olaz", None, &[]).unwrap();
        assert_eq!(encrypted.len() % 16, 0);
        let (plain, _) = cipher_run("aes-256-cbc", false, &key, &iv, &encrypted, None, &[]).unwrap();
        assert_eq!(plain, b"olaz");
    }

    #[test]
    fn page_url_follows_the_installed_runtime_version() {
        let dir = std::env::temp_dir().join(format!("olaz-page-version-{}", std::process::id()));
        let bridge = Bridge::new(dir, "695".into(), false, Arc::new(Events::new()));
        bridge.set_app_version("26.11.10");
        let url = bridge.page_url("/app/login.html");
        assert!(url.contains("client_version=695"));
        assert!(url.contains("app_version=26.11.10"));
        assert!(!url.contains("app_version=26.10.20"));
    }

    #[test]
    fn shared_worker_routes_same_numeric_task_id_to_each_requester() {
        let dir = std::env::temp_dir().join(format!("olaz-worker-routes-{}", std::process::id()));
        let events = Arc::new(Events::new());
        let bridge = Bridge::new(dir, "694".into(), false, events.clone());
        events.push(json!({ "kind": "baseline" }));

        bridge.channel("shared-worker-handle", &[json!("main-task")], false, "main").unwrap();
        bridge
            .channel("shared-worker-handle", &[json!("worker-task")], false, "shared-worker")
            .unwrap();
        bridge
            .channel(
                "shared-worker-run",
                &[json!({ "id": 1, "type": "main-task" })],
                false,
                "main",
            )
            .unwrap();
        bridge
            .channel(
                "shared-worker-run",
                &[json!({ "id": 1, "type": "worker-task" })],
                false,
                "shared-worker",
            )
            .unwrap();

        let (run_cursor, runs) = events.since(1, std::time::Duration::ZERO);
        assert_eq!(runs.len(), 2);

        bridge
            .channel(
                "shared-worker-resp",
                &[json!({ "id": runs[0]["args"][0]["id"], "result": "main" })],
                false,
                "main",
            )
            .unwrap();
        bridge
            .channel(
                "shared-worker-resp",
                &[json!({ "id": runs[1]["args"][0]["id"], "result": "worker" })],
                false,
                "shared-worker",
            )
            .unwrap();

        let (_, responses) = events.since(run_cursor, std::time::Duration::ZERO);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["target"], "main");
        assert_eq!(responses[0]["args"][0]["id"], 1);
        assert_eq!(responses[0]["args"][0]["result"], "main");
        assert_eq!(responses[1]["target"], "shared-worker");
        assert_eq!(responses[1]["args"][0]["id"], 1);
        assert_eq!(responses[1]["args"][0]["result"], "worker");
    }

    #[test]
    fn crypto_one_shot_defaults_null_iv_and_honours_encodings() {
        let dir = std::env::temp_dir().join(format!("olaz-one-shot-{}", std::process::id()));
        let bridge = Bridge::new(dir, "694".into(), false, Arc::new(Events::new()));
        let key = "3FC4F0D2AB50057BCE0D90D9187A22B1";
        let encrypted = bridge
            .member(
                "$znode.crypto.encrypt",
                &[
                    json!("olaz"),
                    json!("aes-256-cbc"),
                    json!(key),
                    Value::Null,
                    json!({ "key": "utf-8", "input": "utf-8", "output": "hex" }),
                ],
                false,
            )
            .unwrap();
        let encrypted = encrypted.as_str().expect("output=hex phải trả chuỗi").to_string();
        assert_eq!(encrypted.len() % 32, 0);
        let plain = bridge
            .member(
                "$znode.crypto.decrypt",
                &[
                    json!(encrypted),
                    json!("aes-256-cbc"),
                    json!(key),
                    Value::Null,
                    json!({ "key": "utf-8", "input": "hex", "output": "utf-8" }),
                ],
                false,
            )
            .unwrap();
        assert_eq!(plain, json!("olaz"));
    }

    #[test]
    fn gcm_roundtrip_carries_tag() {
        let key = [9u8; 32];
        let iv = [1u8; 12];
        let (cipher_text, tag) = cipher_run("aes-256-gcm", true, &key, &iv, b"hello", None, &[1, 2]).unwrap();
        assert_eq!(tag.as_ref().map(|t| t.len()), Some(16));
        let (plain, _) = cipher_run("aes-256-gcm", false, &key, &iv, &cipher_text, tag.as_deref(), &[1, 2]).unwrap();
        assert_eq!(plain, b"hello");
    }

    #[test]
    fn sha256_digest_is_hex() {
        let digest = hash_digest("sha256", b"olaz").unwrap();
        assert_eq!(digest.len(), 32);
        let hex = hex_encode(&digest);
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(digest, hash_digest("sha256", b"olaz ").unwrap());
        assert_eq!(hash_digest("md5", b"olaz").unwrap().len(), 16);
    }

    #[test]
    fn file_md5_streams_known_content() {
        let dir = std::env::temp_dir().join(format!("olaz-md5-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.bin");
        std::fs::write(&file, b"hello").unwrap();
        assert_eq!(file_md5(&file).unwrap(), "5d41402abc4b2a76b9719d911017c592");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hkdf_is_deterministic_and_sized() {
        let a = hkdf_sha256(b"key", b"salt", b"info", 64);
        assert_eq!(a.len(), 64);
        assert_eq!(a, hkdf_sha256(b"key", b"salt", b"info", 64));
    }

    #[test]
    fn file_manager_logs_and_viewer_key_roundtrip() {
        let dir = std::env::temp_dir().join(format!("olaz-file-manager-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bridge = Bridge::new(dir.clone(), "694".into(), false, Arc::new(Events::new()));
        let payload = json!({ "__bytes": base64::engine::general_purpose::STANDARD.encode(b"hello") });
        bridge
            .member(
                "$zFileManager.writeZLog",
                &[json!("z.log"), payload, json!({ "overwrite": true })],
                false,
            )
            .unwrap();
        assert_eq!(
            bridge.member("$zFileManager.readZLog", &[json!("z.log")], false).unwrap(),
            json!("hello")
        );
        // The viewer key must reject when missing, not resolve empty: the renderer
        // uses the rejection to fall back to the login flow.
        assert!(bridge
            .member("$zFileManager.getViewerkey", &[json!(dir.to_string_lossy())], false)
            .is_err());
        bridge.member("$zFileManager.writeRecentId", &[json!("123")], false).unwrap();
        assert_eq!(
            bridge
                .member("$zFileManager.getViewerkey", &[json!(dir.to_string_lossy())], false)
                .unwrap(),
            json!("123")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Volume statistics for a path, shaped like macOS `getDiskUsage` (`total/free/available/used`)
/// plus `size`/`diskPath` for the `$znode.diskSpace` call. Without this the renderer got
/// `undefined` and treated a disk with tens of GB free as full.
fn disk_usage(path: &str) -> Result<Value, String> {
    let target = if path.is_empty() { "/" } else { path };
    let c_path = std::ffi::CString::new(target.as_bytes()).map_err(|_| "đường dẫn chứa NUL".to_string())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
        return Err(format!("statvfs {target}: {}", std::io::Error::last_os_error()));
    }
    let frsize = st.f_frsize as u64;
    let total = st.f_blocks as u64 * frsize;
    let free = st.f_bavail as u64 * frsize;
    Ok(json!({
        "total": total,
        "size": total,
        "free": free,
        "available": free,
        "used": total.saturating_sub(free),
        "diskPath": target,
    }))
}
