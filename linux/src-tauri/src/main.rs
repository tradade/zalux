//! Olaz cho Linux — vỏ Tauri quanh renderer chính thức của Zalo PC.
//!
//! `--serve-only` chạy mỗi loopback bridge (không mở cửa sổ), dùng cho kiểm thử
//! và cho các script bên ngoài; mặc định thì mở cửa sổ WebKitGTK.

mod asar;
mod bridge;
mod call;
mod download;
mod fingerprint;
mod host;
mod instance;
mod net;
mod runtime;

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
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url.parse()?))
                .title("Olaz")
                .inner_size(1100.0, 720.0)
                .min_inner_size(760.0, 520.0)
                .initialization_script(&script)
                .build()?;
            if let Some(worker_url) = &worker_url {
                tauri::WebviewWindowBuilder::new(
                    app,
                    "shared-worker",
                    tauri::WebviewUrl::External(worker_url.parse()?),
                )
                .title("Olaz worker")
                .visible(false)
                .inner_size(1.0, 1.0)
                .initialization_script(&script)
                .build()?;
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
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::TrayIconBuilder;
            use tauri::Manager;
            let open_item = MenuItem::with_id(app, "tray-open", "Mở Olaz", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "tray-quit", "Thoát", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&open_item, &quit_item])?;
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
