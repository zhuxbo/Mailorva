import Foundation
import MailSearchCore

@MainActor func checkReadAndQuote() async throws {
    let api = FakeAPI()
    let store = SearchStore(api: api)
    store.refreshAccounts()
    try await waitFor { store.canWrite }
    api.pages = [try page(["new-read"], total: 1)]
    store.search(query: "pages", accountId: nil)
    try await waitFor { !store.loading }
    store.openMessage(id: "old-read")
    try await waitFor { api.details["old-read"] != nil }
    store.openMessage(id: "new-read")
    try await waitFor { api.details["new-read"] != nil }
    api.details.removeValue(forKey: "old-read")!.resume(returning: try mail("old-read"))
    api.details.removeValue(forKey: "new-read")!.resume(returning: try mail("new-read"))
    try await waitFor { store.detail?.isRead == true }
    try expect(store.items.first?.isRead == true, "list must reflect acknowledged read status")
    try expect(api.readRequests == ["new-read"], "stale details must not mark unread mail read")
    store.markCurrentMessageRead()
    try expect(api.readRequests.count == 1)
    api.failRead = true
    store.openMessage(id: "read-failure")
    try await waitFor { api.details["read-failure"] != nil }
    api.details.removeValue(forKey: "read-failure")!.resume(returning: try mail("read-failure"))
    try await waitFor { store.readError != nil }
    try expect(store.detail?.isRead == false && store.detailError == nil, "failed marking must keep readable detail and truthful flags")
    api.failRead = false
    store.markCurrentMessageRead()
    try await waitFor { store.detail?.isRead == true }
    api.allowWrite = false
    store.refreshAccounts()
    try await waitFor { !store.canWrite }
    let before = api.readRequests.count
    store.openMessage(id: "readonly")
    try await waitFor { api.details["readonly"] != nil }
    api.details.removeValue(forKey: "readonly")!.resume(returning: try mail("readonly"))
    try await waitFor { !store.detailLoading }
    try expect(api.readRequests.count == before)
    // Permission arriving after the detail still marks the currently displayed message.
    api.allowWrite = true
    store.refreshAccounts()
    try await waitFor { store.detail?.isRead == true }
    try expect(api.readRequests.last == "readonly")
    let original = try mail("quote")
    let included = ReplyBody.compose("收到，谢谢", original: original)
    try expect(included.hasPrefix("收到，谢谢\n\n> -------- 原邮件 --------"))
    try expect(included.contains("> 发件人：sender@example.com") && included.contains("> 正文"))
    try expect(ReplyBody.compose("收到，谢谢", original: original, includeOriginal: false) == "收到，谢谢")
    let config = URLSessionConfiguration.ephemeral
    config.protocolClasses = [StubProtocol.self]
    let http = HTTPMailAPI(connection: try Connection(address: "https://mail.example.com", token: "test"), configuration: config)
    StubProtocol.handler = { request in
        try expect(request.httpMethod == "POST" && request.url?.path == "/api/messages/m/flags")
        let body = try requestBody(request)
        try expect(body["seen"] as? Bool == true && body.count == 1)
        return (200, try JSONSerialization.data(withJSONObject: ["id": "m", "flags": ["\\Seen"]]))
    }
    let flags = try await http.markRead(id: "m")
    try expect(flags == ["\\Seen"])
    StubProtocol.handler = { _ in (200, Data(#"{"id":"other","flags":[]}"#.utf8)) }
    do { _ = try await http.markRead(id: "m"); throw CheckFailure(message: "Expected flag response validation") }
    catch ClientError.invalidResponse {}
}
