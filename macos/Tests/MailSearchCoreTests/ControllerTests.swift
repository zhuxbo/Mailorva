import Foundation
import MailSearchCore

func mail(_ id: String) throws -> MailMessage {
    try decode("""
    {"id":"\(id)","account_id":"a","account_email":"a@example.com","folder":"INBOX","subject":"证书","from_address":"sender@example.com","to_addresses":["a@example.com"],"received_at":"2026-09-26T10:00:00Z","body_preview":"正文","body_text":"正文"}
    """)
}
func page(_ ids: [String], total: Int) throws -> MessagePage {
    let rows = try ids.map { id -> [String: Any] in
        let m = try mail(id)
        return ["id":m.id,"account_id":m.accountId,"account_email":m.accountEmail,"folder":m.folder,"subject":m.subject,"from_address":m.fromAddress,"to_addresses":m.toAddresses,"received_at":m.receivedAt,"body_preview":m.bodyPreview]
    }
    let data = try JSONSerialization.data(withJSONObject: ["items":rows,"total":total,"limit":30,"offset":0,"sync":[]])
    return try decode(String(decoding: data, as: UTF8.self))
}
@MainActor final class FakeAPI: MailAPI, ReplyAPI {
    var pages: [MessagePage] = []
    var requests: [(String, Int)] = []
    var continuations: [String: CheckedContinuation<MessagePage, Error>] = [:]
    var details: [String: CheckedContinuation<MailMessage, Error>] = [:]
    var readRequests: [String] = []
    var failRead = false
    var allowWrite = true
    func markRead(id: String) async throws -> [String] {
        readRequests.append(id)
        if failRead { throw ClientError.unavailable }
        return ["\\Seen"]
    }
    var sent = 0
    var checked = 0
    var failSend = false
    func accounts() async throws -> AccountResponse { try decode("{\"accounts\":[],\"can_write\":\(allowWrite),\"sync\":[]}") }
    func messages(query: String, accountId: String?, offset: Int) async throws -> MessagePage {
        requests.append((query, offset))
        if query == "pages" { return pages.removeFirst() }
        return try await withCheckedThrowingContinuation { continuations[query] = $0 }
    }
    func message(id: String) async throws -> MailMessage {
        try await withCheckedThrowingContinuation { details[id] = $0 }
    }
    func prepareReply(id: String, operationId: String, body: String, fromAddress: String?) async throws -> ReplyPreview {
        let data = try JSONSerialization.data(withJSONObject: ["operation_id":operationId,"status":"prepared","message_id":"<sent@example.com>","account_id":"a","account_email":"a@example.com","to":["reply@example.com"],"subject":"Re: 证书","body_text":body])
        return try decode(String(decoding: data, as: UTF8.self))
    }
    func sendReply(operationId: String) async throws -> SendStatus {
        sent += 1
        if failSend { throw ClientError.unavailable }
        return try status(operationId)
    }
    func sendStatus(operationId: String) async throws -> SendStatus { checked += 1; return try status(operationId) }
    private func status(_ id: String) throws -> SendStatus { try decode("{\"operation_id\":\"\(id)\",\"status\":\"sent\",\"message_id\":\"<sent@example.com>\"}") }
}
@MainActor final class MemoryVault: CredentialStore {
    var saved: Connection?
    var pending: PendingReply?
    var fail = false
    func connection() throws -> Connection? { saved }
    func saveConnection(_ value: Connection?) throws { saved = value }
    func pendingReply() throws -> PendingReply? { pending }
    func savePendingReply(_ value: PendingReply?) throws { if fail { throw ClientError.keychain }; pending = value }
}
@MainActor func waitFor(_ predicate: () -> Bool) async throws {
    let deadline = Date().addingTimeInterval(2)
    while !predicate() {
        if Date() > deadline { throw CheckFailure(message: "Timed out waiting for test state") }
        await Task.yield()
    }
}
@MainActor func checkControllers() async throws {
    let api = FakeAPI(), vault = MemoryVault()
    let search = SearchStore(api: api)
    search.refreshAccounts()
    try await waitFor { search.canWrite }
    search.search(query: "old", accountId: nil)
    try await waitFor { api.continuations["old"] != nil }
    search.search(query: "new", accountId: nil)
    try await waitFor { api.continuations["new"] != nil }
    api.continuations.removeValue(forKey: "new")!.resume(returning: try page(["new"], total: 1))
    try await waitFor { !search.loading }
    api.continuations.removeValue(forKey: "old")!.resume(returning: try page(["old"], total: 1))
    await Task.yield()
    try expect(search.items.map(\.id) == ["new"], "old search overwrote new results")
    search.openMessage(id: "old")
    try await waitFor { api.details["old"] != nil }
    search.openMessage(id: "new")
    try await waitFor { api.details["new"] != nil }
    api.details.removeValue(forKey: "new")!.resume(returning: try mail("new"))
    try await waitFor { !search.detailLoading }
    api.details.removeValue(forKey: "old")!.resume(returning: try mail("old"))
    await Task.yield()
    try expect(search.detail?.id == "new")
    api.pages = [try page(["a", "b"], total: 4), try page(["b", "c"], total: 4)]
    search.search(query: "pages", accountId: nil)
    try await waitFor { !search.loading }
    search.loadMore()
    try await waitFor { !search.loading }
    try expect(search.items.map(\.id) == ["a", "b", "c"] && search.nextOffset == 4)
    try expect(api.requests.last?.1 == 2, "pagination must use actual server rows")
    let reply = try ReplyController(api: api, vault: vault, serverAddress: "https://mail.example.com/")
    await reply.prepare(id: "a", body: "  \n")
    try expect(reply.preview == nil && reply.error != nil)
    await reply.prepare(id: "a", body: "收到，谢谢")
    try expect(reply.preview?.to == ["reply@example.com"] && reply.canSend)
    vault.fail = true
    await reply.send()
    try expect(api.sent == 0, "must persist before send")
    vault.fail = false; api.failSend = true
    await reply.send()
    let operation = reply.pending?.preview.operationId
    try expect(api.sent == 1 && operation != nil && !reply.canSend)
    await reply.send()
    try expect(api.sent == 1, "unknown must not resend")
    let otherServer = try ReplyController(api: api, vault: vault, serverAddress: "https://other.example.com/")
    await otherServer.checkStatus()
    try expect(api.checked == 0 && !otherServer.serverMatches)
    let restored = try ReplyController(api: api, vault: vault, serverAddress: "https://mail.example.com/")
    try expect(restored.pending?.preview.operationId == operation && !restored.canSend)
    await restored.checkStatus()
    try expect(restored.status == "sent" && restored.pending == nil && vault.pending == nil && !restored.canSend)
    try expect(api.sent == 1 && api.checked == 1)
}
