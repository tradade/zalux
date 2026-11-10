//! Fetching Zalo PC's renderer (`app.asar`) from VNG — the Linux counterpart of
//! `Sources/Olaz/ZaloRuntime/ZaloRuntimeInstaller.swift`.
//!
//! The macOS build mounts the pinned DMG with `hdiutil` and checks VNG's Apple
//! code signature with `SecStaticCodeCheckValidity`. Linux has neither, so:
//!
//! * the DMG is unpacked with 7-Zip (p7zip-full), which reads HFS+ images and
//!   Windows NSIS installers alike;
//! * the trust anchor is the pinned **SHA-256** of the installer (and, for a
//!   local installer, `OLAZ_INSTALLER_SHA256` when given) — not a signature;
//! * whatever comes out must be a real Zalo renderer: the archive is parsed and
//!   must contain `pc-dist/login.html`, so a wrong or truncated download is
//!   rejected before it is installed.
//!
//! The installed layout matches macOS: `<userData>/ZaloRuntime/Contents/Resources/app.asar`,
//! which `host::find_asar` already prefers.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

use sha2::{Digest, Sha256};

/// The Zalo PC release this build is tested with (`ZaloRuntime.pinned` on macOS).
pub const PINNED_VERSION: &str = "26.10.20";
pub const PINNED_URL: &str = "https://res-download-pc.zadn.vn/mac/ZaloSetup-universal-26.10.20.dmg";
pub const PINNED_SHA256: &str = "a45d30b7f7b060f76b87722ef419b8f41d23a2913cdef084dcb0955dde8d85a0";
/// zalo.me/pc redirects a Mac browser to the DMG it currently offers.
const LATEST_LINK: &str = "https://zalo.me/download/zalo-pc";
const SAFARI_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

pub struct Installed {
    pub asar: PathBuf,
    pub version: String,
}

pub fn root(userdata: &Path) -> PathBuf {
    userdata.join("ZaloRuntime")
}

pub fn current_asar(userdata: &Path) -> PathBuf {
    root(userdata).join("Contents/Resources/app.asar")
}

/// A newer runtime downloaded by a previous launch. It is promoted before the
/// renderer is opened, so an update never replaces the archive under a running
/// page.
pub fn pending_root(userdata: &Path) -> PathBuf {
    root(userdata).join("pending")
}

fn read_version(base: &Path) -> Option<String> {
    fs::read_to_string(base.join("version"))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

pub fn installed_version(userdata: &Path) -> String {
    read_version(&root(userdata)).unwrap_or_else(|| PINNED_VERSION.to_string())
}

/// One line of the startup check, mirrored to the diagnostics page so a GUI
/// launch says *why* there is no renderer yet.
#[derive(Clone)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    pub fn json(&self) -> Value {
        json!({ "name": self.name, "ok": self.ok, "detail": self.detail })
    }
}

/// Whether a missing runtime may be installed without being asked.
pub fn auto_fetch() -> bool {
    std::env::var("OLAZ_AUTO_FETCH").map(|value| value != "0").unwrap_or(true)
}

/// Whether an installed runtime may be upgraded in the background.
pub fn auto_update() -> bool {
    auto_fetch() && std::env::var("OLAZ_AUTO_UPDATE").map(|value| value != "0").unwrap_or(true)
}

/// `26.11.10` is newer than `26.10.20`: compared component-wise as numbers.
pub fn is_newer(version: &str, than: &str) -> bool {
    let a: Vec<u64> = version.split('.').map(|part| part.parse().unwrap_or(0)).collect();
    let b: Vec<u64> = than.split('.').map(|part| part.parse().unwrap_or(0)).collect();
    for index in 0..a.len().max(b.len()) {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        if left != right {
            return left > right;
        }
    }
    false
}

/// Adopts a runtime another launch downloaded into `pending`. Runs before the
/// asar is opened, so promotion is a rename, not a rewrite under a live page.
/// A pending archive that is unreadable, not a renderer, or not newer than the
/// installed one is discarded instead of trusted.
pub fn promote_pending(userdata: &Path) -> Option<Installed> {
    let pending = pending_root(userdata);
    let pending_asar = pending.join("Contents/Resources/app.asar");
    if !pending_asar.is_file() {
        return None;
    }
    let Some(version) = read_version(&pending) else {
        eprintln!("[olaz] runtime: bản chờ thiếu version — bỏ");
        let _ = fs::remove_dir_all(&pending);
        return None;
    };
    let current_version = installed_version(userdata);
    let valid = crate::asar::Asar::open(&pending_asar)
        .map(|archive| archive.contains("pc-dist/login.html"))
        .unwrap_or(false);
    if !valid {
        eprintln!("[olaz] runtime: bản chờ Zalo {version} không phải renderer đọc được — bỏ");
        let _ = fs::remove_dir_all(&pending);
        return None;
    }
    if !is_newer(&version, &current_version) {
        eprintln!("[olaz] runtime: bản chờ Zalo {version} không mới hơn {current_version} — bỏ");
        let _ = fs::remove_dir_all(&pending);
        return None;
    }

    let destination = current_asar(userdata);
    if let Some(parent) = destination.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            eprintln!("[olaz] runtime: không tạo được {}: {error}", parent.display());
            return None;
        }
    }
    // Same filesystem and no live reader: rename is atomic. Fall back to a
    // copy only when the runtime folder sits on another mount.
    if let Err(rename_error) = fs::rename(&pending_asar, &destination) {
        if let Err(copy_error) = fs::copy(&pending_asar, &destination) {
            eprintln!(
                "[olaz] runtime: không chuyển được bản chờ Zalo {version} ({rename_error}; {copy_error})"
            );
            return None;
        }
    }
    fs::write(root(userdata).join("version"), format!("{version}\n")).ok();
    let _ = fs::remove_dir_all(&pending);
    eprintln!("[olaz] runtime: dùng Zalo {version} đã tải ở lần chạy trước (thay {current_version})");
    Some(Installed { asar: destination, version })
}

fn extractor_tool() -> Option<&'static str> {
    ["7z", "7zz", "7za"].into_iter().find(|tool| {
        Command::new(tool)
            .arg("i")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// The startup check: what is on disk, what would unpack the installer, where
/// the installer comes from, and whether a missing runtime installs itself.
/// Cheap enough to run again from the diagnostics page.
pub fn preflight(userdata: &Path) -> Vec<Check> {
    let target = current_asar(userdata);
    let renderer = match crate::asar::Asar::open(&target) {
        Ok(archive) => Check {
            name: "renderer Zalo",
            ok: true,
            detail: format!(
                "Zalo {} · {} file · {}",
                installed_version(userdata),
                archive.count(),
                target.display()
            ),
        },
        Err(_) if target.is_file() => Check {
            name: "renderer Zalo",
            ok: false,
            detail: format!("app.asar đã cài nhưng không đọc được: {}", target.display()),
        },
        Err(_) => Check {
            name: "renderer Zalo",
            ok: false,
            detail: "chưa cài — chưa có app.asar".to_string(),
        },
    };
    let extractor = match extractor_tool() {
        Some(tool) => Check { name: "7-Zip", ok: true, detail: tool.to_string() },
        None => Check {
            name: "7-Zip",
            ok: false,
            detail: "thiếu — cần p7zip-full để giải nén DMG/EXE (sudo apt install p7zip-full)".to_string(),
        },
    };
    let source = match std::env::var("OLAZ_INSTALLER") {
        Ok(path) if !path.trim().is_empty() => {
            let ok = Path::new(&path).is_file();
            Check {
                name: "bộ cài",
                ok,
                detail: if ok {
                    format!("cục bộ: {path}")
                } else {
                    format!("OLAZ_INSTALLER không trỏ tới file nào: {path}")
                },
            }
        }
        _ => Check {
            name: "bộ cài",
            ok: true,
            detail: format!("tải từ VNG: Zalo {PINNED_VERSION} (323 MB)"),
        },
    };
    let fetch = Check {
        name: "tự cài khi thiếu",
        ok: true,
        detail: if auto_fetch() { "bật".to_string() } else { "tắt (OLAZ_AUTO_FETCH=0)".to_string() },
    };
    vec![renderer, extractor, source, fetch]
}

/// Uses the installed runtime if it is readable, otherwise installs one.
/// `Ok(None)` means "no runtime, and fetching is switched off".
pub fn ensure(userdata: &Path, debug: bool) -> Result<Option<Installed>, String> {
    let target = current_asar(userdata);
    if target.is_file() {
        if crate::asar::Asar::open(&target).is_ok() {
            return Ok(Some(Installed { version: installed_version(userdata), asar: target }));
        }
        eprintln!("[olaz] app.asar đã cài không đọc được — cài lại");
        let _ = fs::remove_file(&target);
    }
    if !auto_fetch() {
        eprintln!("[olaz] OLAZ_AUTO_FETCH=0 — không tự tải runtime");
        return Ok(None);
    }
    install(userdata, debug).map(Some)
}

/// Always installs (used by `--fetch-runtime`): local installer when
/// `OLAZ_INSTALLER` is set, otherwise the pinned download.
pub fn install(userdata: &Path, debug: bool) -> Result<Installed, String> {
    let base = root(userdata);
    let work = base.join("work");
    fs::create_dir_all(&work).map_err(|e| format!("không tạo được {}: {e}", work.display()))?;

    let local = std::env::var("OLAZ_INSTALLER").ok().filter(|value| !value.trim().is_empty());
    let source = match local {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_file() {
                return Err(format!("OLAZ_INSTALLER không trỏ tới file nào: {}", path.display()));
            }
            eprintln!("[olaz] dùng bộ cài cục bộ: {}", path.display());
            if let Ok(expected) = std::env::var("OLAZ_INSTALLER_SHA256") {
                let digest = sha256_file(&path)?;
                if !digest.eq_ignore_ascii_case(expected.trim()) {
                    return Err(format!("bộ cài không khớp OLAZ_INSTALLER_SHA256 (nhận {digest})"));
                }
            }
            path
        }
        None => download_release(&work, debug)?,
    };

    let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let version = version_from_installer_name(&name).unwrap_or_else(|| PINNED_VERSION.to_string());
    let installed = install_into(&source, &base, &work, &version);
    let _ = fs::remove_dir_all(&work);
    installed
}

/// Downloads the newest runtime zalo.me offers, when it is newer than the one
/// installed, into `pending`; the next launch promotes it. Nothing is replaced
/// while the app is open, and a failed download leaves the working runtime as is.
/// Returns the version staged for the next launch.
pub fn update_if_newer(userdata: &Path, debug: bool, installed: &str) -> Result<Option<String>, String> {
    if !auto_update() {
        return Ok(None);
    }
    let (url, version) = latest_release()?;
    if !is_newer(&version, installed) {
        if debug {
            eprintln!("[olaz] runtime: Zalo {installed} đã là bản mới nhất");
        }
        return Ok(None);
    }
    // Do not replace a newer pending build left by an earlier launch.
    if let Some(staged) = read_version(&pending_root(userdata)) {
        if !is_newer(&version, &staged) {
            return Ok(None);
        }
    }

    eprintln!(
        "[olaz] runtime: có Zalo {version} (đang dùng {installed}) — tải ở nền, dùng từ lần chạy sau"
    );
    let base = pending_root(userdata);
    let work = root(userdata).join("work-update");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).map_err(|e| format!("không tạo được {}: {e}", work.display()))?;
    let target = work.join(format!("ZaloSetup-{version}.dmg"));
    download(&url, &target, debug)?;
    // Replace an older pending tree only after the new installer arrived.
    let _ = fs::remove_dir_all(&base);
    let result = install_into(&target, &base, &work, &version);
    let _ = fs::remove_dir_all(&work);
    match result {
        Ok(installed) => {
            eprintln!(
                "[olaz] runtime: Zalo {} sẵn sàng, dùng từ lần chạy sau",
                installed.version
            );
            Ok(Some(installed.version))
        }
        Err(error) => Err(error),
    }
}

fn install_into(source: &Path, base: &Path, work: &Path, version: &str) -> Result<Installed, String> {
    let extract = work.join("extract");
    let _ = fs::remove_dir_all(&extract);
    fs::create_dir_all(&extract).map_err(|e| format!("không tạo được {}: {e}", extract.display()))?;
    extract_archive(source, &extract)?;

    let asar = find_asar(&extract).ok_or_else(|| {
        "bộ cài không chứa app.asar — cần bản Zalo PC cho macOS (.dmg) hoặc Windows (.exe)".to_string()
    })?;
    let archive = crate::asar::Asar::open(&asar)?;
    if !archive.contains("pc-dist/login.html") {
        return Err("app.asar tìm thấy không phải renderer Zalo PC (thiếu pc-dist/login.html)".into());
    }

    let resources = base.join("Contents/Resources");
    fs::create_dir_all(&resources).map_err(|e| format!("không tạo được {}: {e}", resources.display()))?;
    let destination = resources.join("app.asar");
    fs::copy(&asar, &destination).map_err(|e| format!("không copy được app.asar: {e}"))?;
    fs::write(base.join("version"), format!("{version}\n")).ok();

    eprintln!(
        "[olaz] đã cài renderer Zalo {version}: {} file, {} MB → {}",
        archive.count(),
        archive_bytes(&destination) / (1024 * 1024),
        destination.display()
    );
    Ok(Installed { asar: destination, version: version.to_string() })
}

fn archive_bytes(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// 7-Zip reads DMG (HFS+), NSIS installers and plain archives.
fn extract_archive(source: &Path, destination: &Path) -> Result<(), String> {
    let mut last = String::new();
    for tool in ["7z", "7zz", "7za"] {
        let output = Command::new(tool)
            .arg("x")
            .arg("-y")
            .arg(format!("-o{}", destination.display()))
            .arg(source)
            .output();
        match output {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                // 7-Zip exits 2 for warnings: a DMG's `/Applications` symlink is
                // "Dangerous link path was ignored", which does not matter here.
                // The archive is good if the tree already holds the renderer.
                if find_asar(destination).is_some() {
                    return Ok(());
                }
                let stderr = String::from_utf8_lossy(&output.stderr);
                last = format!("{tool}: {}", stderr.lines().last().unwrap_or("").trim());
            }
            Err(error) => last = format!("{tool}: {error}"),
        }
    }
    Err(format!(
        "không giải nén được {} ({last}) — cần p7zip-full (sudo apt install p7zip-full)",
        source.display()
    ))
}

fn find_asar(dir: &Path) -> Option<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name != "app.asar.unpacked" {
                    stack.push(path);
                }
            } else if entry.file_name() == "app.asar" {
                found.push(path);
            }
        }
    }
    found.sort_by_key(|path| (!path.to_string_lossy().contains("Contents/Resources")) as u8);
    found.into_iter().next()
}

/// `ZaloSetup-universal-26.10.20.dmg` / `ZaloSetup-26.10.20.exe` → `26.10.20`.
pub fn version_from_installer_name(name: &str) -> Option<String> {
    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name);
    let tail = stem.rsplit('-').next()?;
    let parts: Vec<&str> = tail.split('.').collect();
    if parts.len() < 2 || !parts.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    Some(tail.to_string())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("không mở được {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 4 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(|e| format!("đọc {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

fn download_release(work: &Path, debug: bool) -> Result<PathBuf, String> {
    let target = work.join(format!("ZaloSetup-{PINNED_VERSION}.dmg"));
    match download(PINNED_URL, &target, debug) {
        Ok(()) => {
            let digest = sha256_file(&target)?;
            if digest != PINNED_SHA256 {
                return Err(format!(
                    "bộ cài Zalo {PINNED_VERSION} không khớp SHA-256 (nhận {digest}) — không dùng"
                ));
            }
            eprintln!("[olaz] SHA-256 khớp bản ghim {PINNED_VERSION}");
            Ok(target)
        }
        Err(error) => {
            eprintln!("[olaz] không tải được bản ghim {PINNED_VERSION}: {error}");
            if std::env::var("OLAZ_ALLOW_LATEST").map(|value| value == "0").unwrap_or(false) {
                return Err(error);
            }
            let (url, version) = latest_release()?;
            let target = work.join(format!("ZaloSetup-{version}.dmg"));
            download(&url, &target, debug)?;
            eprintln!("[olaz] dùng bản zalo.me đang phát hành: {version} (không có SHA-256 để đối chiếu)");
            Ok(target)
        }
    }
}

fn download(url: &str, target: &Path, debug: bool) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60 * 30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let mut response = client
        .get(url)
        .header("user-agent", SAFARI_UA)
        .send()
        .map_err(|e| format!("tải {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("máy chủ tải Zalo trả về HTTP {}", response.status().as_u16()));
    }
    let total = response.content_length();
    let mut file = fs::File::create(target).map_err(|e| format!("không tạo được {}: {e}", target.display()))?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut received: u64 = 0;
    let mut next_report: u64 = 0;
    loop {
        let read = response.read(&mut buffer).map_err(|e| format!("đọc dữ liệu tải: {e}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read]).map_err(|e| format!("ghi {}: {e}", target.display()))?;
        received += read as u64;
        if received >= next_report {
            let total_text = total.map(|t| format!("{} MB", t / (1 << 20))).unwrap_or_else(|| "?".into());
            eprintln!("[olaz] đang tải bộ cài Zalo … {} MB / {total_text}", received / (1 << 20));
            next_report = received + (10 << 20);
        }
    }
    file.flush().ok();
    if debug {
        eprintln!("[olaz] tải xong {received} byte từ {url}");
    }
    Ok(())
}

/// The DMG zalo.me offers right now, read from its redirect.
fn latest_release() -> Result<(String, String), String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let response = client
        .head(LATEST_LINK)
        .header("user-agent", SAFARI_UA)
        .send()
        .map_err(|e| format!("hỏi zalo.me: {e}"))?;
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .ok_or("zalo.me không chuyển hướng tới bộ cài")?;
    let url = if location.starts_with("http") {
        location.to_string()
    } else {
        format!("https://zalo.me{}", if location.starts_with('/') { location.to_string() } else { format!("/{location}") })
    };
    let name = url.rsplit('/').next().unwrap_or("");
    let version = version_from_installer_name(name).ok_or_else(|| format!("không đọc được phiên bản từ {url}"))?;
    Ok((url, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal app.asar with one pc-dist/login.html entry, enough for the
    /// renderer check `promote_pending` performs.
    fn write_runtime(base: &Path, body: &[u8]) -> Vec<u8> {
        let header = json!({
            "files": { "pc-dist": { "files": {
                "login.html": { "size": body.len(), "offset": "0" }
            }}}
        });
        let json = serde_json::to_vec(&header).unwrap();
        let header_pickle = 8 + json.len();
        let mut blob = Vec::new();
        blob.extend_from_slice(&4u32.to_le_bytes());
        blob.extend_from_slice(&(header_pickle as u32).to_le_bytes());
        blob.extend_from_slice(&((4 + json.len()) as u32).to_le_bytes());
        blob.extend_from_slice(&(json.len() as u32).to_le_bytes());
        blob.extend_from_slice(&json);
        blob.extend_from_slice(body);
        let resources = base.join("Contents/Resources");
        fs::create_dir_all(&resources).unwrap();
        fs::write(resources.join("app.asar"), &blob).unwrap();
        blob
    }

    #[test]
    fn versions_compare_componentwise() {
        assert!(is_newer("26.11.10", "26.10.20"));
        assert!(is_newer("26.10.20", "26.9.10"));
        assert!(is_newer("27", "26.11.10"));
        assert!(!is_newer("26.10.20", "26.10.20"));
        assert!(!is_newer("26.10.20", "26.11.0"));
    }

    #[test]
    fn promotes_a_newer_pending_runtime() {
        let userdata = std::env::temp_dir().join(format!("olaz-promote-{}", std::process::id()));
        let _ = fs::remove_dir_all(&userdata);
        write_runtime(&root(&userdata), b"old");
        fs::write(root(&userdata).join("version"), "26.10.20\n").unwrap();
        write_runtime(&pending_root(&userdata), b"new");
        fs::write(pending_root(&userdata).join("version"), "26.11.10\n").unwrap();

        let promoted = promote_pending(&userdata).expect("bản chờ mới hơn phải được dùng");
        assert_eq!(promoted.version, "26.11.10");
        assert_eq!(read_version(&root(&userdata)).as_deref(), Some("26.11.10"));
        assert!(!pending_root(&userdata).exists());
        let archive = crate::asar::Asar::open(&promoted.asar).unwrap();
        assert_eq!(archive.read("pc-dist/login.html").unwrap(), b"new");
        fs::remove_dir_all(&userdata).ok();
    }

    #[test]
    fn discards_a_pending_runtime_that_is_not_newer() {
        let userdata = std::env::temp_dir().join(format!("olaz-pending-old-{}", std::process::id()));
        let _ = fs::remove_dir_all(&userdata);
        write_runtime(&root(&userdata), b"current");
        fs::write(root(&userdata).join("version"), "26.11.10\n").unwrap();
        write_runtime(&pending_root(&userdata), b"stale");
        fs::write(pending_root(&userdata).join("version"), "26.10.20\n").unwrap();

        assert!(promote_pending(&userdata).is_none());
        assert_eq!(read_version(&root(&userdata)).as_deref(), Some("26.11.10"));
        assert!(!pending_root(&userdata).exists());
        fs::remove_dir_all(&userdata).ok();
    }

    #[test]
    fn installer_names_carry_the_version() {
        assert_eq!(version_from_installer_name("ZaloSetup-universal-26.10.20.dmg").as_deref(), Some("26.10.20"));
        assert_eq!(version_from_installer_name("ZaloSetup-26.10.20.exe").as_deref(), Some("26.10.20"));
        assert_eq!(version_from_installer_name("ZaloSetup-25.1.dmg").as_deref(), Some("25.1"));
        assert_eq!(version_from_installer_name("app.asar"), None);
        assert_eq!(version_from_installer_name("ZaloSetup-universal-latest.dmg"), None);
    }

    #[test]
    fn sha256_matches_known_digest() {
        let path = std::env::temp_dir().join(format!("olaz-sha-{}", std::process::id()));
        fs::write(&path, b"olaz").unwrap();
        // SHA-256("olaz"), same value `bridge::hash_digest` produces.
        assert_eq!(
            sha256_file(&path).unwrap(),
            "bb35ddce85c5be15a5a9a9c908ddc462db84f24422bf0e4175e77c0dae5d4a54"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn preflight_lists_the_startup_checks() {
        let dir = std::env::temp_dir().join(format!("olaz-preflight-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let checks = preflight(&dir);
        let names: Vec<&str> = checks.iter().map(|check| check.name).collect();
        assert_eq!(names, ["renderer Zalo", "7-Zip", "bộ cài", "tự cài khi thiếu"]);
        assert!(!checks[0].ok, "thư mục rỗng thì chưa có renderer");
        assert!(checks[0].json()["detail"].as_str().unwrap().contains("chưa cài"));
        assert!(checks[1].json()["detail"].is_string());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finds_the_asar_inside_the_installer_tree() {
        let dir = std::env::temp_dir().join(format!("olaz-tree-{}", std::process::id()));
        let nested = dir.join("Zalo.app/Contents/Resources");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("app.asar"), b"x").unwrap();
        fs::create_dir_all(dir.join("Zalo.app/Contents/Resources/app.asar.unpacked")).unwrap();
        assert_eq!(find_asar(&dir).unwrap(), nested.join("app.asar"));
        fs::remove_dir_all(&dir).ok();
    }
}
