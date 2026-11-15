//! Olaz cho Linux — vỏ Tauri quanh renderer chính thức của Zalo PC.
//!
//! `--serve-only` chạy mỗi loopback bridge (không mở cửa sổ), dùng cho kiểm thử
//! và cho các script bên ngoài; mặc định thì mở cửa sổ WebKitGTK.

mod asar;
mod bridge;
mod call;
mod download;
mod error_catalog;
mod fingerprint;
mod host;
mod instance;
mod net;
mod runtime;
mod search_index;
mod session;
mod update;

use std::sync::Arc;
use std::time::Duration;

fn main() {
    // `--fetch-runtime` only installs the Zalo renderer and exits: the same
    // installer the app runs on first launch, usable from a terminal/CI.
    if std::env::args().any(|arg| arg == "--fetch-runtime") {
        let debug = std::env::var("OLAZ_DEBUG").map(|value| value != "0").unwrap_or(false);
        match runtime::install(&host::userdata_dir(), debug) {
            Ok(installed) => {
                println!("{}", installed.asar.display());
                return;
            }
            Err(error) => {
                eprintln!("olaz-linux: {error}");
                std::process::exit(1);
            }
        }
    }

    let serve_only = std::env::args().any(|arg| arg == "--serve-only");
    let ctx = match host::AppContext::load() {
        Ok(ctx) => Arc::new(ctx),
        Err(error) => {
            eprintln!("olaz-linux: {error}");
            std::process::exit(2);
        }
    };
    // Single-instance: một instance khác đang nghe cổng bridge (cùng origin,
    // không thể chạy song song) → hỏi người dùng đóng phiên cũ hay huỷ.
    // `--serve-only` là chế độ script, giữ hành vi cũ (fail khi cổng bận).
    if !serve_only {
        if let Some(old_pid) = instance::detect(instance::bridge_port()) {
            if !instance::ask_replace(old_pid) {
                eprintln!("[olaz] người dùng huỷ — một phiên Olaz khác vẫn đang chạy.");
                return;
            }
            if !instance::terminate(old_pid) {
                eprintln!("olaz-linux: không đóng được phiên cũ (PID {old_pid}) — cổng vẫn bận.");
                std::process::exit(2);
            }
            eprintln!("[olaz] đã đóng phiên cũ (PID {old_pid}) — mở phiên mới.");
        }
    }
    let running = match host::start(ctx.clone()) {
        Ok(running) => running,
        Err(error) => {
            eprintln!("olaz-linux: không mở được bridge trên 127.0.0.1: {error}");
            std::process::exit(2);
        }
    };
    // Dock icon + minimize: install the desktop entry and icons from the
    // AppImage before the window opens, so the shell can match the window's
    // `app_id`/`WM_CLASS` to an installed application. No-op outside an
    // AppImage (cargo run skips it).
    host::install_desktop_entry();
    eprintln!(
        "[olaz] bridge: http://127.0.0.1:{}  entry: {}  userData: {}",
        running.port,
        ctx.entry(),
        ctx.userdata.display()
    );

    // Diagnostics: prints the exact script injected into the webview. Before the
    // helper and the installer start, so this stays a pure read.
    if std::env::args().any(|arg| arg == "--print-init-script") {
        print!("{}", ctx.boot_script());
        return;
    }

    // Start the TLS/HTTP-2 fingerprint helper now, so the renderer's first request
    // does not wait for it (the macOS build warms it up at engine boot too).
    crate::fingerprint::warm_up(ctx.clone());

    // First-run initialisation: check what is installed, then install the
    // renderer in the background. Runs before the window opens, but never
    // downloads on this thread, so the window appears immediately.
    host::bootstrap_runtime(ctx.clone());

    // Tự kiểm tra bản mới của app (GitHub Releases) — thread nền, popup hỏi
    // trước khi tải. Chế độ script (--serve-only) không cần.
    if !serve_only {
        update::check_in_background(ctx.userdata.clone(), ctx.debug);
    }

    if serve_only {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    let script = ctx.boot_script();
    // Always land on the status page: it says what was found, installs in the
    // background if needed and moves to the renderer itself the moment it is
    // ready. Going straight to `login.html` left a first paint with no status
    // at all — and no way back when the renderer itself stalled.
    let url = format!("http://127.0.0.1:{}/__olaz/index.html", running.port);
    // UA Chromium 108 — Electron 22.3.9 của Zalo PC. `getBrowserName()` của
    // renderer dùng UA để chọn đường mở link: UA Safari/WebKit khiến nó nạp
    // link vào iframe ẩn (openUrlInSandBox) rồi xoá đi — không có gì mở ra. Với
    // "Chrome" trong UA (cùng `window.chrome` shim ở olaz-linux.js) renderer
    // đi đúng đường window.open như bên Electron.
    // CHÚ Ý: đổi UA của webview làm Zalo yêu cầu đăng nhập lại (session gắn
    // môi trường trang) — nếu khôi phục session cũ, có thể tạm hạ về UA mặc
    // định của WebKitGTK.
    let user_agent: &'static str = match std::env::var("OLAZ_WEBVIEW_UA") {
        Ok(value) if !value.is_empty() => Box::leak(value.into_boxed_str()),
        _ => "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/108.0.0.0 Safari/537.36",
    };
    let use_custom_ua = std::env::var("OLAZ_WEBVIEW_UA") != Ok("default".to_string());
    // Electron created a hidden `shared-worker.html` window next to the main one;
    // it runs the start-up cloud scan/offline-sync tasks and owns the
    // SecureLocalStorage that needs the viewer key. Without it the renderer's
    // sync UI reports "Worker is not ready" and cached messages stay empty.
    let worker_url = if ctx.asar.lock().unwrap().is_some() {
        Some(format!(
            "http://127.0.0.1:{}{}",
            running.port,
            ctx.bridge.page_url("/app/shared-worker.html")
        ) + "&already_login=y")
    } else {
        None
    };

    tauri::Builder::default()
        .setup(move |app| {
            // Window-control IPC (`minimize`, `toggle-maximize`, …) and
            // notification clicks drive the real window through this handle.
            ctx.bridge.set_app_handle(app.handle().clone());
            // Popup nội bộ (photo/video viewer): Electron mở BrowserWindow con
            // qua window.open rồi chính renderer nạp popup-viewer.html và tự
            // đổ dữ liệu vào (cùng origin). Tauri tái hiện bằng đường
            // NewWindowResponse::Create — window.open trả về WindowProxy thật,
            // mọi logic phía renderer của multi-window manager chạy như cũ.
            let popup_app = app.handle().clone();
            let popup_script = script.clone();
            let popup_prefix = format!("http://127.0.0.1:{}", running.port);
            let popup_ua = if use_custom_ua { Some(user_agent) } else { None };
            let popup_seq = std::sync::atomic::AtomicU64::new(1);
            let mut main_window = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(url.parse()?),
            )
            .title("Olaz")
            .inner_size(1100.0, 720.0)
            .min_inner_size(760.0, 520.0);
            if use_custom_ua {
                main_window = main_window.user_agent(user_agent);
            }
            main_window
                .initialization_script(&script)
                .on_new_window(move |url, features| {
                    let raw = url.as_str();
                    eprintln!("[popup] on_new_window: {raw}");
                    if raw.starts_with(&popup_prefix) {
                        let label = format!(
                            "popup-{}",
                            popup_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        );
                        // Nhãn truyền qua URL query: popup-viewer.html tự chèn
                        // `/__olaz/boot.js` bằng thẻ script trong trang (không
                        // qua initialization_script của webview — related view
                        // bỏ qua chúng), nên shim đọc nhãn từ location.search.
                        let popup_url = if url.query().is_some() {
                            format!("{}&__olazWin={}", raw, label)
                        } else {
                            format!("{}?__olazWin={}", raw, label)
                        };
                        let parsed: tauri::Url = match popup_url.parse() {
                            Ok(parsed) => parsed,
                            Err(error) => {
                                eprintln!("[popup] URL lỗi: {error}");
                                return tauri::webview::NewWindowResponse::Deny;
                            }
                        };
                        let mut popup = tauri::WebviewWindowBuilder::new(
                            &popup_app,
                            label,
                            tauri::WebviewUrl::External(parsed),
                        )
                        .title("Olaz viewer")
                        .inner_size(760.0, 480.0)
                        .initialization_script(&popup_script)
                        .with_related_view(features.opener().webview.clone());
                        if let Some(size) = features.size() {
                            popup = popup.inner_size(size.width, size.height);
                        }
                        if let Some(pos) = features.position() {
                            popup = popup.position(pos.x, pos.y);
                        }
                        if let Some(ua) = popup_ua {
                            popup = popup.user_agent(ua);
                        }
                        return match popup.build() {
                            Ok(window) => tauri::webview::NewWindowResponse::Create { window },
                            Err(error) => {
                                eprintln!("[popup] tạo cửa sổ lỗi: {error}");
                                tauri::webview::NewWindowResponse::Deny
                            }
                        };
                    }
                    // Link ngoài: WebKitGTK không handler "create" thì chỉ im
                    // lặng — mở bằng trình duyệt hệ thống như
                    // `NSWorkspace.shared.open` trên macOS
                    // (OfficialClientHost.decidePolicyFor).
                    if let Some(sanitized) = crate::bridge::external_url(raw) {
                        crate::bridge::open_external(&sanitized);
                    }
                    tauri::webview::NewWindowResponse::Deny
                })
                .build()?;
            // `window.open` từ script (không user gesture) mặc định bị WebKitGTK
            // chặn — media viewer mở popup trong hiệu ứng React sau click, nên
            // cần bật JavaScript-can-open-windows-automatically cho mọi webview.
            use tauri::Manager as _;
            let enable_script_windows = |window: &tauri::WebviewWindow| {
                let _ = window.with_webview(|platform| {
                    use webkit2gtk::{SettingsExt, WebViewExt};
                    if let Some(settings) = platform.inner().settings() {
                        settings.set_javascript_can_open_windows_automatically(true);
                    }
                });
            };
            enable_script_windows(app.get_webview_window("main").as_ref().unwrap());
            if let Some(worker) = app.get_webview_window("shared-worker") {
                enable_script_windows(&worker);
            }
            if let Some(worker_url) = &worker_url {
                let mut worker_window = tauri::WebviewWindowBuilder::new(
                    app,
                    "shared-worker",
                    tauri::WebviewUrl::External(worker_url.parse()?),
                )
                .title("Olaz worker")
                .visible(false)
                .inner_size(1.0, 1.0);
                if use_custom_ua {
                    worker_window = worker_window.user_agent(user_agent);
                }
                worker_window.initialization_script(&script).build()?;
            }
            // Photos that predate the cache-stem fix get their `Cache/` variants here,
            // off the UI thread. The walk is idempotent and skips complete entries.
            let zalo_pc = host::userdata_dir().join("ZaloPC");
            std::thread::spawn(move || crate::download::seed_existing_caches(&zalo_pc));
            // Started after both windows exist; the watcher is on by default
            // in debug builds and only touches <userData>/dev.
            if ctx.dev_eval_enabled() {
                host::start_dev_harness(ctx.clone());
            }
            // Tray icon (khay hệ thống / AppIndicator — GNOME cần extension
            // AppIndicator, máy này có sẵn): đóng cửa sổ thì app vẫn chạy nền
            // với icon; menu "Mở Olaz" mở lại, "Thoát" kết thúc hẳn.
            use tauri::menu::{CheckMenuItem, Menu, MenuItem};
            use tauri::tray::TrayIconBuilder;
            use tauri::Manager;
            let open_item = MenuItem::with_id(app, "tray-open", "Mở Olaz", true, None::<&str>)?;
            let autostart_item = CheckMenuItem::with_id(
                app,
                "tray-autostart",
                "Khởi động cùng hệ thống",
                true,
                host::autostart_enabled(),
                None::<&str>,
            )?;
            let quit_item = MenuItem::with_id(app, "tray-quit", "Thoát", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&open_item, &autostart_item, &quit_item])?;
            let mut tray = TrayIconBuilder::with_id("olaz-tray")
                .menu(&tray_menu)
                .tooltip("Olaz")
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "tray-open" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.unminimize();
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "tray-autostart" => {
                        // Trạng thái chuẩn là file XDG autostart; sync lại item
                        // (AppIndicator tự toggle hiển thị trước khi event tới).
                        let enable = !host::autostart_enabled();
                        host::set_autostart(enable);
                        if let Some(kind) = app.menu().and_then(|menu| menu.get("tray-autostart")) {
                            if let tauri::menu::MenuItemKind::Check(item) = kind {
                                let _ = item.set_checked(enable);
                            }
                        }
                    }
                    "tray-quit" => app.exit(0),
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
            Ok(())
        })
        // Đóng cửa sổ (X của titlebar renderer hoặc `close-window` IPC) chỉ
        // ẩn — app chạy nền với icon tray như Zalo PC. Thoát hẳn qua menu tray.
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("olaz-linux: Tauri không khởi động được");
}
