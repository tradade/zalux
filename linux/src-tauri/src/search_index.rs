//! Sidecar full-text index of message bodies (`<userdata>/search.db`).
//!
//! The renderer keeps its history in IndexedDB (this app forces
//! `fail_sqlite3=true`, WebKitGTK has no native SQLite DAL), which gives no
//! offline search. This index is our own FTS5 store — the same schema and
//! Vietnamese tokenizer the macOS build uses (`MessageSearchIndex.swift`,
//! borrowed from the Zalos client's offline store): `unicode61
//! remove_diacritics 2`, so "xin chao" matches "xin chào".
//!
//! The feed is what the page has loaded: `web/olaz-linux.js` batches the
//! messages it sees per conversation and POSTs them to
//! `/__olaz/search-index`; the diagnostics-independent search endpoint is
//! `/__olaz/search`. The index never touches the renderer's own data.

use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

pub struct SearchIndex {
    path: PathBuf,
    conn: Mutex<Option<Connection>>,
}

impl SearchIndex {
    /// `directory`: created on first use; the file is `search.db` inside it.
    pub fn new(directory: PathBuf) -> Self {
        Self { path: directory.join("search.db"), conn: Mutex::new(None) }
    }

    fn connect(&self) -> Result<Connection, String> {
        let conn = Connection::open(&self.path).map_err(|e| format!("mở search.db: {e}"))?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| e.to_string())?;
        // WAL + NORMAL: a commit no longer fsyncs; durability across an OS crash is not
        // worth an fsync per message batch here (the index rebuilds from the page).
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(|e| e.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA).map_err(|e| format!("tạo schema search.db: {e}"))?;
        Ok(conn)
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T, String>) -> Result<T, String> {
        let mut guard = self.conn.lock().unwrap();
        if guard.is_none() {
            *guard = Some(self.connect()?);
        }
        f(guard.as_ref().unwrap())
    }

    /// Upserts `upserted` (`[{id, senderId?, senderName?, ts, isMine, text, type?}]`, millisecond
    /// timestamps) of one conversation and drops `removed` ids. Messages without readable
    /// text (stickers, notices) are skipped.
    pub fn update(&self, conv_id: &str, upserted: &[Value], removed: &[String]) -> Result<(), String> {
        if upserted.is_empty() && removed.is_empty() {
            return Ok(());
        }
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
            {
                let mut insert = tx
                    .prepare_cached(
                        "INSERT INTO messages (msg_id, conv_id, sender_id, sender_name, ts, from_me, body, msg_type)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                         ON CONFLICT(msg_id) DO UPDATE SET
                           conv_id=excluded.conv_id, sender_id=excluded.sender_id, sender_name=excluded.sender_name,
                           ts=excluded.ts, from_me=excluded.from_me, body=excluded.body, msg_type=excluded.msg_type",
                    )
                    .map_err(|e| e.to_string())?;
                for message in upserted {
                    let text = message.get("text").and_then(|v| v.as_str()).unwrap_or_default();
                    let id = message.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    if text.trim().is_empty() || id.is_empty() {
                        continue;
                    }
                    let ts = message.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
                    let kind = message.get("type").and_then(|v| v.as_str()).unwrap_or("other");
                    // Centered notices / recalls carry no searchable content of their own.
                    if matches!(kind, "notice" | "recalled" | "system") {
                        continue;
                    }
                    insert
                        .execute(rusqlite::params![
                            id,
                            conv_id,
                            message.get("senderId").and_then(|v| v.as_str()).unwrap_or_default(),
                            message.get("senderName").and_then(|v| v.as_str()).unwrap_or_default(),
                            ts,
                            message.get("isMine").and_then(|v| v.as_bool()).unwrap_or(false),
                            fold_vietnamese(text),
                            kind,
                        ])
                        .map_err(|e| format!("index tin nhắn {id}: {e}"))?;
                }
                if !removed.is_empty() {
                    let mut delete = tx
                        .prepare_cached("DELETE FROM messages WHERE msg_id = ?1 AND conv_id = ?2")
                        .map_err(|e| e.to_string())?;
                    for id in removed {
                        delete.execute(rusqlite::params![id, conv_id]).map_err(|e| e.to_string())?;
                    }
                }
            }
            tx.commit().map_err(|e| e.to_string())
        })
    }

    /// Match expression: every whitespace-separated token becomes a quoted
    /// prefix term joined with AND — "xin chao" → `"xin"* AND "chao"*`. Tokens
    /// are folded like the indexed bodies (`fold_vietnamese`), the tokenizer
    /// handles the rest of the diacritics.
    fn match_expression(query: &str) -> String {
        query
            .split_whitespace()
            .map(|token| format!("\"{}\"*", fold_vietnamese(token).replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    /// Newest-first hits with snippets; `conv_id` None searches every conversation.
    pub fn search(&self, query: &str, conv_id: Option<&str>, limit: usize) -> Result<Value, String> {
        let match_expr = Self::match_expression(query);
        if match_expr.is_empty() {
            return Ok(json!({ "hits": [] }));
        }
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT m.msg_id, m.conv_id, m.sender_name, m.ts, m.from_me,
                        snippet(messages_fts, 0, '', '', '…', 14)
                 FROM messages_fts JOIN messages m ON m.rowid = messages_fts.rowid
                 WHERE messages_fts MATCH ?1 {} ORDER BY m.ts DESC LIMIT ?2",
                if conv_id.is_some() { "AND m.conv_id = ?3" } else { "" }
            );
            let mut values: Vec<rusqlite::types::Value> = vec![
                rusqlite::types::Value::Text(match_expr),
                rusqlite::types::Value::Integer(limit as i64),
            ];
            if let Some(conv) = conv_id {
                values.push(rusqlite::types::Value::Text(conv.to_string()));
            }
            let mut statement = conn.prepare(&sql).map_err(|e| e.to_string())?;
            let mut rows = statement
                .query(rusqlite::params_from_iter(values))
                .map_err(|e| e.to_string())?;
            let mut hits = Vec::new();
            while let Some(row) = rows.next().map_err(|e| e.to_string())? {
                let field = |i: usize| -> Result<String, String> { row.get(i).map_err(|e| e.to_string()) };
                hits.push(json!({
                    "id": field(0)?,
                    "convId": field(1)?,
                    "senderName": field(2)?,
                    "ts": row.get::<_, i64>(3).map_err(|e| e.to_string())?,
                    "isMine": row.get::<_, i64>(4).map_err(|e| e.to_string())? != 0,
                    "snippet": field(5)?,
                }));
            }
            Ok(json!({ "hits": hits }))
        })
    }
}

// ---------------------------------------------------------------- async feed

/// One queued index update (the shape of a chat-state post).
struct FeedJob {
    conv_id: String,
    upserted: Vec<Value>,
    removed: Vec<String>,
}

/// Handle for feeding the index without ever blocking the caller: `feed` only
/// enqueues. The worker thread applies jobs on its own, so the zstate handler
/// replies in microseconds no matter how big the message batch is — SQLite
/// work must never sit on an HTTP thread (the page's ~6-connections-per-host
/// budget is what keeps every other bridge call moving).
pub struct SearchFeed {
    tx: Mutex<mpsc::SyncSender<FeedJob>>,
    dropped: AtomicU64,
}

impl SearchFeed {
    /// Spawns the worker for `index` and returns the enqueue handle. The
    /// backlog is bounded; when it overflows (indexer slower than the page
    /// loads for a long stretch) the *newest* jobs are dropped — the index is
    /// best effort by contract and those messages simply stay unsearched.
    pub fn spawn(index: Arc<SearchIndex>) -> Self {
        let (tx, rx) = mpsc::sync_channel::<FeedJob>(512);
        let dropped = AtomicU64::new(0);
        std::thread::Builder::new()
            .name("search-index".into())
            .spawn(move || {
                for job in rx {
                    if let Err(error) = index.update(&job.conv_id, &job.upserted, &job.removed) {
                        eprintln!("[olaz] search-index: {error}");
                    }
                }
            })
            .expect("spawn search-index worker");
        Self { tx: Mutex::new(tx), dropped }
    }

    /// Enqueues without blocking; returns false when the backlog was full and
    /// the job was dropped.
    pub fn feed(&self, conv_id: &str, upserted: Vec<Value>, removed: Vec<String>) -> bool {
        if upserted.is_empty() && removed.is_empty() {
            return true;
        }
        let send = self.tx.lock().unwrap().try_send(FeedJob {
            conv_id: conv_id.to_string(),
            upserted,
            removed,
        });
        match send {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(job)) => {
                let seen = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if seen % 100 == 1 {
                    eprintln!("[olaz] search-index: backlog đầy, bỏ batch (tổng đã bỏ: {seen})");
                }
                let _ = job;
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        }
    }
}

/// The tokenizer (`remove_diacritics 2`) folds combining marks (à → a) but not
/// the stroke of đ/Đ — no combining mark there. Both bodies and queries fold
/// it to d, so "don" finds "đơn" and the other way around.
fn fold_vietnamese(text: &str) -> String {
    text.replace('đ', "d").replace('Đ', "D")
}

/// External-content FTS5: the text lives in `messages`, the index in
/// `messages_fts`, kept in sync by triggers.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages (
    msg_id TEXT PRIMARY KEY,
    conv_id TEXT NOT NULL,
    sender_id TEXT,
    sender_name TEXT,
    ts INTEGER NOT NULL,
    from_me INTEGER NOT NULL,
    body TEXT NOT NULL,
    msg_type TEXT
);
CREATE INDEX IF NOT EXISTS messages_conv_ts ON messages(conv_id, ts DESC);
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    body, tokenize=\"unicode61 remove_diacritics 2\", content=\"messages\", content_rowid=\"rowid\");
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, body) VALUES (new.rowid, new.body);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.rowid, old.body);
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.rowid, old.body);
    INSERT INTO messages_fts(rowid, body) VALUES (new.rowid, new.body);
END;
";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn index() -> SearchIndex {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "olaz-search-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        SearchIndex::new(dir)
    }

    fn message(id: &str, text: &str) -> Value {
        json!({ "id": id, "senderName": "Lan", "ts": 1_700_000_000_000i64, "isMine": false, "text": text, "type": "text" })
    }

    #[test]
    fn diacritic_insensitive_search_round_trip() {
        let index = index();
        index
            .update("conv1", &[message("m1", "Xin chào các bạn"), message("m2", "Hẹn gặp lại")], &[])
            .unwrap();
        let hits = index.search("xin chao", None, 50).unwrap();
        let hits = hits.get("hits").unwrap().as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].get("id").unwrap(), "m1");
        assert!(hits[0].get("snippet").unwrap().as_str().unwrap().contains("Xin"));
        // The other way around too.
        assert_eq!(index.search("hẹn gặp lại", None, 50).unwrap().get("hits").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn upsert_dedup_and_remove() {
        let index = index();
        index.update("conv1", &[message("m1", "giá rẻ")], &[]).unwrap();
        index.update("conv1", &[message("m1", "giá rẻ")], &[]).unwrap();
        assert_eq!(index.search("gia re", None, 50).unwrap().get("hits").unwrap().as_array().unwrap().len(), 1);
        index.update("conv1", &[], &["m1".to_string()]).unwrap();
        assert!(index.search("gia re", None, 50).unwrap().get("hits").unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn conversation_filter_and_empty_text_skip() {
        let index = index();
        index
            .update("conv1", &[message("m1", "hoá đơn tháng này"), message("m2", "")], &[])
            .unwrap();
        index.update("conv2", &[message("m3", "hoá đơn điện nước")], &[]).unwrap();
        let result = index.search("hoa don", Some("conv2"), 50).unwrap();
        let in_conv = result.get("hits").unwrap().as_array().unwrap();
        assert_eq!(in_conv.len(), 1);
        assert_eq!(in_conv[0].get("convId").unwrap(), "conv2");
    }

    #[test]
    fn empty_query_is_empty_result() {
        let index = index();
        index.update("conv1", &[message("m1", "chào")], &[]).unwrap();
        assert!(index.search("   ", None, 50).unwrap().get("hits").unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn d_stroke_folds_both_ways() {
        let index = index();
        index.update("conv1", &[message("m1", "hoá đơn điện nước")], &[]).unwrap();
        for query in ["don", "đơn", "ĐƠN"] {
            let hits = index.search(query, None, 50).unwrap().get("hits").unwrap().as_array().unwrap().clone();
            assert_eq!(hits.len(), 1, "query {query}");
            assert_eq!(hits[0].get("id").unwrap(), "m1");
        }
    }
}
