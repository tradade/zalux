//! Single-instance: phát hiện một Olaz khác đang chạy (cổng bridge cố định
//! 27182 bị chiếm) và hỏi người dùng có đóng phiên cũ để mở phiên mới không.
//!
//! Cổng bridge là định danh origin (IndexedDB/localStorage khoá theo
//! scheme+host+port — xem `host::start`), nên hai instance không thể chạy cùng
//! lúc. Trước đây lần mở sau chỉ thoát với một dòng stderr (mờ nhạt khi mở từ
//! GUI — trông như "app không chạy được"); giờ có dialog hỏi rõ.

use std::path::Path;
use std::time::Duration;

/// Cổng bridge mặc định — trùng `DEFAULT_PORT` của `host.rs`, overridable bằng
/// `OLAZ_PORT` (phải khớp với `host::start`, nếu không detect nhìn nhầm cổng).
pub fn bridge_port() -> u16 {
    std::env::var("OLAZ_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or(27_182)
}

/// Có một instance khác đang nghe cổng bridge? Trả về PID của nó khi xác định
/// được là tiến trình olaz-linux (không bao giờ đụng tiến trình lạ chiếm cổng).
pub fn detect(port: u16) -> Option<u32> {
    if !port_busy(port) {
        return None;
    }
    find_olaz_pid()
}

/// Có thật một olaz-linux khác đang nghe cổng? (kiểm tra trước khi dialog:
/// PID tìm được phải khớp exe để SIGTERM không bắn nhầm).
pub fn port_busy(port: u16) -> bool {
    use std::net::TcpStream;
    let Ok(addr) = format!("127.0.0.1:{port}").parse() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Quét /proc: tiến trình nào exe là `olaz-linux` (AppImage mount hoặc
/// target/debug|release) và không phải mình.
fn find_olaz_pid() -> Option<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return None;
    };
    let me = std::process::id();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == me || !is_olaz_pid(pid) {
            continue;
        }
        return Some(pid);
    }
    None
}

/// `/proc/<pid>/exe` trỏ tới binary `olaz-linux` (cùng user nên đọc được link).
fn is_olaz_pid(pid: u32) -> bool {
    let link = format!("/proc/{pid}/exe");
    match std::fs::read_link(&link) {
        Ok(target) => {
            let name = target.file_name().map(|name| name.to_string_lossy().into_owned());
            name.as_deref() == Some("olaz-linux")
        }
        Err(_) => false,
    }
}

/// Đóng instance cũ: SIGTERM (Tauri dừng sạch, socket đóng), chờ tối đa 5s,
/// rồi SIGKILL nếu còn. Trả true khi cổng đã giải phóng.
pub fn terminate(pid: u32) -> bool {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    for _ in 0..50 {
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    // Cho SIGKILL một nhịp để socket được đóng trước khi bind lại.
    for _ in 0..20 {
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !port_busy(bridge_port())
}

/// Dialog hỏi người dùng: true = "Đóng phiên cũ & mở mới", false = Huỷ.
/// `OLAZ_REPLACE_INSTANCE=1` bỏ qua dialog (dùng cho script/kiểm thử).
pub fn ask_replace(old_pid: u32) -> bool {
    if std::env::var("OLAZ_REPLACE_INSTANCE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return true;
    }
    ask_replace_dialog(old_pid)
}

fn ask_replace_dialog(old_pid: u32) -> bool {
    use gtk::prelude::*;
    if gtk::init().is_err() {
        eprintln!(
            "[olaz] không mở được dialog (không có màn hình?) — thoát. Đóng phiên Olaz cũ \
             (PID {old_pid}) rồi mở lại, hoặc đặt OLAZ_REPLACE_INSTANCE=1."
        );
        return false;
    }
    let dialog = gtk::MessageDialog::builder()
        .message_type(gtk::MessageType::Question)
        .title("Olaz")
        .text("Olaz đang chạy")
        .secondary_text(format!(
            "Đã có một cửa sổ Olaz đang hoạt động (PID {old_pid}).\nMở phiên mới sẽ đóng phiên cũ — dữ liệu giữ nguyên vì cùng profile."
        ))
        .build();
    dialog.add_button("Huỷ", gtk::ResponseType::Cancel);
    dialog.add_button("Đóng phiên cũ & mở mới", gtk::ResponseType::Ok);
    dialog.set_default_response(gtk::ResponseType::Ok);
    let replace = dialog.run() == gtk::ResponseType::Ok;
    dialog.close();
    drop(dialog);
    replace
}
