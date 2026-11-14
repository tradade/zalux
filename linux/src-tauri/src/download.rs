//! Download manager: plays `main-dist`'s `new-download-channel` (class `GYN9`).
//!
//! The official renderer never fetches media itself. `CloudService.downloadWithMultiSrc`
//! — the photo loader `loadPC`, video, voice, file downloads, and the Zalo Cloud
//! fallback behind them — calls `$zdownload.sendDownloadRequest({channel, data})`
//! and waits for `{channel: "new-download-file-resp", data}` pushed back on the
//! same channel. Electron main answered that; without it every download promise
//! stays pending, so a synced conversation shows text but no image and nothing
//! ever lands in `ZaloDownloads`.
//!
//! Contract, from `Sources/Olaz/ZaloRuntime/DownloadManager.swift`:
//!   request  new-download-file        data = {key, downloadFrom, type,
//!                                            data{url, fileName},
//!                                            options{saveDir, headers, maxTTFB, srcAction},
//!                                            extraHeader, secure}
//!            new-download-file-abort  data = {key, downloadFrom}
//!   response new-download-file-resp   data = {downloadError|null, savePath,
//!                                            downloadFrom, key, type, isCached}
//!   errors   {name, code, normalize: true}; HTTP s → DOWNLOAD_SERVICE_NET_s / 75000 + s.
//!
//! Writes are atomic (temp file in the destination folder, then rename), so a
//! failed or aborted transfer can never leave a half-written image behind.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use reqwest::cookie::CookieStore;
use serde_json::{json, Value};

use crate::host::Events;

/// Untouched downloads (video, voice, files) stream to disk under this cap.
const FILE_CAP: u64 = 2 << 30;
/// Body of an encrypted download must be in memory to be decrypted.
const SECURE_CAP: u64 = 256 << 20;

/// In-flight downloads, keyed `downloadFrom|key`, so an abort can reach one.
fn inflight() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    static INFLIGHT: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
    INFLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `$zdownload.sendDownloadRequest` — `args` is the request object itself.
pub fn request(
    events: Arc<Events>,
    jar: Arc<reqwest::cookie::Jar>,
    user_agent: String,
    debug: bool,
    args: &Value,
) {
    let channel = args
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let data = args.get("data").cloned().unwrap_or_else(|| json!({}));
    match channel {
        "new-download-file" => start(events, jar, user_agent, debug, data),
        "new-download-file-abort" => abort(&data),
        // concurrency / quota / response-cache config, ping, scan, priority:
        // nothing to schedule here, the renderer gets its answer synchronously.
        _ => {}
    }
}

fn abort(data: &Value) {
    let id = format!("{}|{}", text(&data["downloadFrom"]), text(&data["key"]));
    if let Some(flag) = inflight().lock().unwrap().get(&id) {
        flag.store(true, Ordering::Relaxed);
    }
}

fn start(
    events: Arc<Events>,
    jar: Arc<reqwest::cookie::Jar>,
    user_agent: String,
    debug: bool,
    request: Value,
) {
    let key = request.get("key").cloned().unwrap_or(Value::Null);
    let from = text(&request["downloadFrom"]);
    let kind = text(&request["type"]);
    let id = format!("{from}|{}", text(&key));
    if debug {
        eprintln!(
            "[download] {kind} from={from} key={} url={}",
            text(&key),
            text(&request["data"]["url"])
        );
    }
    let cancel = Arc::new(AtomicBool::new(false));
    inflight()
        .lock()
        .unwrap()
        .insert(id.clone(), cancel.clone());

    std::thread::spawn(move || {
        let url = text(&request["data"]["url"]);
        let outcome = seed_photo_cache(
            transfer(&request, &jar, &user_agent, debug, &cancel),
            &kind,
            &url,
        );
        if debug {
            match (&outcome.save_path, &outcome.failure) {
                (Some(path), _) => eprintln!(
                    "[download] {kind} key={} saved={path} cached={}",
                    text(&key),
                    outcome.cached
                ),
                (None, Some(err)) => {
                    eprintln!("[download] {kind} key={} failed: {err}", text(&key))
                }
                _ => {}
            }
        }
        {
            let mut map = inflight().lock().unwrap();
            let current = map.get(&id);
            if current
                .map(|flag| Arc::ptr_eq(flag, &cancel))
                .unwrap_or(false)
            {
                map.remove(&id);
            }
        }
        if let Some(path) = &outcome.save_path {
            *last_saved().lock().unwrap() = Some(path.clone());
        }
        let data = json!({
            "downloadError": outcome.failure.unwrap_or(Value::Null),
            "savePath": outcome.save_path.clone().map(Value::String).unwrap_or(Value::Null),
            "downloadFrom": from,
            "key": key,
            "type": kind,
            "isCached": outcome.cached,
        });
        events.push(json!({
            "kind": "send",
            "channel": "new-download-channel",
            "args": [{ "channel": "new-download-file-resp", "data": data }],
        }));
    });
}

/// Đường dẫn file lần tải gần nhất (media cache) — `show-save-dialog` dùng
/// làm tên gợi ý khi renderer gửi `defaultPath` rỗng (media item của viewer
/// không mang fileName/url như bên Electron).
fn last_saved() -> &'static std::sync::Mutex<Option<String>> {
    static LAST: std::sync::OnceLock<std::sync::Mutex<Option<String>>> = std::sync::OnceLock::new();
    LAST.get_or_init(|| std::sync::Mutex::new(None))
}

/// Path lần tải gần nhất (nếu có).
pub fn last_saved_path() -> Option<String> {
    last_saved().lock().unwrap().clone()
}

/// JPEG XL codestream (`FF 0A`) or container (`....JXL ` box).
pub(crate) fn is_jxl(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0x0A]) || (bytes.len() > 12 && &bytes[4..8] == b"JXL ")
}

/// Decodes a JPEG XL image (`jxl-oxide`) and re-encodes it as baseline JPEG.
///
/// The renderer decodes message photos through the native `zjxl` addon; Linux
/// answers that namespace with the host bridge instead (see `bridge.member`
/// `$olaz.jxl.decode`), so this is the shared path for both the download
/// manager and the renderer's photo component.
pub(crate) fn jxl_to_jpeg(bytes: &[u8], quality: u8) -> Option<Vec<u8>> {
    let decoder = jxl_oxide::integration::JxlDecoder::new(std::io::Cursor::new(bytes)).ok()?;
    let picture = image::DynamicImage::from_decoder(decoder).ok()?;
    let mut jpeg = Vec::new();
    let mut encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality.clamp(1, 100));
    encoder.encode_image(&picture).ok()?;
    Some(jpeg)
}

/// The qualities a message photo can carry, keyed the way the message JSON spells
/// them. The bubble probes one cache path per key; they do not share a name shape.
pub(crate) const PHOTO_QUALITIES: [&str; 4] = ["thumbUrl", "normalUrl", "hdUrl", "oriUrl"];

/// The extension the renderer appends to a cached variant name.
///
/// `getName` builds `<cli>_<from>_<to>_<quality><ext>` where `ext` is
/// `path.extname(removeProtocol(url))` run through `.replace(/[^\.a-zA-Z0-9].+$/, "")`
/// — so a photo served from `https://…/abc.jpg` is probed as `…_t.jpg`, never as
/// `…_t`. Naming the cache entry without the extension is why a downloaded photo
/// still never reached its bubble.
pub(crate) fn url_extension(url: &str) -> String {
    // `path.extname` works on the basename, and a query/fragment is not part of it.
    let path = url.split(['?', '#']).next().unwrap_or("");
    let base = path.rsplit('/').next().unwrap_or("");
    let ext = match base.rfind('.') {
        // A leading dot is a dotfile, not an extension.
        Some(dot) if dot > 0 => &base[dot..],
        _ => "",
    };
    // What is left after the renderer's `.replace(/[^\.a-zA-Z0-9].+$/, "")`: keep the
    // extension only when every character in it is one the regex lets through.
    if ext.len() > 1 && ext[1..].chars().all(|c| c.is_ascii_alphanumeric()) {
        ext.to_string()
    } else {
        String::new()
    }
}

/// md5 of the URL, hex — the half of a `picture/` name the renderer derives from it.
fn url_md5(url: &str) -> String {
    use md5::Digest as _;
    let digest = md5::Md5::digest(url.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The path the renderer probes for one quality of a message photo.
///
/// The two families do **not** share a shape (`compact-app-pc`, module `MbQE`):
///
/// * thumbnail — `thumb` locator, `Cache/` subdir:
///   `<resource>/Cache/<cli>_<from>_<to>_t<ext(url)>`
///   (`E = ${stem}_${Wkux.b(quality)}${ext}`, `getZaloDownloadsPictureDirSync(conv, me, "Cache")`)
/// * every other quality — `d` locator, `picture/` dir, URL folded into an md5:
///   `<resource>/picture/<cli>_<from>_<to>_<md5(url)><ext(url)>`
///   (`_ = ${stem}_${md5(url)}${ext}`, plain `getZaloDownloadsPictureDirSync(conv, me)`)
///
/// Writing all four into `Cache/` under the thumbnail's shape — what this wrapper did —
/// matches only `_t`; the `_n`/`_h`/`_o` files were never looked at.
pub(crate) fn photo_variant_path(resource: &Path, stem: &str, quality: &str, url: &str) -> PathBuf {
    let ext = url_extension(url);
    if quality == "thumbUrl" {
        return resource.join("Cache").join(format!("{stem}_t{ext}"));
    }
    resource
        .join("picture")
        .join(format!("{stem}_{}{ext}", url_md5(url)))
}

/// Every cache path for one photo: one entry per quality the message carries.
///
/// `urls` is `(quality, url)` in the order the message spells them; a quality the
/// message does not carry simply has no cache file, exactly as in the official client.
pub(crate) fn photo_variant_paths(
    resource: &Path,
    stem: &str,
    urls: &[(String, String)],
) -> Vec<PathBuf> {
    urls.iter()
        .map(|(quality, url)| photo_variant_path(resource, stem, quality, url))
        .collect()
}

/// Bytes safe to put under a cache name the `<img>` will load: a decoded JPEG for
/// JPEG XL, the original bytes for a format WebKitGTK renders natively, and `None`
/// for anything else.
///
/// A CDN link that expired answers `200` with an HTML error page, and a video
/// thumbnail request can answer with the MP4 itself. Both used to land in `Cache/`
/// as the thumbnail, so the bubble got a file that never decodes.
pub(crate) fn photo_payload(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.is_empty() {
        return None;
    }
    if is_jxl(bytes) {
        return jxl_to_jpeg(bytes, 88);
    }
    // Magic bytes only: a full decode of a 20 MB original per download is not worth
    // it, and a body starting with a JPEG/PNG/GIF/WEBP signature is that format.
    let fits = bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(b"\x89PNG")
        || bytes.starts_with(b"GIF8")
        || (bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP");
    fits.then(|| bytes.to_vec())
}

/// The official photo bubble loads its image from cached variants next to the download:
/// `<resource>/Cache/<stem>_t` (thumb), `_n`, `_h`, `_o`, written by the native gen-thumb /
/// libvips path that does not exist on Linux. Without them `fs.accessSync` answers ENOENT and
/// the bubble keeps its blurred LQIP placeholder even though the photo itself downloaded fine.
/// Writing the decoded JPEG under each name gives the component a loadable file for whichever
/// quality it asks for; the download's own path is left untouched.
/// Writes every cache variant this wrapper can derive for a downloaded photo.
///
/// The `picture/` file the download manager just wrote already *is* one of the quality
/// entries — but its name carries a server checksum where the renderer expects
/// `md5(url)`, so it does not count. The download request carries the source URL, and
/// that is enough to satisfy the thumbnail (and the original when it is the same URL);
/// the other qualities need their own CDN URL, which arrives separately.
fn seed_photo_cache(outcome: Outcome, kind: &str, url: &str) -> Outcome {
    if !(kind.starts_with("Photo") || kind == "image") || url.is_empty() {
        return outcome;
    }
    let Some(save) = outcome.save_path.as_deref().map(Path::new) else {
        return outcome;
    };
    write_cache_variants(save, url);
    outcome
}

/// Writes the cache entries for one downloaded photo. Returns whether a variant landed.
fn write_cache_variants(save: &Path, url: &str) -> bool {
    let Some(resource) = save.parent().and_then(Path::parent) else {
        return false;
    };
    let Some(stem) = save.file_stem().and_then(|stem| stem.to_str()).map(cache_stem) else {
        return false;
    };
    // The file the downloader saved is named after the URL it fetched, so the quality is
    // whichever URL matches. Both spellings the renderer looks for are written when the
    // URL is the thumbnail URL; a non-thumb URL only satisfies the `picture/` entry.
    let urls = quality_urls(stem, url);
    let paths = photo_variant_paths(resource, stem, &urls);
    if paths.iter().all(|path| path.is_file()) {
        return true;
    }
    let Ok(bytes) = std::fs::read(save) else {
        return false;
    };
    let Some(payload) = photo_payload(&bytes) else {
        // Not an image (an HTML error page, a video, a document): leave the cache
        // alone so the component keeps its own placeholder instead of a broken file.
        return false;
    };
    if let Some(dir) = paths.first().and_then(|path| path.parent()) {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut wrote = false;
    for path in &paths {
        wrote |= std::fs::write(path, &payload).is_ok();
    }
    wrote
}

/// `(quality, url)` pairs safe to materialise from a single downloaded URL.
///
/// The thumbnail name is `<stem>_t<ext>`, independent of the URL, so the fetched bytes
/// always satisfy it. The other quality names embed `md5(url)`, so they are only correct
/// for the URL actually downloaded.
fn quality_urls(_stem: &str, url: &str) -> Vec<(String, String)> {
    vec![
        ("thumbUrl".to_string(), url.to_string()),
        ("oriUrl".to_string(), url.to_string()),
    ]
}

/// Writes the cache entries for one photo from bytes in memory.
///
/// `urls` is `(quality, url)` for each quality the message carries, in message order.
/// The bubble loads exactly one file per quality; this wrapper's downloader is the only
/// thing that can produce them, so the decoder and the naming live in one place.
pub(crate) fn write_photo_cache(
    resource: &Path,
    stem: &str,
    urls: &[(String, String)],
    bytes: &[u8],
) -> Option<Vec<PathBuf>> {
    if stem.is_empty() || urls.is_empty() {
        return None;
    }
    let payload = photo_payload(bytes)?;
    let paths = photo_variant_paths(resource, stem, urls);
    for dir in paths.iter().filter_map(|path| path.parent()) {
        std::fs::create_dir_all(dir).ok()?;
    }
    for path in &paths {
        std::fs::write(path, &payload).ok()?;
    }
    Some(paths)
}

/// Photos that downloaded before the cache stem was fixed have no `Cache/` entry, so their
/// bubble stays blurred. Walk every `<userData>/ZaloPC/<uid>/ZaloDownloads/resource/<conv>
/// /picture/` file once at start-up and materialise the missing variants.
pub(crate) fn seed_existing_caches(zalo_pc: &Path) {
    let Ok(users) = std::fs::read_dir(zalo_pc) else {
        return;
    };
    for user in users.flatten() {
        let resource = user.path().join("ZaloDownloads").join("resource");
        let Ok(convs) = std::fs::read_dir(&resource) else {
            continue;
        };
        for conv in convs.flatten() {
            let Ok(files) = std::fs::read_dir(conv.path().join("picture")) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                // A `picture/` file already carries the quality name the renderer probes
                // for its non-thumbnail quality (the md5 half comes from the URL, which
                // only the message row knows). Nothing to materialise from the file
                // alone, so this walk only repairs the old extension-less spelling.
                if path.is_file() {
                    archive_legacy_cache_variants(&path);
                }
            }
        }
    }
}

/// Rewrites cache entries older builds left extension-less.
///
/// Before the URL extension was carried, every variant was written as
/// `<resource>/Cache/<stem>_<quality>`. Those names are never probed, so a photo that
/// only ever had one keeps a blurred bubble. The thumbnail spelling is derivable from
/// the file itself; it is written alongside and the stale name is dropped.
fn archive_legacy_cache_variants(save: &Path) -> bool {
    let Some(resource) = save.parent().and_then(Path::parent) else {
        return false;
    };
    let Some(stem) = save.file_stem().and_then(|stem| stem.to_str()).map(cache_stem) else {
        return false;
    };
    let Some(quality) = stem_quality(stem) else {
        return false;
    };
    let ext = save
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
        .unwrap_or_default();
    let cache = resource.join("Cache");
    let legacy = cache.join(format!("{stem}_{quality}"));
    if legacy.is_file() {
        return true;
    }
    let Ok(bytes) = std::fs::read(save) else {
        return false;
    };
    let Some(payload) = photo_payload(&bytes) else {
        return false;
    };
    let _ = std::fs::create_dir_all(&cache);
    std::fs::write(cache.join(format!("{stem}_{quality}{ext}")), &payload).is_ok()
}

/// The quality letter a `<stem>_<letter>` cache name ends in, if it has one.
fn stem_quality(stem: &str) -> Option<&'static str> {
    let letter = stem.rsplit('_').next()?;
    match letter {
        "t" => Some("t"),
        "n" => Some("n"),
        "h" => Some("h"),
        "o" => Some("o"),
        _ => None,
    }
}

/// The renderer probes `Cache/<msgId>_<fromUid>_<toUid>_<quality>` (no checksum), but the
/// downloaded file is named `<msgId>_<fromUid>_<toUid>_<checksum>.jpg` — so the trailing
/// checksum segment has to go before the variant names match.
fn cache_stem(stem: &str) -> &str {
    match stem.rsplit_once('_') {
        Some((head, tail))
            if tail.len() >= 16 && tail.chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            head
        }
        _ => stem,
    }
}

/// Pixel size of a JPEG XL image, for the renderer's `getJxlInfo` call. A photo that
/// already arrived as JPEG (the CDN does that for `jxlstatus=1`) is measured with the
/// same decoder instead of failing the call.
pub(crate) fn jxl_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if !is_jxl(bytes) {
        let picture = image::load_from_memory(bytes).ok()?;
        return Some((picture.width(), picture.height()));
    }
    let decoder = jxl_oxide::integration::JxlDecoder::new(std::io::Cursor::new(bytes)).ok()?;
    let picture = image::DynamicImage::from_decoder(decoder).ok()?;
    Some((picture.width(), picture.height()))
}

struct Outcome {
    save_path: Option<String>,
    failure: Option<Value>,
    cached: bool,
}

impl Outcome {
    fn saved(path: &Path) -> Self {
        Self {
            save_path: Some(path.to_string_lossy().into_owned()),
            failure: None,
            cached: false,
        }
    }

    fn cached(path: &Path) -> Self {
        Self {
            save_path: Some(path.to_string_lossy().into_owned()),
            failure: None,
            cached: true,
        }
    }

    fn fail(name: &str, code: u64) -> Self {
        Self {
            save_path: None,
            failure: Some(failure(name, code, None)),
            cached: false,
        }
    }

    fn fail_msg(name: &str, code: u64, message: &str) -> Self {
        Self {
            save_path: None,
            failure: Some(failure(name, code, Some(message))),
            cached: false,
        }
    }

    fn aborted() -> Self {
        Self::fail("GENERAL_MAIN_CONTROLLER_OBJ_DOWNLOAD_ABORT", 1009)
    }

    fn invalid_url() -> Self {
        Self::fail("GENERAL_UI_OBJ_DOWNLOAD_INVALID_URL", 9)
    }

    fn bad_save_dir() -> Self {
        Self::fail(
            "GENERAL_MAIN_CONTROLLER_OBJ_DOWNLOAD_SAVE_DIR_INVALID",
            1012,
        )
    }

    fn empty_body() -> Self {
        Self::fail(
            "DOWNLOAD_SERVICE_SINGLE_INVALID_CONTENT_LENGTH",
            75000 + 5002,
        )
    }

    fn too_large() -> Self {
        Self::fail(
            "DOWNLOAD_SERVICE_SINGLE_FILE_SIZE_LIMIT_EXCEED",
            75000 + 5026,
        )
    }

    fn generic() -> Self {
        Self::fail(
            "DOWNLOAD_SERVICE_SINGLE_DOWNLOAD_ON_EMPTY_ERR",
            75000 + 5025,
        )
    }

    fn http(status: u16) -> Self {
        Self::fail(
            &format!("DOWNLOAD_SERVICE_NET_{status}"),
            75000 + status as u64,
        )
    }
}

fn failure(name: &str, code: u64, message: Option<&str>) -> Value {
    let mut out = json!({ "name": name, "code": code, "normalize": true });
    if let Some(message) = message {
        out["message"] = json!(message);
    }
    out
}

fn transfer(
    request: &Value,
    jar: &Arc<reqwest::cookie::Jar>,
    user_agent: &str,
    debug: bool,
    cancel: &AtomicBool,
) -> Outcome {
    let data = &request["data"];
    let options = &request["options"];
    let kind = text(&request["type"]);

    let Some(raw_url) = data.get("url").and_then(Value::as_str) else {
        return Outcome::invalid_url();
    };
    let Ok(url) = reqwest::Url::parse(raw_url) else {
        return Outcome::invalid_url();
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Outcome::invalid_url();
    }
    let Some(destination) = destination(options, data) else {
        return Outcome::bad_save_dir();
    };
    let Some(parent) = destination.parent() else {
        return Outcome::bad_save_dir();
    };
    if std::fs::create_dir_all(parent).is_err() {
        return Outcome::bad_save_dir();
    }

    // A completed earlier download: atomic writes mean an existing file is whole.
    if usable(&destination) {
        return Outcome::cached(&destination);
    }

    let secure = Secure::parse(&request["secure"]);
    if matches!(secure, Secure::Invalid) {
        return Outcome::fail_msg("INS_OF_ERROR_ERR", 14998, "File secure decrypt failed");
    }

    let ttfb = options.get("maxTTFB").and_then(Value::as_u64).unwrap_or(0);
    let mut builder = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(if ttfb > 0 {
            ttfb.max(5_000)
        } else {
            600_000
        }));
    if ttfb > 0 {
        builder = builder.connect_timeout(Duration::from_millis(ttfb.max(5_000)));
    }
    let Ok(client) = builder.build() else {
        return Outcome::generic();
    };

    if debug {
        eprintln!(
            "[download] {kind} host={} src={} secure={}",
            url.host_str().unwrap_or("-"),
            options
                .get("srcAction")
                .and_then(Value::as_str)
                .unwrap_or("-"),
            if matches!(secure, Secure::None) {
                "no"
            } else {
                "yes"
            }
        );
    }

    if kind.starts_with("Photo") || kind == "image" {
        match download_photo_with_fingerprint(request, &url, jar, user_agent, debug, cancel) {
            Ok(Some(body)) => {
                let plain = match secure.decrypt(&body) {
                    Ok(plain) if plain.is_empty() => return Outcome::empty_body(),
                    Ok(plain) => plain,
                    Err(_) => {
                        return Outcome::fail_msg(
                            "INS_OF_ERROR_ERR",
                            14998,
                            "File secure decrypt failed",
                        )
                    }
                };
                return match write_atomic(&destination, &plain) {
                    Ok(()) => Outcome::saved(&destination),
                    Err(WriteError::TooLarge) => Outcome::too_large(),
                    Err(WriteError::Aborted) => Outcome::aborted(),
                    Err(WriteError::Empty) => Outcome::empty_body(),
                    Err(WriteError::Io(_)) => Outcome::bad_save_dir(),
                };
            }
            Ok(None) => {}
            Err(outcome) => return outcome,
        }
    }

    let photo = kind.starts_with("Photo") || kind == "image";
    let response =
        match send_with_media_redirects(&client, &url, &headers(request), jar, user_agent, photo) {
            Ok(response) => response,
            Err(MediaSendError::Network(error)) => {
                if cancel.load(Ordering::Relaxed) {
                    return Outcome::aborted();
                }
                if debug {
                    eprintln!("[download] {kind} lỗi: {error}");
                }
                let timed_out = error.is_timeout();
                return if timed_out {
                    Outcome::fail("DOWNLOAD_SERVICE_SINGLE_MAX_TTFB_TIMEOUT", 75000 + 5027)
                } else {
                    Outcome::fail("ERR_CONNECTION_FAILED", 30033)
                };
            }
            Err(MediaSendError::InvalidUrl) => return Outcome::invalid_url(),
        };
    let status = response.status();
    if !status.is_success() {
        if debug {
            eprintln!("[download] {kind} HTTP {}", status.as_u16());
        }
        return Outcome::http(status.as_u16());
    }

    let result = if matches!(secure, Secure::None) {
        stream_to_file(response, &destination, cancel)
    } else {
        let body = match read_capped(response, SECURE_CAP, cancel) {
            Ok(bytes) => bytes,
            Err(outcome) => return outcome,
        };
        match secure.decrypt(&body) {
            Ok(plain) if plain.is_empty() => return Outcome::empty_body(),
            Ok(plain) => write_atomic(&destination, &plain),
            Err(_) => {
                return Outcome::fail_msg("INS_OF_ERROR_ERR", 14998, "File secure decrypt failed")
            }
        }
    };
    match result {
        Ok(()) => {
            if debug {
                eprintln!("[download] {kind} → {}", destination.display());
            }
            Outcome::saved(&destination)
        }
        Err(WriteError::Empty) => Outcome::empty_body(),
        Err(WriteError::TooLarge) => Outcome::too_large(),
        Err(WriteError::Aborted) => Outcome::aborted(),
        Err(WriteError::Io(error)) => {
            if debug {
                eprintln!("[download] {kind} ghi lỗi: {error}");
            }
            Outcome::bad_save_dir()
        }
    }
}

enum MediaSendError {
    Network(reqwest::Error),
    InvalidUrl,
}

fn send_with_media_redirects(
    client: &reqwest::blocking::Client,
    url: &reqwest::Url,
    base_headers: &[(String, String)],
    jar: &reqwest::cookie::Jar,
    user_agent: &str,
    photo: bool,
) -> Result<reqwest::blocking::Response, MediaSendError> {
    const MAX_REDIRECTS: usize = 10;
    let mut current = url.clone();
    let mut headers = base_headers.to_vec();

    for redirect in 0..=MAX_REDIRECTS {
        let cookie = jar
            .cookies(&current)
            .and_then(|value| value.to_str().ok().map(str::to_owned));
        let request_headers = media_request_headers(&headers, &current, cookie, user_agent, photo);
        let mut outgoing = client.get(current.clone());
        for (name, value) in request_headers {
            outgoing = outgoing.header(name, value);
        }
        let response = outgoing.send().map_err(MediaSendError::Network)?;
        if !is_redirect_status(response.status().as_u16()) {
            return Ok(response);
        }

        let Some(location) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
        else {
            return Ok(response);
        };
        if redirect == MAX_REDIRECTS {
            return Ok(response);
        }
        let next = current
            .join(location)
            .map_err(|_| MediaSendError::InvalidUrl)?;
        if !matches!(next.scheme(), "http" | "https") || next.host_str().is_none() {
            return Err(MediaSendError::InvalidUrl);
        }
        headers = redirect_media_headers(&headers, &current, &next);
        current = next;
    }

    unreachable!("redirect loop returns the final response at the limit")
}

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn download_photo_with_fingerprint(
    request: &Value,
    url: &reqwest::Url,
    jar: &reqwest::cookie::Jar,
    user_agent: &str,
    debug: bool,
    cancel: &AtomicBool,
) -> Result<Option<Vec<u8>>, Outcome> {
    let Some(helper) = crate::fingerprint::running_helper() else {
        return Ok(None);
    };

    let timeout_ms = request["options"]
        .get("maxTTFB")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(20_000)
        .max(5_000);
    let mut current = url.clone();
    let mut base_headers = headers(request);
    for redirect in 0..=5 {
        let cookie = jar
            .cookies(&current)
            .and_then(|value| value.to_str().ok().map(str::to_owned));
        let wire = crate::fingerprint::Request {
            method: "GET".into(),
            url: current.to_string(),
            headers: media_request_headers(&base_headers, &current, cookie, user_agent, true),
            body: None,
            timeout_ms,
            follow: false,
        };
        let response = match helper.perform_cancelable(&wire, Some(cancel)) {
            Ok(response) => response,
            Err(crate::fingerprint::Error::Unavailable(error)) => {
                if debug {
                    eprintln!("[download] fingerprint helper không sẵn sàng: {error}");
                }
                return Ok(None);
            }
            Err(crate::fingerprint::Error::Failed(error)) => {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Outcome::aborted());
                }
                if debug {
                    eprintln!("[download] Photo lỗi qua fingerprint helper: {error}");
                }
                let lower = error.to_ascii_lowercase();
                return Err(if lower.contains("timeout") {
                    Outcome::fail("DOWNLOAD_SERVICE_SINGLE_MAX_TTFB_TIMEOUT", 75000 + 5027)
                } else if lower.contains("body too large") {
                    Outcome::too_large()
                } else {
                    Outcome::fail("ERR_CONNECTION_FAILED", 30033)
                });
            }
        };

        if is_redirect_status(response.status) {
            let location = response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                .map(|(_, value)| value);
            let Some(location) = location else {
                return Err(Outcome::http(response.status));
            };
            if redirect == 5 {
                return Err(Outcome::http(response.status));
            }
            let Ok(next) = current.join(location) else {
                return Err(Outcome::invalid_url());
            };
            if !matches!(next.scheme(), "http" | "https") || next.host_str().is_none() {
                return Err(Outcome::invalid_url());
            }
            base_headers = redirect_media_headers(&base_headers, &current, &next);
            current = next;
            continue;
        }
        if !(200..300).contains(&response.status) {
            return Err(Outcome::http(response.status));
        }
        return Ok(Some(response.body));
    }

    Err(Outcome::generic())
}

fn media_request_headers(
    base: &[(String, String)],
    url: &reqwest::Url,
    cookie: Option<String>,
    user_agent: &str,
    photo: bool,
) -> Vec<(String, String)> {
    let trusted = is_zalo_host(url);
    let token_chars = "!#$%&'*+-.^_`|~";
    let forbidden = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "host",
        "content-length",
        "expect",
        "accept-encoding",
    ];
    let sensitive = [
        "cloud-viewer-key",
        "cookie",
        "authorization",
        "proxy-authorization",
    ];
    let mut out = Vec::new();
    for (name, value) in base {
        let lower = name.to_ascii_lowercase();
        let valid_name = !name.is_empty()
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || token_chars.contains(ch));
        let valid_value = value
            .chars()
            .all(|ch| (ch >= ' ' && ch != '\u{7f}') || ch == '\t');
        if !valid_name
            || !valid_value
            || forbidden.contains(&lower.as_str())
            || (!trusted && sensitive.contains(&lower.as_str()))
        {
            continue;
        }
        set_header(&mut out, name, value);
    }
    if trusted {
        if !out
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("cookie"))
        {
            if let Some(cookie) = cookie {
                set_header(&mut out, "cookie", &cookie);
            }
        }
    }

    let defaults = [
        ("User-Agent", user_agent),
        (
            "sec-ch-ua",
            "\"Not?A_Brand\";v=\"8\", \"Chromium\";v=\"108\"",
        ),
        ("sec-ch-ua-mobile", "?0"),
        ("sec-ch-ua-platform", "\"macOS\""),
        ("accept-language", "en-US"),
        (
            "Accept",
            if photo {
                "image/webp,image/apng,image/*,*/*;q=0.8"
            } else {
                "*/*"
            },
        ),
        ("sec-fetch-dest", if photo { "image" } else { "empty" }),
        ("sec-fetch-mode", if photo { "no-cors" } else { "cors" }),
        ("sec-fetch-site", "cross-site"),
    ];
    for (name, value) in defaults {
        set_header(&mut out, name, value);
    }
    out
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some(existing) = headers
        .iter_mut()
        .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
    {
        existing.1 = value.to_string();
    } else {
        headers.push((name.to_string(), value.to_string()));
    }
}

fn redirect_media_headers(
    headers: &[(String, String)],
    origin: &reqwest::Url,
    next: &reqwest::Url,
) -> Vec<(String, String)> {
    if same_origin(origin, next) {
        return headers.to_vec();
    }
    headers
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.to_ascii_lowercase().as_str(),
                "cloud-viewer-key" | "cookie" | "authorization" | "proxy-authorization"
            )
        })
        .cloned()
        .collect()
}

fn same_origin(left: &reqwest::Url, right: &reqwest::Url) -> bool {
    left.scheme().eq_ignore_ascii_case(right.scheme())
        && left.host_str().map(str::to_ascii_lowercase)
            == right.host_str().map(str::to_ascii_lowercase)
        && left.port_or_known_default() == right.port_or_known_default()
}

fn is_zalo_host(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    [
        "zadn.vn",
        "zdn.vn",
        "zalo.me",
        "zaloapp.com",
        "zingcdn.me",
        "zalo.vn",
        "zing.vn",
    ]
    .iter()
    .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

enum WriteError {
    Empty,
    TooLarge,
    Aborted,
    Io(std::io::Error),
}

/// Streams the body to a temporary sibling, then renames it into place.
fn stream_to_file(
    mut response: reqwest::blocking::Response,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<(), WriteError> {
    let staged = staged_path(destination);
    let mut file = match std::fs::File::create(&staged) {
        Ok(file) => file,
        Err(error) => return Err(WriteError::Io(error)),
    };
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&staged);
            return Err(WriteError::Aborted);
        }
        let read = match response.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                let _ = std::fs::remove_file(&staged);
                return Err(WriteError::Io(error));
            }
        };
        total += read as u64;
        if total > FILE_CAP {
            let _ = std::fs::remove_file(&staged);
            return Err(WriteError::TooLarge);
        }
        if let Err(error) = file.write_all(&buffer[..read]) {
            let _ = std::fs::remove_file(&staged);
            return Err(WriteError::Io(error));
        }
    }
    drop(file);
    if total == 0 {
        let _ = std::fs::remove_file(&staged);
        return Err(WriteError::Empty);
    }
    publish(&staged, destination).map_err(WriteError::Io)
}

/// Whole body in memory, for a download that has to be decrypted.
fn read_capped(
    mut response: reqwest::blocking::Response,
    cap: u64,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, Outcome> {
    let mut bytes = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Outcome::aborted());
        }
        match response.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                bytes.extend_from_slice(&buffer[..read]);
                if bytes.len() as u64 > cap {
                    return Err(Outcome::too_large());
                }
            }
            Err(_) => return Err(Outcome::generic()),
        }
    }
    Ok(bytes)
}

fn write_atomic(destination: &Path, bytes: &[u8]) -> Result<(), WriteError> {
    let staged = staged_path(destination);
    std::fs::write(&staged, bytes).map_err(WriteError::Io)?;
    publish(&staged, destination).map_err(WriteError::Io)
}

fn publish(staged: &Path, destination: &Path) -> std::io::Result<()> {
    match std::fs::rename(staged, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(staged, destination)?;
            let _ = std::fs::remove_file(staged);
            Ok(())
        }
    }
}

fn staged_path(destination: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    destination.with_file_name(format!(
        ".olaz-{}-{}.part",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn usable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false)
}

/// `options.saveDir/fileName`, name reduced to a single path component.
fn destination(options: &Value, data: &Value) -> Option<PathBuf> {
    let save_dir = options.get("saveDir").and_then(Value::as_str)?;
    if !save_dir.starts_with('/') {
        return None;
    }
    let raw = data.get("fileName").and_then(Value::as_str).unwrap_or("");
    let mut name = Path::new(raw)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    name = name.replace('\0', "");
    if name.is_empty() || name == "." || name == ".." {
        name = "no_name".to_string();
    }
    Some(Path::new(save_dir).join(name))
}

/// `options.headers` [{name, value}] (Zalo Cloud's `cloud-viewer-key`) plus `extraHeader`.
fn headers(request: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(list) = request["options"]["headers"].as_array() {
        for entry in list {
            if let (Some(name), Some(value)) = (entry["name"].as_str(), entry["value"].as_str()) {
                if !name.is_empty() {
                    out.push((name.to_string(), value.to_string()));
                }
            }
        }
    }
    if let Some(map) = request["extraHeader"].as_object() {
        for (name, value) in map {
            if let Some(value) = value.as_str() {
                if !name.is_empty() {
                    out.push((name.clone(), value.to_string()));
                }
            }
        }
    }
    out
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => String::new(),
    }
}

/// Stream decryption main applies to e2ee / Zalo Cloud ("pcloud") downloads:
/// `secure = {type, cryptography: {alg, key, iv, tag?, type: "block"|"linear", blockSize}}`.
/// Any other algorithm or malformed key / iv makes the download fail — ciphertext
/// must never be saved as the media.
enum Secure {
    None,
    Ctr {
        key: Vec<u8>,
        iv: Vec<u8>,
        block: Option<usize>,
    },
    Gcm {
        key: Vec<u8>,
        iv: Vec<u8>,
        tag: Vec<u8>,
    },
    Invalid,
}

impl Secure {
    fn parse(value: &Value) -> Self {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind != "e2ee" && kind != "pcloud" {
            return Secure::None;
        }
        let crypto = &value["cryptography"];
        let (Some(alg), Some(key), Some(iv)) = (
            crypto.get("alg").and_then(Value::as_str),
            bytes(&crypto["key"]),
            bytes(&crypto["iv"]),
        ) else {
            return Secure::Invalid;
        };
        if key.len() != 32 || iv.is_empty() {
            return Secure::Invalid;
        }
        let block = crypto.get("type").and_then(Value::as_str) == Some("block");
        match alg {
            "aes-256-ctr" => {
                if iv.len() != 16 {
                    return Secure::Invalid;
                }
                let size = crypto
                    .get("blockSize")
                    .and_then(Value::as_u64)
                    .unwrap_or(4096)
                    .max(16) as usize;
                Secure::Ctr {
                    key,
                    iv,
                    block: if block { Some(size) } else { None },
                }
            }
            "aes-256-gcm" => {
                if block {
                    return Secure::Invalid;
                }
                let Some(tag) = bytes(&crypto["tag"]) else {
                    return Secure::Invalid;
                };
                if tag.len() != 16 || iv.len() < 12 {
                    return Secure::Invalid;
                }
                Secure::Gcm { key, iv, tag }
            }
            _ => Secure::Invalid,
        }
    }

    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Secure::None => Ok(data.to_vec()),
            Secure::Ctr { key, iv, block } => ctr_xor(key, iv, data, *block),
            Secure::Gcm { key, iv, tag } => gcm_decrypt(key, iv, tag, data),
            Secure::Invalid => Err("invalid".into()),
        }
    }
}

fn bytes(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::String(text) => base64::engine::general_purpose::STANDARD.decode(text).ok(),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(Value::as_u64)
                .map(|byte| byte as u8)
                .collect(),
        ),
        // `HostScripts/bridge.js` serialises every ArrayBuffer/Uint8Array sent
        // through the loopback bridge as `{__bytes: <base64>}`. Media secure
        // descriptors use those values for key/IV/tag, just as the macOS
        // bridge receives native Data. Accept the wire representation instead
        // of silently treating an encrypted download as malformed.
        Value::Object(object) => object
            .get("__bytes")
            .and_then(Value::as_str)
            .and_then(|text| base64::engine::general_purpose::STANDARD.decode(text).ok()),
        _ => None,
    }
}

/// AES-256-CTR (big-endian counter). `block` present: every `block` bytes restart
/// the same counter, and a trailing partial block passes through unchanged.
fn ctr_xor(key: &[u8], iv: &[u8], data: &[u8], block: Option<usize>) -> Result<Vec<u8>, String> {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};

    if iv.len() != 16 {
        return Err("iv".into());
    }
    let mut out = data.to_vec();
    macro_rules! run {
        ($ty:ty) => {{
            let cipher = <$ty>::new_from_slice(key).map_err(|_| "key".to_string())?;
            let span = block.unwrap_or(usize::MAX);
            let mut start = 0usize;
            while start < out.len() {
                let length = span.min(out.len() - start);
                let mut counter = [0u8; 16];
                counter.copy_from_slice(iv);
                let mut offset = 0usize;
                while offset < length {
                    let mut stream = GenericArray::clone_from_slice(&counter);
                    cipher.encrypt_block(&mut stream);
                    let take = 16.min(length - offset);
                    for index in 0..take {
                        out[start + offset + index] ^= stream[index];
                    }
                    offset += take;
                    increment(&mut counter);
                }
                start += length;
            }
        }};
    }
    match key.len() {
        16 => run!(aes::Aes128),
        24 => run!(aes::Aes192),
        32 => run!(aes::Aes256),
        _ => return Err("key".into()),
    }
    Ok(out)
}

fn increment(counter: &mut [u8; 16]) {
    for byte in counter.iter_mut().rev() {
        let (next, carry) = byte.overflowing_add(1);
        *byte = next;
        if !carry {
            break;
        }
    }
}

fn gcm_decrypt(key: &[u8], iv: &[u8], tag: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

    if iv.len() != 12 || tag.len() != 16 {
        return Err("gcm".into());
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "gcm key".to_string())?;
    let mut sealed = Vec::with_capacity(data.len() + tag.len());
    sealed.extend_from_slice(data);
    sealed.extend_from_slice(tag);
    cipher
        .decrypt(
            Nonce::from_slice(iv),
            Payload {
                msg: &sealed,
                aad: &[],
            },
        )
        .map_err(|_| "gcm auth".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_and_original_live_in_two_different_dirs() {
        // `MbQE`: the thumbnail is `<stem>_t<ext>` under `Cache/`; every other quality
        // folds `md5(url)` in and sits under `picture/`. Writing all four into `Cache/`
        // matched only `_t`, which is why three quarters of the bubbles stayed blank.
        let resource = Path::new("/x/ZaloDownloads/resource/123");
        let url = "https://photo-cdn.zdn.vn/a/b/c.jpg";
        let thumb = photo_variant_path(resource, "1794313779487_1_2", "thumbUrl", url);
        assert_eq!(
            thumb,
            Path::new("/x/ZaloDownloads/resource/123/Cache/1794313779487_1_2_t.jpg")
        );
        let orig = photo_variant_path(resource, "1794313779487_1_2", "oriUrl", url);
        assert_eq!(
            orig,
            Path::new("/x/ZaloDownloads/resource/123/picture/1794313779487_1_2_990f54dc83dcf32632f6b917cba48641.jpg")
        );
        assert!(orig.is_absolute());
    }

    #[test]
    fn original_variant_name_carries_the_url_md5() {
        let resource = Path::new("/x/ZaloDownloads/resource/123");
        let url = "https://f41-zpg.zdn.vn/1042162557850566303/4d6d8f6225cbd3958ada.jpg";
        let path = photo_variant_path(resource, "1_2_3", "hdUrl", url);
        // The renderer derives the middle segment from the URL alone, so the name has to
        // be reproducible from the message row and nothing else.
        assert_eq!(url_md5(url).len(), 32);
        assert!(path.to_string_lossy().contains(&url_md5(url)));
        assert!(path.to_string_lossy().ends_with(".jpg"));
    }

    #[test]
    fn photo_cache_written_from_memory_uses_the_probed_names() {
        let dir = std::env::temp_dir().join(format!("olaz-photo-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let image = image::RgbImage::from_pixel(2, 2, image::Rgb([200, 30, 30]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&image::DynamicImage::ImageRgb8(image))
            .unwrap();
        let url = "https://photo-cdn.zdn.vn/a/b/c.jpg";
        let urls = vec![("thumbUrl".to_string(), url.to_string())];
        let files = write_photo_cache(&dir, "1794313779487_1_2", &urls, &jpeg)
            .expect("jpeg bytes write the thumbnail");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], dir.join("Cache/1794313779487_1_2_t.jpg"));
        for path in files {
            assert_eq!(std::fs::read(&path).unwrap(), jpeg);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn photo_cache_stem_drops_the_checksum_segment() {
        // The downloaded file is named `<cli>_<from>_<to>_<checksum>.jpg`; the names the
        // renderer probes carry no checksum, so it has to go.
        let stem = cache_stem("1794313779487_2892790810767605205_2892790810767605205_68dff14dff9789e10fb84b71cf272e77");
        assert_eq!(stem, "1794313779487_2892790810767605205_2892790810767605205");
    }

    #[test]
    fn cache_write_refuses_a_body_that_is_not_an_image() {
        // What a CDN answers when the link expired: 200 + an HTML error page. It used
        // to be written as the thumbnail, so the bubble got a file that never decodes.
        let dir = std::env::temp_dir().join(format!("olaz-photo-html-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let urls = vec![("thumbUrl".to_string(), "https://a/b/c.jpg".to_string())];
        let html = b"<!DOCTYPE html><html><head><title>404</title></head></html>";
        assert!(write_photo_cache(&dir, "1_2_3", &urls, html).is_none());
        assert!(!dir.join("Cache").exists(), "no cache entry for a non-image payload");
        // Same for an MP4 (a video-thumb request answering with the video itself).
        let mp4 = b"\x00\x00\x00\x1cftypmp42";
        assert!(write_photo_cache(&dir, "1_2_3", &urls, mp4).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_names_take_the_extension_from_the_url() {
        // `path.extname(removeProtocol(url))` — a photo CDN URL always carries one, so
        // the cache entry has to as well. Query strings and fragments are not part of it.
        assert_eq!(url_extension("https://photo-cdn.zdn.vn/a/b/c.jpg?k=1&t=2"), ".jpg");
        // The renderer's `path.extname` keeps the URL's case, so the cache name must too.
        assert_eq!(url_extension("https://photo-cdn.zdn.vn/a/b/c.JXL"), ".JXL");
        assert_eq!(url_extension("https://photo-cdn.zdn.vn/a/b/c.webp#x"), ".webp");
        assert_eq!(url_extension("https://photo-cdn.zdn.vn/a/b/noext"), "");
        assert_eq!(url_extension("https://photo-cdn.zdn.vn/a/v1.2/noext"), "");
        // `?e2esession` is a query the renderer strips before taking the extension.
        assert_eq!(url_extension("https://a/b/c.jpg?e2esession=abc"), ".jpg");
    }

    #[test]
    fn destination_keeps_a_single_component() {
        let options = json!({ "saveDir": "/tmp/olaz-downloads" });
        let data = json!({ "fileName": "../../etc/passwd" });
        let path = destination(&options, &data).expect("absolute saveDir");
        assert_eq!(path, Path::new("/tmp/olaz-downloads/passwd"));
    }

    #[test]
    fn destination_refuses_a_relative_save_dir() {
        let options = json!({ "saveDir": "downloads" });
        let data = json!({ "fileName": "photo.jpg" });
        assert!(destination(&options, &data).is_none());
    }

    #[test]
    fn ctr_block_mode_restarts_the_counter() {
        let key = [9u8; 32];
        let iv = [5u8; 16];
        // Two 16-byte blocks: block mode must decrypt each with the same keystream.
        let stream = ctr_xor(&key, &iv, &[0u8; 16], None).unwrap();
        let mut data = stream.clone();
        data.extend_from_slice(&stream);
        let plain = ctr_xor(&key, &iv, &data, Some(16)).unwrap();
        assert!(plain.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn ctr_linear_mode_chains_the_counter() {
        let key = [3u8; 32];
        let iv = [7u8; 16];
        let input = vec![0u8; 40];
        let encrypted = ctr_xor(&key, &iv, &input, None).unwrap();
        let decrypted = ctr_xor(&key, &iv, &encrypted, None).unwrap();
        assert_eq!(decrypted, input);
        // 40 bytes are not two identical blocks: the counter moved on.
        assert_ne!(encrypted[..16], encrypted[16..32]);
    }

    #[test]
    fn invalid_secure_block_never_yields_bytes() {
        let secure = Secure::parse(&json!({
            "type": "e2ee",
            "cryptography": { "alg": "aes-256-ctr", "key": "AAAA", "iv": "AAAA", "type": "block" }
        }));
        assert!(matches!(secure, Secure::Invalid));
    }

    #[test]
    fn secure_parser_accepts_bridge_encoded_crypto_bytes() {
        let key = [0x31; 32];
        let iv = [0x72; 16];
        let plain = b"encrypted photo bytes";
        let cipher = ctr_xor(&key, &iv, plain, None).unwrap();
        let secure = Secure::parse(&json!({
            "type": "e2ee",
            "cryptography": {
                "alg": "aes-256-ctr",
                "key": { "__bytes": base64::engine::general_purpose::STANDARD.encode(key) },
                "iv": { "__bytes": base64::engine::general_purpose::STANDARD.encode(iv) },
                "type": "linear"
            }
        }));

        assert_eq!(secure.decrypt(&cipher).unwrap(), plain);
    }

    #[test]
    fn secure_parser_accepts_bridge_encoded_gcm_tag() {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Nonce};

        let key = [0x19; 32];
        let iv = [0x42; 12];
        let plain = b"encrypted gcm photo bytes";
        let cipher = Aes256Gcm::new_from_slice(&key)
            .unwrap()
            .encrypt(Nonce::from_slice(&iv), plain.as_slice())
            .unwrap();
        let split = cipher.len() - 16;
        let tag = &cipher[split..];
        let secure = Secure::parse(&json!({
            "type": "e2ee",
            "cryptography": {
                "alg": "aes-256-gcm",
                "key": { "__bytes": base64::engine::general_purpose::STANDARD.encode(key) },
                "iv": { "__bytes": base64::engine::general_purpose::STANDARD.encode(iv) },
                "tag": { "__bytes": base64::engine::general_purpose::STANDARD.encode(tag) },
                "type": "linear"
            }
        }));

        assert_eq!(secure.decrypt(&cipher[..split]).unwrap(), plain);
    }

    #[test]
    fn photo_headers_match_chromium_and_keep_trusted_cloud_headers() {
        let url = reqwest::Url::parse("https://f1.zdn.vn/photo.jpg").unwrap();
        let headers = media_request_headers(
            &[
                ("cloud-viewer-key".into(), "viewer-key".into()),
                ("x-photo-quality".into(), "original".into()),
            ],
            &url,
            Some("zpw_sek=session".into()),
            "ZaloPC-darwin-23v695",
            true,
        );

        let header = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("cloud-viewer-key"), Some("viewer-key"));
        assert_eq!(header("cookie"), Some("zpw_sek=session"));
        assert_eq!(header("user-agent"), Some("ZaloPC-darwin-23v695"));
        assert_eq!(
            header("accept"),
            Some("image/webp,image/apng,image/*,*/*;q=0.8")
        );
        assert_eq!(header("sec-fetch-dest"), Some("image"));
        assert_eq!(header("sec-fetch-mode"), Some("no-cors"));
        assert_eq!(header("sec-fetch-site"), Some("cross-site"));
        assert_eq!(header("x-photo-quality"), Some("original"));
    }

    #[test]
    fn sensitive_media_headers_do_not_leave_zalo_hosts() {
        let url = reqwest::Url::parse("https://example.org/photo.jpg").unwrap();
        let headers = media_request_headers(
            &[
                ("cloud-viewer-key".into(), "viewer-key".into()),
                ("cookie".into(), "session=secret".into()),
                ("x-photo-quality".into(), "original".into()),
                ("bad\nheader".into(), "value".into()),
            ],
            &url,
            Some("zpw_sek=session".into()),
            "ZaloPC-darwin-23v695",
            true,
        );

        let names: Vec<_> = headers
            .iter()
            .map(|(name, _)| name.to_ascii_lowercase())
            .collect();
        assert!(!names.contains(&"cloud-viewer-key".to_string()));
        assert!(!names.contains(&"cookie".to_string()));
        assert!(!names.contains(&"bad\nheader".to_string()));
        assert!(names.contains(&"x-photo-quality".to_string()));
    }

    #[test]
    fn redirect_to_another_host_drops_sensitive_media_headers() {
        let original = reqwest::Url::parse("https://f1.zdn.vn/photo.jpg").unwrap();
        let next = reqwest::Url::parse("https://cdn.example.org/photo.jpg").unwrap();
        let headers = vec![
            ("cloud-viewer-key".into(), "viewer-key".into()),
            ("cookie".into(), "session=secret".into()),
            ("x-photo-quality".into(), "original".into()),
        ];
        let redirected = redirect_media_headers(&headers, &original, &next);

        assert_eq!(
            redirected,
            vec![("x-photo-quality".into(), "original".into())]
        );
    }

    #[test]
    fn redirect_to_another_origin_drops_sensitive_media_headers() {
        let original = reqwest::Url::parse("https://f1.zdn.vn/photo.jpg").unwrap();
        let headers = vec![
            ("cloud-viewer-key".into(), "viewer-key".into()),
            ("cookie".into(), "session=secret".into()),
            ("x-photo-quality".into(), "original".into()),
        ];

        for next in [
            reqwest::Url::parse("https://f1.zdn.vn:444/photo.jpg").unwrap(),
            reqwest::Url::parse("http://f1.zdn.vn/photo.jpg").unwrap(),
        ] {
            let redirected = redirect_media_headers(&headers, &original, &next);
            assert_eq!(
                redirected,
                vec![("x-photo-quality".into(), "original".into())]
            );
        }
    }

    #[test]
    fn fallback_download_sanitizes_headers_and_sets_photo_fingerprint() {
        use std::net::{TcpListener, TcpStream};
        use std::thread;

        fn read_request(stream: &mut TcpStream) -> String {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 2048];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        }

        static NEXT: AtomicU64 = AtomicU64::new(1);
        let redirect_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let redirect_address = redirect_listener.local_addr().unwrap();
        let final_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let final_address = final_listener.local_addr().unwrap();
        let redirect_server = thread::spawn(move || {
            let (mut stream, _) = redirect_listener.accept().unwrap();
            let request = read_request(&mut stream);
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{final_address}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
            request
        });
        let final_server = thread::spawn(move || {
            let (mut stream, _) = final_listener.accept().unwrap();
            let request = read_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
            request
        });

        let directory = std::env::temp_dir().join(format!(
            "olaz-download-header-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let request = json!({
            "type": "Photo",
            "data": {
                "url": format!("http://{redirect_address}/photo.jpg"),
                "fileName": "fallback.jpg"
            },
            "options": {
                "saveDir": directory,
                "headers": [
                    {"name": "cloud-viewer-key", "value": "viewer-secret"},
                    {"name": "cookie", "value": "session=secret"},
                    {"name": "authorization", "value": "Bearer secret"},
                    {"name": "x-photo-quality", "value": "original"}
                ]
            }
        });
        let jar = Arc::new(reqwest::cookie::Jar::default());
        let url = reqwest::Url::parse(&format!("http://{redirect_address}/photo.jpg")).unwrap();
        jar.add_cookie_str("jar-secret=secret; Path=/", &url);
        let outcome = transfer(
            &request,
            &jar,
            "ZaloPC-darwin-23v695",
            false,
            &AtomicBool::new(false),
        );
        let redirect_wire = redirect_server.join().unwrap().to_ascii_lowercase();
        let final_wire = final_server.join().unwrap().to_ascii_lowercase();
        let path = directory.join("fallback.jpg");

        assert!(
            outcome.failure.is_none(),
            "download failed: {:?}",
            outcome.failure
        );
        assert!(redirect_wire.contains("get /photo.jpg http/1.1"));
        assert!(final_wire.contains("get /final http/1.1"));
        for wire in [&redirect_wire, &final_wire] {
            assert!(wire.contains("user-agent: zalopc-darwin-23v695"));
            assert!(wire.contains("sec-ch-ua:"));
            assert!(wire.contains("accept: image/webp,image/apng,image/*,*/*;q=0.8"));
            assert!(wire.contains("sec-fetch-dest: image"));
            assert!(wire.contains("x-photo-quality: original"));
            assert!(!wire.contains("cloud-viewer-key:"));
            assert!(!wire.contains("cookie:"));
            assert!(!wire.contains("jar-secret"));
            assert!(!wire.contains("authorization:"));
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"ok");

        std::fs::remove_dir_all(directory).unwrap();
    }
}
