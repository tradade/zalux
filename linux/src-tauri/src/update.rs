//! Tự kiểm tra bản mới của app trên GitHub Releases (`linux-v*`), hiện popup
//! hỏi cập nhật, tải AppImage về, thay thế và khởi động lại.
//!
//! Luồng: thread nền (check sau 30s rồi mỗi 12h) → `releases/latest` → bản mới
//! hơn tag nhúng trong binary (`APP_RELEASE_TAG`, đặt bởi CI) và chưa bị skip
//! → popup GTK → "Cập nhật ngay": tải về + đối chiếu SHA-256 từ `SHA256SUMS`
//! → rename thay `$APPIMAGE` (nguyên tử, instance đang chạy giữ inode cũ) →
//! spawn instance mới với `OLAZ_REPLACE_INSTANCE=1` (tự đóng phiên cũ qua
//! single-instance) → thoát. Chạy bản .deb (không có `$APPIMAGE`) → chỉ mở
//! trang tải về. "Bỏ qua phiên này" lưu vào `<userdata>/app-update.json` và
//! áp cho đúng tag đó; bản mới hơn vẫn hỏi.
//!
//! Bản dev (không có tag nhúng) hoặc `OLAZ_UPDATE_CHECK=0` → tắt hẳn. Lỗi
//! mạng/API im lặng (chỉ log stderr).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

/// Khoảng nghỉ trước lần check đầu (nhường băng thông cho runtime download).
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
const GITHUB_API: &str = "https://api.github.com/repos";

/// Tag release nhúng lúc build (CI đặt `APP_RELEASE_TAG`); rỗng = bản dev.
pub fn release_tag() -> &'static str {
    option_env!("APP_RELEASE_TAG").unwrap_or("")
}

#[derive(Debug, PartialEq)]
struct Release {
    tag: String,
    /// URL tải asset AppImage của release.
    appimage_url: Option<String>,
    /// URL `SHA256SUMS` (tuỳ chọn — có thì đối chiếu SHA-256).
    sums_url: Option<String>,
    /// Trang release (mở bằng xdg-open khi chạy bản .deb).
    html_url: String,
}

pub fn check_in_background(userdata: PathBuf, debug: bool) {
    if release_tag().is_empty() {
        eprintln!("[update] bản dev (không nhúng APP_RELEASE_TAG) — bỏ qua kiểm tra cập nhật");
        return;
    }
    if std::env::var("OLAZ_UPDATE_CHECK").map(|value| value == "0").unwrap_or(false) {
        return;
    }
    let spawn = std::thread::Builder::new().name("app-update".into()).spawn(move || {
        std::thread::sleep(FIRST_CHECK_DELAY);
        loop {
            match check_once(&userdata, debug) {
                Ok(()) => {}
                // App vừa tự restart (process exit) hoặc lỗi — lỗi chỉ log.
                Err(error) => eprintln!("[update] {error}"),
            }
            std::thread::sleep(CHECK_INTERVAL);
        }
    });
    if spawn.is_err() {
        eprintln!("[update] không tạo được thread kiểm tra");
    }
}

enum Choice {
    Update,
    Skip,
    Later,
}

fn check_once(userdata: &Path, debug: bool) -> Result<(), String> {
    let repo = std::env::var("OLAZ_UPDATE_REPO").unwrap_or_else(|_| "tradade/zalux".into());
    let Some(release) = latest_release(&repo)? else {
        return Ok(());
    };
    let current = release_tag();
    if version_of(&release.tag) == version_of(current) {
        return Ok(());
    }
    if skipped_tag(userdata).as_deref() == Some(release.tag.as_str()) {
        if debug {
            eprintln!("[update] {} đã bị bỏ qua bởi người dùng", release.tag);
        }
        return Ok(());
    }
    let appimage_mode = std::env::var_os("APPIMAGE").is_some();
    if debug {
        eprintln!(
            "[update] phát hiện bản mới {} (hiện tại {}) — hỏi người dùng",
            release.tag, current
        );
    }
    match ask_update(current, &release.tag, appimage_mode) {
        Choice::Later => Ok(()),
        Choice::Skip => {
            set_skipped_tag(userdata, &release.tag);
            Ok(())
        }
        Choice::Update => {
            if !appimage_mode {
                // Bản .deb: cần quyền root để thay binary — mở trang tải về.
                let _ = std::process::Command::new("xdg-open")
                    .arg(&release.html_url)
                    .spawn();
                return Ok(());
            }
            let appimage_url = release
                .appimage_url
                .ok_or_else(|| "release mới không có asset AppImage".to_string())?;
            let staged = download_and_stage(userdata, &appimage_url, release.sums_url.as_deref(), &release.tag, debug)?;
            apply_and_restart(&staged)
        }
    }
}

/// `releases/latest` của repo; `None` nếu bản mới nhất không phải tag `linux-v*`.
fn latest_release(repo: &str) -> Result<Option<Release>, String> {
    let value = github_get(&format!("{GITHUB_API}/{repo}/releases/latest"))?;
    let Some(tag) = value.get("tag_name").and_then(Value::as_str) else {
        return Err("release không có tag_name".into());
    };
    if !tag.starts_with("linux-v") {
        return Ok(None);
    }
    let mut appimage_url = None;
    let mut sums_url = None;
    for asset in value.get("assets").and_then(Value::as_array).unwrap_or(&vec![]) {
        let name = asset.get("name").and_then(Value::as_str).unwrap_or("");
        let url = asset.get("browser_download_url").and_then(Value::as_str).unwrap_or("");
        if name.ends_with(".AppImage") {
            appimage_url = Some(url.to_string());
        } else if name == "SHA256SUMS" {
            sums_url = Some(url.to_string());
        }
    }
    let html_url = value
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or("https://github.com/tradade/zalux/releases")
        .to_string();
    Ok(Some(Release { tag: tag.to_string(), appimage_url, sums_url, html_url }))
}

fn github_get(url: &str) -> Result<Value, String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("http client: {error}"))?;
    let mut response = client
        .get(url)
        .header("user-agent", "olaz-linux")
        .header("accept", "application/vnd.github+json")
        .send()
        .map_err(|error| format!("gọi {url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} trả về HTTP {}", response.status().as_u16()));
    }
    let mut text = String::new();
    response
        .read_to_string(&mut text)
        .map_err(|error| format!("đọc {url}: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("JSON từ {url}: {error}"))
}

fn fetch_text(url: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| format!("http client: {error}"))?;
    let mut response = client
        .get(url)
        .header("user-agent", "olaz-linux")
        .send()
        .map_err(|error| format!("tải {url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} trả về HTTP {}", response.status().as_u16()));
    }
    let mut text = String::new();
    response.read_to_string(&mut text).map_err(|error| format!("đọc {url}: {error}"))?;
    Ok(text)
}

// MARK: popup

enum Ask {
    Confirmed,
    Declined,
}

fn ask_update(current: &str, latest: &str, appimage_mode: bool) -> Choice {
    use gtk::prelude::*;
    use std::sync::mpsc;
    let (sender, receiver) = mpsc::channel();
    let current = current.to_string();
    let latest = latest.to_string();
    gtk::glib::MainContext::default().invoke(move || {
        let action_text = if appimage_mode { "Cập nhật ngay" } else { "Mở trang tải về" };
        let dialog = gtk::MessageDialog::builder()
            .message_type(gtk::MessageType::Question)
            .title("Cập nhật Olaz")
            .text(format!("Có phiên bản mới: {latest}"))
            .secondary_text(format!(
                "Phiên hiện tại: {current}\n{}",
                if appimage_mode {
                    "Cập nhật ngay không? App sẽ tải về và tự khởi động lại."
                } else {
                    "Bản .deb cần cài lại bằng quyền root — mở trang tải về?"
                }
            ))
            .build();
        dialog.add_button("Bỏ qua phiên này", gtk::ResponseType::No);
        dialog.add_button(action_text, gtk::ResponseType::Ok);
        dialog.set_default_response(gtk::ResponseType::Ok);
        let response = dialog.run();
        dialog.close();
        let _ = sender.send(response);
    });
    match receiver.recv_timeout(Duration::from_secs(3600)) {
        Ok(gtk::ResponseType::Ok) => Choice::Update,
        Ok(gtk::ResponseType::No) => Choice::Skip,
        _ => Choice::Later,
    }
}

// MARK: download + restart

fn download_and_stage(
    userdata: &Path,
    url: &str,
    sums_url: Option<&str>,
    tag: &str,
    debug: bool,
) -> Result<PathBuf, String> {
    let dir = userdata.join("app-update/pending");
    std::fs::create_dir_all(&dir).map_err(|error| format!("tạo {}: {error}", dir.display()))?;
    let file_name = url.rsplit('/').next().unwrap_or("Olaz.AppImage");
    let target = dir.join(file_name);
    crate::runtime::download(url, &target, debug)?;
    // Đối chiếu SHA-256 từ SHA256SUMS của release (có mới kiểm, không có thì
    // chấp nhận — HTTPS đã bảo vệ đường truyền).
    if let Some(sums_url) = sums_url {
        match fetch_text(sums_url) {
            Ok(sums) => match sha_from_sums(&sums, file_name) {
                Some(expected) => {
                    let actual = crate::runtime::sha256_file(&target)?;
                    if actual != expected {
                        let _ = std::fs::remove_file(&target);
                        return Err(format!(
                            "{file_name} không khớp SHA-256 (nhận {actual}) — xoá, không cập nhật"
                        ));
                    }
                    eprintln!("[update] SHA-256 khớp cho {file_name}");
                }
                None => eprintln!("[update] SHA256SUMS không có dòng cho {file_name} — bỏ qua đối chiếu"),
            },
            Err(error) => eprintln!("[update] không tải được SHA256SUMS: {error}"),
        }
    }
    Ok(target)
}

/// `"abc123  Olaz_1.0.0_amd64.AppImage"` → `Some("abc123")` (sha256sum format).
fn sha_from_sums(sums: &str, file_name: &str) -> Option<String> {
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        let name = parts.next()?;
        let name = name.trim_start_matches('*');
        if name == file_name && digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(digest.to_ascii_lowercase());
        }
    }
    None
}

/// Thay AppImage rồi khởi động lại: rename nguyên tử (instance đang chạy giữ
/// inode cũ), spawn instance mới với `OLAZ_REPLACE_INSTANCE=1` — nó tự đóng
/// phiên này qua cơ chế single-instance — rồi thoát.
fn apply_and_restart(staged: &Path) -> Result<(), String> {
    let Some(appimage) = std::env::var_os("APPIMAGE").map(PathBuf::from) else {
        return Err("không có $APPIMAGE — không tự thay được".into());
    };
    std::fs::rename(staged, &appimage).map_err(|error| {
        format!(
            "không thay được {} bằng {}: {error}",
            appimage.display(),
            staged.display()
        )
    })?;
    eprintln!("[update] đã thay AppImage — khởi động lại");
    std::process::Command::new(&appimage)
        .env("OLAZ_REPLACE_INSTANCE", "1")
        .spawn()
        .map_err(|error| format!("không khởi động được instance mới: {error}"))?;
    // Nhịp cho instance mới kịp start (nó sẽ tự đóng phiên này).
    std::thread::sleep(Duration::from_secs(2));
    std::process::exit(0);
}

// MARK: skip per-version

fn skip_file(userdata: &Path) -> PathBuf {
    userdata.join("app-update.json")
}

fn skipped_tag(userdata: &Path) -> Option<String> {
    let data = std::fs::read_to_string(skip_file(userdata)).ok()?;
    let value: Value = serde_json::from_str(&data).ok()?;
    value.get("skipped_tag").and_then(Value::as_str).map(str::to_string)
}

fn set_skipped_tag(userdata: &Path, tag: &str) {
    let data = json!({ "skipped_tag": tag });
    let _ = std::fs::write(skip_file(userdata), data.to_string());
    eprintln!("[update] người dùng bỏ qua {tag}");
}

/// `linux-v1.0.4` → `1.0.4` (so sánh bằng `runtime::is_newer`).
fn version_of(tag: &str) -> &str {
    tag.strip_prefix("linux-v").unwrap_or(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_line_matches_the_asset_name() {
        let sums = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  Olaz_1.0.0_amd64.AppImage\n\
                    0123  Olaz_1.0.0_amd64.deb\n";
        assert_eq!(
            sha_from_sums(sums, "Olaz_1.0.0_amd64.AppImage"),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into())
        );
        assert_eq!(sha_from_sums(sums, "khac.AppImage"), None);
        // dòng checksum giả (ngắn) bị bỏ
        assert_eq!(sha_from_sums("0123  Olaz_1.0.0_amd64.AppImage\n", "Olaz_1.0.0_amd64.AppImage"), None);
    }

    #[test]
    fn version_strips_the_linux_v_prefix() {
        assert_eq!(version_of("linux-v1.0.4"), "1.0.4");
        assert_eq!(version_of("1.0.4"), "1.0.4");
    }

    #[test]
    fn newer_tag_wins_and_equal_tag_does_not() {
        assert!(crate::runtime::is_newer(version_of("linux-v1.0.4"), version_of("linux-v1.0.3")));
        assert!(!crate::runtime::is_newer(version_of("linux-v1.0.4"), version_of("linux-v1.0.4")));
        assert!(crate::runtime::is_newer(version_of("linux-v1.1.0"), version_of("linux-v1.0.9")));
    }

    #[test]
    fn skip_file_roundtrips() {
        let dir = std::env::temp_dir().join(format!("olaz-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(skipped_tag(&dir), None);
        set_skipped_tag(&dir, "linux-v1.0.4");
        assert_eq!(skipped_tag(&dir).as_deref(), Some("linux-v1.0.4"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dev_build_without_tag_is_detected() {
        // release_tag() chỉ phụ thuộc compile-time env; test hành vi rỗng.
        let dev = release_tag().is_empty();
        if dev {
            assert_eq!(version_of(release_tag()), "");
        }
    }
}
