import Foundation

package final class NoRedirect: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    package func urlSession(_ session: URLSession, task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void) { completionHandler(nil) }
}
public final class HTTPMailAPI: MailAPI, ReplyAPI, AccountSettingsAPI, @unchecked Sendable {
    private let connection: Connection
    private let session: URLSession
    public init(connection: Connection, configuration: URLSessionConfiguration = .ephemeral) {
        self.connection = connection
        configuration.urlCache = nil
        configuration.httpCookieStorage = nil
        configuration.httpShouldSetCookies = false
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.timeoutIntervalForRequest = 30
        configuration.timeoutIntervalForResource = 180
        session = URLSession(configuration: configuration, delegate: NoRedirect(), delegateQueue: nil)
    }
    deinit { session.invalidateAndCancel() }
    private func request<T: Decodable>(_ path: [String], query: [URLQueryItem] = [], body: [String: String]? = nil,
                                       method: String? = nil, jsonBody: Data? = nil) async throws -> T {
        // Encode each segment independently so opaque message IDs cannot become a new route.
        let allowed = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-._~"))
        let encoded = path.map { $0.addingPercentEncoding(withAllowedCharacters: allowed)! }.joined(separator: "/")
        guard var parts = URLComponents(string: connection.url.absoluteString + encoded) else { throw ClientError.invalidConnection }
        if !query.isEmpty { parts.queryItems = query }
        guard let url = parts.url else { throw ClientError.invalidConnection }
        var request = URLRequest(url: url)
        request.setValue("Bearer " + connection.token, forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        let data = try body.map { try JSONSerialization.data(withJSONObject: $0) } ?? jsonBody
        request.httpMethod = method ?? (data == nil ? "GET" : "POST")
        if let data {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = data
        }
        do {
            let (data, response) = try await session.data(for: request)
            guard let response = response as? HTTPURLResponse else { throw ClientError.invalidResponse }
            switch response.statusCode {
            case 200..<300: break
            case 400, 422: throw ClientError.invalidRequest
            case 401: throw ClientError.unauthorized
            case 403: throw ClientError.forbidden
            case 404: throw ClientError.notFound
            case 409: throw ClientError.conflict
            default: throw ClientError.unavailable
            }
            let decoder = JSONDecoder()
            decoder.keyDecodingStrategy = .convertFromSnakeCase
            guard let result = try? decoder.decode(T.self, from: data) else { throw ClientError.invalidResponse }
            return result
        } catch is CancellationError { throw CancellationError() }
        catch let error as ClientError { throw error }
        catch { if Task.isCancelled { throw CancellationError() }; throw ClientError.unavailable }
    }
    public func accounts() async throws -> AccountResponse { try await request(["api", "accounts"]) }
    public func messages(query: String, accountId: String?, offset: Int) async throws -> MessagePage {
        var items = [URLQueryItem(name: "q", value: query), URLQueryItem(name: "limit", value: "30"), URLQueryItem(name: "offset", value: String(offset))]
        if let accountId { items.append(URLQueryItem(name: "account_id", value: accountId)) }
        return try await request(["api", "messages"], query: items)
    }
    public func message(id: String) async throws -> MailMessage { try await request(["api", "messages", id]) }
    public func markRead(id: String) async throws -> [String] {
        struct Result: Decodable { let id: String; let flags: [String] }
        let result: Result = try await request(["api", "messages", id, "flags"], method: "POST",
            jsonBody: JSONSerialization.data(withJSONObject: ["seen": true]))
        guard result.id == id, result.flags.contains("\\Seen") else { throw ClientError.invalidResponse }
        return result.flags
    }
    public func prepareReply(id: String, operationId: String, body: String, fromAddress: String? = nil) async throws -> ReplyPreview {
        // ReplyView already applies the user's quote toggle to the complete body.
        var input: [String: Any] = ["operation_id": operationId, "body_text": body, "include_original": false]
        if let fromAddress { input["from_address"] = fromAddress }
        let preview: ReplyPreview = try await request(["api", "messages", id, "reply", "prepare"], method: "POST", jsonBody: JSONSerialization.data(withJSONObject: input))
        // An older server must not silently send from the primary account instead of the chosen alias.
        if let fromAddress, preview.senderAddress.caseInsensitiveCompare(fromAddress) != .orderedSame {
            throw ClientError.invalidResponse
        }
        return preview
    }
    public func sendReply(operationId: String) async throws -> SendStatus {
        try await request(["api", "send", operationId, "send"], body: [:])
    }
    public func sendStatus(operationId: String) async throws -> SendStatus {
        try await request(["api", "send", operationId])
    }
    public func accountSettings() async throws -> AccountSettingsResponse {
        try await request(["api", "account-settings"])
    }
    public func createAccount(_ input: AccountInput) async throws -> ManagedAccount {
        try await request(["api", "account-settings"], method: "POST", jsonBody: encodeAccount(input))
    }
    public func updateAccount(id: String, input: AccountInput) async throws -> ManagedAccount {
        try await request(["api", "account-settings", id], method: "PUT", jsonBody: encodeAccount(input))
    }
    public func deleteAccount(id: String) async throws {
        struct Deleted: Decodable { let deleted: Bool }
        let result: Deleted = try await request(["api", "account-settings", id], method: "DELETE")
        guard result.deleted else { throw ClientError.invalidResponse }
    }
    private func encodeAccount(_ input: AccountInput) throws -> Data {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        return try encoder.encode(input)
    }
}
