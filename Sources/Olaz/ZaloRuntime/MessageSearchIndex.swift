import Foundation
import SQLite3

/// Sidecar full-text index of message bodies, so search works offline over everything the engine
/// has seen (`<userData>/olaz/search.db`). Zalo PC keeps its own search inside the official
/// databases; this index lives outside `_production` and never touches them.
///
/// The tokenizer is FTS5's `unicode61 remove_diacritics 2`: "xin chao" matches "xin chào" (and the
/// other way around), the way the Vietnamese tokenizer of the Zalos client's offline store works.
/// The feed is the chat state the engine already receives (`ZaloEngine+Chat.mergeChat`): live
/// merges of the open conversation upsert by message id, removals drop out, logout erases the file.
final class MessageSearchIndex: @unchecked Sendable {
    /// One search result row.
    struct SearchHit: Equatable, Identifiable, Sendable {
        var msgId: String
        var conversationId: String
        var senderName: String
        /// Match excerpt with `…` around it (FTS5 `snippet`).
        var snippet: String
        var date: Date
        var isMine: Bool
        var id: String { msgId }
    }

    private let url: URL
    private let queue = DispatchQueue(label: "app.olaz.searchindex")
    private var db: OpaquePointer?

    /// `directory`: created on first use; the database file is `search.db` inside it.
    init(directory: URL) {
        url = directory.appendingPathComponent("search.db")
    }

    // MARK: Schema

    /// FTS5 external-content table: the text lives in `messages`, the index only in `messages_fts`,
    /// kept in sync by triggers (the external-content contract).
    private static let schema = """
    CREATE TABLE IF NOT EXISTS messages (
        msg_id TEXT PRIMARY KEY,
        conv_id TEXT NOT NULL,
        sender_id TEXT,
        sender_name TEXT,
        ts INTEGER NOT NULL,
        from_me INTEGER NOT NULL,
        body TEXT NOT NULL,
        msg_type TEXT,
        raw TEXT
    ) STRICT;
    CREATE INDEX IF NOT EXISTS messages_conv_ts ON messages(conv_id, ts DESC);
    CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
        body, tokenize="unicode61 remove_diacritics 2", content="messages", content_rowid="rowid");
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
    """

    private func connect() throws -> OpaquePointer {
        if let db { return db }
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        var handle: OpaquePointer?
        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_PRIVATECACHE | SQLITE_OPEN_FULLMUTEX
        guard sqlite3_open_v2(url.path, &handle, flags, nil) == SQLITE_OK, let handle else {
            let error = Self.error(handle, fallback: SQLITE_ERROR)
            if let handle { sqlite3_close_v2(handle) }
            throw error
        }
        try Self.exec(handle, "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        try Self.exec(handle, Self.schema)
        db = handle
        return handle
    }

    // MARK: Feed

    /// Upserts messages (id-keyed, like the engine's merge) and drops `removed` ids of the same conversation.
    func update(_ conversationId: String, upserted: [ChatMessage], removed: [String]) async {
        guard !upserted.isEmpty || !removed.isEmpty else { return }
        _ = try? await onQueue { db in
            try Self.transaction(db) {
                let prepared = try Self.prepare(db, """
                    INSERT INTO messages (msg_id, conv_id, sender_id, sender_name, ts, from_me, body, msg_type, raw)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT(msg_id) DO UPDATE SET
                        conv_id=excluded.conv_id, sender_id=excluded.sender_id, sender_name=excluded.sender_name,
                        ts=excluded.ts, from_me=excluded.from_me, body=excluded.body, msg_type=excluded.msg_type
                    """)
                defer { sqlite3_finalize(prepared) }
                for message in upserted where !message.text.isEmpty {
                    // Stand-in bubbles ("decryption pending") and recalled notices carry no
                    // content of their own; everything else with text is worth finding.
                    if case .placeholder = message.kind { continue }
                    if case .recalled = message.kind { continue }
                    let ts = Int64(message.date.timeIntervalSince1970 * 1000)
                    try Self.bind(prepared, [message.id, conversationId, message.senderId, message.senderName,
                                             ts, message.isMine, Self.foldVietnamese(message.text), Self.typeName(message.kind), nil], db)
                    try Self.stepToEnd(prepared, db)
                    sqlite3_reset(prepared)
                    sqlite3_clear_bindings(prepared)
                }
                if !removed.isEmpty {
                    let delete = try Self.prepare(db, "DELETE FROM messages WHERE msg_id = ? AND conv_id = ?")
                    defer { sqlite3_finalize(delete) }
                    for id in removed {
                        try Self.bind(delete, [id, conversationId], db)
                        try Self.stepToEnd(delete, db)
                        sqlite3_reset(delete)
                        sqlite3_clear_bindings(delete)
                    }
                }
            }
        }
    }

    /// Everything logged out: closes the database and removes the files.
    func erase() async {
        _ = try? await onQueue { db in
            sqlite3_close_v2(db)
            self.db = nil
            let fm = FileManager.default
            for suffix in ["", "-shm", "-wal"] { try? fm.removeItem(atPath: self.url.path + suffix) }
        }
    }

    // MARK: Search

    /// `đ`/`Đ` carry no combining mark, so the tokenizer cannot fold the stroke: both bodies
    /// (at index time) and query tokens fold it to `d`, so "don" finds "đơn" and the other way around.
    private static func foldVietnamese(_ text: String) -> String {
        text.replacingOccurrences(of: "đ", with: "d").replacingOccurrences(of: "Đ", with: "D")
    }

    /// FTS5 match expression for `query`: every whitespace-separated token becomes a quoted prefix
    /// term, joined with AND — "xin chao" → `"xin"* AND "chao"*`. Tokens fold like the indexed
    /// bodies (`foldVietnamese`); the tokenizer on the index side handles the diacritics.
    static func matchExpression(for query: String) -> String {
        query.split(whereSeparator: \.isWhitespace).compactMap { token -> String? in
            let escaped = foldVietnamese(String(token)).replacingOccurrences(of: "\"", with: "\"\"")
            return escaped.isEmpty ? nil : "\"\(escaped)\"*"
        }.joined(separator: " AND ")
    }

    /// Matches ranked newest first; `conversationId` nil searches every conversation.
    func search(_ query: String, in conversationId: String? = nil, limit: Int = 50) async -> [SearchHit] {
        let match = Self.matchExpression(for: query)
        guard !match.isEmpty else { return [] }
        return (try? await onQueue { db in
            let sql = """
                SELECT m.msg_id, m.conv_id, m.sender_name, m.ts, m.from_me,
                       snippet(messages_fts, 0, '', '', '…', 14) AS snip
                FROM messages_fts JOIN messages m ON m.rowid = messages_fts.rowid
                WHERE messages_fts MATCH ? \(conversationId != nil ? "AND m.conv_id = ?" : "")
                ORDER BY m.ts DESC LIMIT ?
                """
            let prepared = try Self.prepare(db, sql)
            defer { sqlite3_finalize(prepared) }
            var params: [Any] = [match]
            if let conversationId { params.append(conversationId) }
            params.append(limit)
            try Self.bind(prepared, params, db)
            var hits: [SearchHit] = []
            while sqlite3_step(prepared) == SQLITE_ROW {
                guard let id = Self.text(prepared, 0) else { continue }
                hits.append(SearchHit(
                    msgId: id,
                    conversationId: Self.text(prepared, 1) ?? "",
                    senderName: Self.text(prepared, 2) ?? "",
                    snippet: Self.text(prepared, 5) ?? "",
                    date: Date(timeIntervalSince1970: Double(sqlite3_column_int64(prepared, 3)) / 1000),
                    isMine: sqlite3_column_int64(prepared, 4) != 0))
            }
            return hits
        }) ?? []
    }

    private static func typeName(_ kind: ChatMessage.Kind) -> String {
        switch kind {
        case .text: "text"
        case .photo: "photo"
        case .link: "link"
        case .file: "file"
        default: "other"
        }
    }

    private static func text(_ prepared: OpaquePointer, _ column: Int32) -> String? {
        guard sqlite3_column_type(prepared, column) == SQLITE_TEXT else { return nil }
        return String(cString: sqlite3_column_text(prepared, column))
    }

    // MARK: SQLite plumbing (always on the index queue)

    private func onQueue<T>(_ body: @escaping @Sendable (OpaquePointer) throws -> T) async throws -> T {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<T, Error>) in
            queue.async {
                do { try continuation.resume(returning: body(self.connect())) }
                catch { continuation.resume(throwing: error) }
            }
        }
    }

    private static func exec(_ db: OpaquePointer, _ sql: String) throws {
        guard sqlite3_exec(db, sql, nil, nil, nil) == SQLITE_OK else { throw error(db, fallback: SQLITE_ERROR) }
    }

    private static func transaction(_ db: OpaquePointer, _ body: () throws -> Void) throws {
        try exec(db, "BEGIN TRANSACTION")
        do { try body(); try exec(db, "COMMIT") }
        catch { sqlite3_exec(db, "ROLLBACK", nil, nil, nil); throw error }
    }

    private static func prepare(_ db: OpaquePointer, _ sql: String) throws -> OpaquePointer {
        var prepared: OpaquePointer?
        guard sqlite3_prepare_v2(db, sql, -1, &prepared, nil) == SQLITE_OK, let prepared else { throw error(db, fallback: SQLITE_ERROR) }
        return prepared
    }

    private static func stepToEnd(_ prepared: OpaquePointer, _ db: OpaquePointer) throws {
        while true {
            let rc = sqlite3_step(prepared)
            if rc == SQLITE_DONE || rc == SQLITE_ROW { return }
            throw error(db, fallback: rc)
        }
    }

    private static let transient = unsafeBitCast(-1, to: sqlite3_destructor_type.self)

    private static func bind(_ prepared: OpaquePointer, _ params: [Any], _ db: OpaquePointer) throws {
        let expected = Int(sqlite3_bind_parameter_count(prepared))
        for index in 0..<expected {
            let position = Int32(index + 1)
            let value: Any? = index < params.count ? params[index] : nil
            let rc: Int32
            switch value {
            case let number as NSNumber:
                if CFGetTypeID(number) == CFBooleanGetTypeID() {
                    rc = sqlite3_bind_int64(prepared, position, number.boolValue ? 1 : 0)
                } else if CFNumberIsFloatType(number) {
                    rc = sqlite3_bind_double(prepared, position, number.doubleValue)
                } else {
                    rc = sqlite3_bind_int64(prepared, position, number.int64Value)
                }
            case let text as String:
                rc = sqlite3_bind_text(prepared, position, text, -1, transient)
            default:
                rc = sqlite3_bind_null(prepared, position)
            }
            guard rc == SQLITE_OK else { throw error(db, fallback: rc) }
        }
    }

    private static func error(_ db: OpaquePointer?, fallback: Int32) -> Error {
        let errno = db.map { sqlite3_extended_errcode($0) & 0xff } ?? fallback
        let detail = db.map { String(cString: sqlite3_errmsg($0)) } ?? String(cString: sqlite3_errstr(fallback))
        return SQLiteError(errno: errno == 0 ? fallback : errno, code: String(cString: sqlite3_errstr(errno == 0 ? fallback : errno)), detail: detail)
    }

    struct SQLiteError: Error {
        let errno: Int32
        let code: String
        let detail: String
    }
}
