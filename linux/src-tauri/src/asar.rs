//! Read-only reader for Electron's `app.asar`, the format Zalo PC ships its
//! renderer in. Port of `Sources/Olaz/ZaloRuntime/AsarArchive.swift`.
//!
//! Layout: `[u32 4][u32 headerPickleSize]` then a pickle `[u32 payloadLen][u32 jsonLen][json]`;
//! file offsets in the JSON header are relative to `8 + headerPickleSize`.
//! Entries flagged `unpacked` live beside the archive in `app.asar.unpacked/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Entry {
    offset: usize,
    size: usize,
    unpacked: bool,
}

pub struct Asar {
    data: Vec<u8>,
    base: usize,
    entries: HashMap<String, Entry>,
    unpacked_root: PathBuf,
}

impl Asar {
    pub fn open(path: &Path) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if data.len() < 16 {
            return Err("app.asar quá nhỏ để là một archive".into());
        }
        let header_pickle = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        let json_len = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
        if 16 + json_len > data.len() {
            return Err("app.asar: header vượt quá kích thước file".into());
        }
        let json: serde_json::Value = serde_json::from_slice(&data[16..16 + json_len])
            .map_err(|e| format!("app.asar: header JSON không hợp lệ: {e}"))?;
        let mut entries = HashMap::new();
        index(&json, "", &mut entries);
        if entries.is_empty() {
            return Err("app.asar: không có file nào trong header".into());
        }
        Ok(Self {
            base: 8 + header_pickle,
            entries,
            data,
            unpacked_root: path.with_file_name("app.asar.unpacked"),
        })
    }

    pub fn contains(&self, path: &str) -> bool {
        self.entries.contains_key(&normalize(path))
    }

    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        let key = normalize(path);
        let entry = self.entries.get(&key)?;
        if entry.unpacked {
            return std::fs::read(self.unpacked_root.join(&key)).ok();
        }
        let start = self.base + entry.offset;
        if start + entry.size > self.data.len() {
            return None;
        }
        Some(self.data[start..start + entry.size].to_vec())
    }

    pub fn count(&self) -> usize {
        self.entries.len()
    }
}

fn index(node: &serde_json::Value, prefix: &str, out: &mut HashMap<String, Entry>) {
    let Some(files) = node.get("files").and_then(|f| f.as_object()) else { return };
    for (name, child) in files {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        if child.get("files").is_some() {
            index(child, &path, out);
        } else if let Some(size) = child.get("size").and_then(|s| s.as_u64()) {
            let offset = child
                .get("offset")
                .and_then(|o| o.as_str())
                .and_then(|o| o.parse::<usize>().ok())
                .unwrap_or(0);
            out.insert(
                path,
                Entry {
                    offset,
                    size: size as usize,
                    unpacked: child.get("unpacked").and_then(|u| u.as_bool()).unwrap_or(false),
                },
            );
        }
    }
}

fn normalize(path: &str) -> String {
    let decoded = percent_encoding::percent_decode_str(path).decode_utf8_lossy().into_owned();
    decoded.trim_start_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal two-file asar so the parser is testable without VNG's archive.
    fn fixture(dir: &Path) -> PathBuf {
        let body_a = b"<html>login</html>";
        let body_b = b"console.log(1)";
        let header = serde_json::json!({
            "files": {
                "pc-dist": { "files": {
                    "login.html": { "size": body_a.len(), "offset": "0" },
                    "app.js": { "size": body_b.len(), "offset": body_a.len().to_string() }
                }}
            }
        });
        let json = serde_json::to_vec(&header).unwrap();
        let mut blob = Vec::new();
        // Real archives: the header pickle size covers everything from its own
        // payload-length field to the end of the JSON, so the file data starts at
        // `8 + header_pickle` = `16 + json_len` — where the reader expects it.
        let header_pickle = 8 + json.len();
        let pickle_payload = 4 + json.len();
        blob.extend_from_slice(&4u32.to_le_bytes());
        blob.extend_from_slice(&(header_pickle as u32).to_le_bytes());
        blob.extend_from_slice(&(pickle_payload as u32).to_le_bytes());
        blob.extend_from_slice(&(json.len() as u32).to_le_bytes());
        blob.extend_from_slice(&json);
        blob.extend_from_slice(body_a);
        blob.extend_from_slice(body_b);
        let path = dir.join("app.asar");
        std::fs::write(&path, blob).unwrap();
        path
    }

    #[test]
    fn reads_files_by_offset() {
        let dir = std::env::temp_dir().join(format!("olaz-asar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let asar = Asar::open(&fixture(&dir)).unwrap();
        assert_eq!(asar.read("pc-dist/login.html").unwrap(), b"<html>login</html>");
        assert_eq!(asar.read("/pc-dist/app.js").unwrap(), b"console.log(1)");
        assert!(asar.read("pc-dist/missing.js").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
