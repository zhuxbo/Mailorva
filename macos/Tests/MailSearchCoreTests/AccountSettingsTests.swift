import Foundation
import MailSearchCore

private let fixtureCredential = UUID().uuidString

private let accountJSON = #"{"id":"a","email":"a@example.com","display_name":"工作","imap_host":"imap.example.com","imap_port":993,"smtp_host":"smtp.example.com","smtp_port":465,"folders":["INBOX"]}"#

func requestBody(_ request: URLRequest) throws -> [String: Any] {
    var data = request.httpBody ?? Data()
    if let stream = request.httpBodyStream {
        stream.open(); defer { stream.close() }
        var buffer = [UInt8](repeating: 0, count: 4096)
        while stream.hasBytesAvailable {
            let count = stream.read(&buffer, maxLength: buffer.count)
            guard count >= 0 else { throw CheckFailure(message: "Cannot read request body") }
            if count == 0 { break }
            data.append(contentsOf: buffer.prefix(count))
        }
    }
    return try JSONSerialization.jsonObject(with: data) as! [String: Any]
}

func checkAccountAPI() async throws {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [StubProtocol.self]
    let api = HTTPMailAPI(connection: try Connection(address: "http://192.168.3.113:8080", token: "test-token"), configuration: configuration)
    StubProtocol.handler = { request in
        try expect(request.httpMethod == "GET" && request.url?.path == "/api/account-settings")
        try expect(request.url?.scheme == "http" && request.value(forHTTPHeaderField: "Authorization") == "Bearer test-token")
        return (200, Data("{\"accounts\":[\(accountJSON)]}".utf8))
    }
    let list = try await api.accountSettings()
    try expect(list.accounts.first?.imapHost == "imap.example.com")
    var input = AccountInput(email: "a@example.com", displayName: "工作", imapHost: "imap.example.com",
                             smtpHost: "smtp.example.com", folders: ["INBOX", "订单"], password: fixtureCredential)
    StubProtocol.handler = { request in
        try expect(request.httpMethod == "POST" && request.url?.path == "/api/account-settings")
        let body = try requestBody(request)
        try expect(body["imap_host"] as? String == "imap.example.com")
        try expect(body["smtp_port"] as? Int == 465)
        try expect(body["folders"] as? [String] == ["INBOX", "订单"])
        try expect(body["password"] as? String == fixtureCredential)
        return (200, Data(accountJSON.utf8))
    }
    _ = try await api.createAccount(input)
    input.password = nil
    input.aliases = ["support@example.com"]
    StubProtocol.handler = { request in
        try expect(request.httpMethod == "PUT" && request.url?.path == "/api/account-settings/a")
        let body = try requestBody(request)
        try expect(body["password"] == nil)
        try expect(body["aliases"] as? [String] == ["support@example.com"])
        return (200, Data(accountJSON.utf8))
    }
    _ = try await api.updateAccount(id: "a", input: input)
    StubProtocol.handler = { request in
        try expect(request.httpMethod == "DELETE" && request.url?.path == "/api/account-settings/a")
        return (200, Data(#"{"deleted":true}"#.utf8))
    }
    try await api.deleteAccount(id: "a")
    StubProtocol.handler = { _ in (403, Data("credential must never be shown".utf8)) }
    do { _ = try await api.accountSettings(); throw CheckFailure(message: "Expected forbidden") }
    catch ClientError.forbidden {}
    StubProtocol.handler = { _ in (409, Data()) }
    do { _ = try await api.createAccount(input); throw CheckFailure(message: "Expected conflict") }
    catch ClientError.conflict {}
}

@MainActor private final class FakeAccountAPI: AccountSettingsAPI {
    var rows: [ManagedAccount] = []
    var creates = 0
    var suspendCreate = false
    var pending: CheckedContinuation<ManagedAccount, Error>?
    var fail = false
    func accountSettings() async throws -> AccountSettingsResponse {
        if fail { throw ClientError.forbidden }
        // AccountSettingsResponse deliberately has no credential fields.
        return try decode("{\"accounts\":[\(rows.isEmpty ? "" : accountJSON)]}")
    }
    func createAccount(_ input: AccountInput) async throws -> ManagedAccount {
        creates += 1
        if fail { throw ClientError.unavailable }
        if suspendCreate { return try await withCheckedThrowingContinuation { pending = $0 } }
        return try decode(accountJSON.replacingOccurrences(of: "\"id\":\"a\"", with: "\"id\":\"\(creates)\""))
    }
    func updateAccount(id: String, input: AccountInput) async throws -> ManagedAccount { try decode(accountJSON) }
    func deleteAccount(id: String) async throws { if fail { throw ClientError.unavailable } }
}

@MainActor func checkAccountStore() async throws {
    let api = FakeAccountAPI(), store = AccountSettingsStore(api: FakeAccountAPI())
    await store.load()
    try expect(store.loaded && store.accounts.isEmpty)
    try expect(store.revision == 0, "opening account settings must not invalidate mail results")
    await store.load()
    try expect(store.revision == 0, "refreshing account settings must not invalidate mail results")
    let input = AccountInput(email: "a@example.com", displayName: "工作", imapHost: "imap.example.com", smtpHost: "smtp.example.com", password: fixtureCredential)
    let multiple = AccountSettingsStore(api: api)
    await multiple.load()
    let first = await multiple.save(id: nil, input: input)
    let second = await multiple.save(id: nil, input: input)
    try expect(first && second && multiple.accounts.count == 2, "must keep previously added accounts")
    try expect(multiple.revision == 2, "successful account saves must invalidate mail results")
    await multiple.delete(id: "1")
    try expect(multiple.revision == 3, "successful account deletion must invalidate mail results")
    try expect(multiple.accounts.map(\.id) == ["2"], "deleting one account must preserve others")
    api.fail = true
    await multiple.delete(id: "2")
    try expect(multiple.accounts.count == 1 && multiple.error != nil)
    let failed = await multiple.save(id: nil, input: input)
    try expect(!failed && api.creates == 3 && multiple.accounts.count == 1, "failed request must not retry automatically")
    api.fail = false; api.suspendCreate = true
    let saving = Task { await multiple.save(id: nil, input: input) }
    try await waitFor { api.pending != nil }
    let duplicate = await multiple.save(id: nil, input: input)
    try expect(!duplicate && api.creates == 4, "double click must not send twice")
    api.pending?.resume(returning: try decode(accountJSON)); api.pending = nil
    let saved = await saving.value
    try expect(saved && !multiple.busy)
    let deniedAPI = FakeAccountAPI(); deniedAPI.fail = true
    let denied = AccountSettingsStore(api: deniedAPI)
    await denied.load()
    try expect(!denied.loaded && denied.error != nil)
}
