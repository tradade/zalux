//! One place that turns errors into user-facing Vietnamese text.
//!
//! Mirrors the error catalog of the Zalos client (`error_catalog.{h,cpp}`):
//! every message the user reads comes from here, while the raw error text
//! always stays available for the log and the diagnostics page (the `trace_raw`
//! behaviour of the original is baked in: raw is never dropped, the catalog
//! only adds the friendly headline). The page itself (the official renderer)
//! handles its own `error_code`s; this catalog covers what the native side
//! reports: bridge handler failures and the runtime installer.

/// A cataloged error: the friendly line (Vietnamese) to show.
pub struct CatalogedError {
    pub user_message: String,
}

/// QR-login long-poll codes that are *not* failures. `-20` asks for a second
/// scan (multi-layer QR, device linking) — the official poller turns it into
/// QR2 and keeps waiting; it must never be shown as an error.
pub fn is_progress_code(code: i64) -> bool {
    code == -20
}

/// Maps a Zalo API `error_code` to a user message. The page already shows its
/// own `error_message` for most flows, so this covers native-side callers:
/// `fallback` (the code's own text) wins when present, unknown codes get a
/// generic line with the code attached.
pub fn describe_api(code: i64, fallback: Option<&str>) -> CatalogedError {
    if is_progress_code(code) {
        return CatalogedError { user_message: "Quét lần nữa bằng mã QR mới để liên kết thiết bị.".into() };
    }
    let user_message = match fallback {
        Some(text) if !text.trim().is_empty() => text.trim().to_string(),
        _ => format!("Đã xảy ra lỗi (mã {code}). Hãy thử lại."),
    };
    CatalogedError { user_message }
}

/// Classifies a native error string (installer, network, helper) into a hint.
/// The raw text stays available — these strings come from `eprintln!` logs and
/// the diagnostics page, both of which must keep it.
pub fn describe_transport(raw: &str) -> CatalogedError {
    let lower = raw.to_ascii_lowercase();
    let user_message = if let Some(status) = http_status(&lower) {
        match status {
            401 | 403 => "Máy chủ từ chối yêu cầu (HTTP {status}) — thử lại sau ít phút.".replace("{status}", &status.to_string()),
            404 => "Không tìm thấy tài nguyên trên máy chủ (HTTP 404) — có thể bản tải đã đổi đường dẫn.".into(),
            429 => "Máy chủ đang giới hạn tần suất (HTTP 429) — thử lại sau ít phút.".into(),
            500..=599 => "Máy chủ đang lỗi tạm thời (HTTP {status}) — thử lại sau.".replace("{status}", &status.to_string()),
            _ => "Máy chủ trả lời mã HTTP {status}.".replace("{status}", &status.to_string()),
        }
    } else if lower.contains("dns") || lower.contains("resolve") {
        "Không phân giải được tên miền — kiểm tra kết nối mạng hoặc DNS.".into()
    } else if lower.contains("timed out") || lower.contains("timeout") || lower.contains("connect") {
        "Không kết nối được máy chủ — kiểm tra kết nối mạng.".into()
    } else if lower.contains("tls") || lower.contains("certificate") {
        "Lỗi bảo mật kết nối (TLS) — kiểm tra ngày giờ hệ thống.".into()
    } else if lower.contains("7z") || lower.contains("giải nén") || lower.contains("extract") {
        "Giải nén bộ cài thất bại — cài p7zip-full rồi thử lại.".into()
    } else if lower.contains("sha256") || lower.contains("digest") {
        "Bộ cài tải về không khớp mã kiểm tra SHA-256 — thử tải lại.".into()
    } else if lower.contains("no space") || lower.contains("hết dung lượng") {
        "Ổ đĩa đầy — giải phóng dung lượng rồi thử lại.".into()
    } else {
        return CatalogedError { user_message: "Đã xảy ra lỗi. Xem chi tiết trong trang chẩn đoán.".into() };
    };
    CatalogedError { user_message }
}

/// The status number of the codebase's own "{url} trả về HTTP {status}" shape.
fn http_status(lower: &str) -> Option<u16> {
    let at = lower.find("http")?;
    let digits: String = lower[at + 4..]
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minus_twenty_is_progress_not_error() {
        assert!(is_progress_code(-20));
        assert!(!is_progress_code(0));
        assert!(!is_progress_code(-1));
    }

    #[test]
    fn api_uses_fallback_then_generic() {
        assert_eq!(describe_api(-102, Some("Tin nhắn quá dài")).user_message, "Tin nhắn quá dài");
        let generic = describe_api(-102, None);
        assert!(generic.user_message.contains("-102"));
    }

    #[test]
    fn transport_classifies_known_shapes() {
        let cases = [
            ("https://x trả về HTTP 403", "từ chối"),
            ("https://x trả về HTTP 502", "lỗi tạm thời"),
            ("dns error: lookup zalo.me", "tên miền"),
            ("connect timed out", "kết nối mạng"),
            ("tải về: certificate expired", "TLS"),
            ("7z không giải nén được", "p7zip"),
            ("bộ cài không khớp OLAZ_INSTALLER_SHA256", "SHA-256"),
        ];
        for (raw, expect) in cases {
            let described = describe_transport(raw);
            assert!(described.user_message.contains(expect), "{raw} → {}", described.user_message);
        }
    }

    #[test]
    fn transport_falls_back_to_generic() {
        let described = describe_transport("lỗi lạ không phân loại được");
        assert!(described.user_message.contains("Đã xảy ra lỗi"));
    }
}
