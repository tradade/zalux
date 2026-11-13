//! Olaz cho Linux — vỏ Tauri quanh renderer chính thức của Zalo PC.
//!
//! `--serve-only` chạy mỗi loopback bridge (không mở cửa sổ), dùng cho kiểm thử
//! và cho các script bên ngoài; mặc định thì mở cửa sổ WebKitGTK.

mod asar;
mod bridge;
mod download;
mod fingerprint;
mod host;
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
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("olaz-linux: Tauri không khởi động được");
}
