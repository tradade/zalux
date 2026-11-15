import Testing
import Foundation
@testable import Olaz

/// `MessageSearchIndex` — the sidecar FTS5 store the offline message search reads. Databases
/// live in a temporary directory; feeds use `ChatMessage` values built directly.
@Suite struct MessageSearchIndexTests {
    private func index() -> MessageSearchIndex {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("OlazSearchIndexTests-\(UUID().uuidString)", isDirectory: true)
        return MessageSearchIndex(directory: root)
    }

    private func message(_ id: String, _ text: String, date: Date = Date(timeIntervalSince1970: 1_700_000_000),
                         isMine: Bool = false, sender: String = "Lan") -> ChatMessage {
        ChatMessage(id: id, conversationId: "conv1", senderId: "u1", senderName: sender, senderAvatar: nil,
                    isMine: isMine, kind: .text, text: text, date: date, status: .sent,
                    quote: nil, reactions: [], isE2EE: false)
    }

    @Test func diacriticInsensitiveRoundTrip() async {
        let index = index()
        await index.update("conv1", upserted: [message("m1", "Xin chào các bạn"), message("m2", "Hẹn gặp lại")], removed: [])
        let hits = await index.search("xin chao")
        #expect(hits.count == 1 && hits[0].msgId == "m1" && hits[0].snippet.contains("Xin"))
        #expect(hits[0].conversationId == "conv1" && hits[0].senderName == "Lan")
        // The other way around too, and prefix matching ("hẹn g" finds "Hẹn gặp lại").
        #expect(await index.search("hẹn gặp").map(\.msgId) == ["m2"])
    }

    @Test func dStrokeFoldsBothWays() async {
        let index = index()
        await index.update("conv1", upserted: [message("m1", "hoá đơn điện nước")], removed: [])
        for query in ["don", "đơn", "ĐƠN"] {
            let hits = await index.search(query)
            #expect(hits.count == 1 && hits[0].msgId == "m1", "query \(query)")
        }
    }

    @Test func upsertDedupsAndRemoves() async {
        let index = index()
        await index.update("conv1", upserted: [message("m1", "giá rẻ")], removed: [])
        await index.update("conv1", upserted: [message("m1", "giá rẻ")], removed: [])
        #expect(await index.search("gia re").count == 1)
        await index.update("conv1", upserted: [], removed: ["m1"])
        #expect(await index.search("gia re").isEmpty)
    }

    @Test func conversationFilter() async {
        let index = index()
        await index.update("conv1", upserted: [message("m1", "hoá đơn tháng này")], removed: [])
        await index.update("conv2", upserted: [message("m3", "hoá đơn điện nước")], removed: [])
        let hits = await index.search("hoa don", in: "conv2")
        #expect(hits.count == 1 && hits[0].conversationId == "conv2")
    }

    @Test func nonTextKindsAndEmptyBodiesSkip() async {
        let index = index()
        var sticker = message("m1", "")
        sticker.kind = .sticker(url: nil)
        var recalled = message("m2", "Tin nhắn đã thu hồi")
        recalled.kind = .recalled
        await index.update("conv1", upserted: [sticker, recalled, message("m3", "tin nhắn thật")], removed: [])
        #expect(await index.search("thu hồi").isEmpty)
        #expect(await index.search("tin nhan that").map(\.msgId) == ["m3"])
    }

    @Test func emptyQueryAndErasedIndex() async {
        let index = index()
        await index.update("conv1", upserted: [message("m1", "chào")], removed: [])
        #expect(await index.search("   ").isEmpty)
        await index.erase()
        #expect(await index.search("chao").isEmpty)
        // The index works again after the erase (the file was removed, not broken).
        await index.update("conv1", upserted: [message("m2", "chào lại")], removed: [])
        #expect(await index.search("chao").map(\.msgId) == ["m2"])
    }
}
