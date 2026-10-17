import Foundation
import MailSearchCore

func checkAliasIdentity() async throws {
    let account: Account = try decode(#"{"id":"a","email":"a@example.com","display_name":"工作","aliases":["support@example.com","billing@example.com"]}"#)
    let base = #"{"id":"m","account_id":"a","account_email":"a@example.com","folder":"INBOX","subject":"Alias","from_address":"sender@example.com","to_addresses":["SUPPORT@example.com"],"cc_addresses":["billing@example.com"],"delivery_addresses":["a@example.com"],"received_at":"2026-10-15T00:00:00Z","body_preview":""}"#
    let message: MailMessage = try decode(base)
    try expect(message.defaultReplyAddress(account: account) == "support@example.com")
    try expect(message.matchingAddresses(account: account) == ["support@example.com", "billing@example.com", "a@example.com"])
    let hidden: MailMessage = try decode(base.replacingOccurrences(of: #""to_addresses":["SUPPORT@example.com"],"cc_addresses":["billing@example.com"],"delivery_addresses":["a@example.com"]"#, with: #""to_addresses":[],"delivery_addresses":["billing@example.com"]"#))
    try expect(hidden.defaultReplyAddress(account: account) == "billing@example.com")
    let old = try mail("old")
    try expect(old.defaultReplyAddress(account: nil) == "a@example.com")
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [StubProtocol.self]
    let api = HTTPMailAPI(connection: try Connection(address: "https://mail.example.com", token: "test"), configuration: configuration)
    let preview = #"{"operation_id":"op","status":"prepared","message_id":"sent","account_id":"a","account_email":"support@example.com","from_address":"support@example.com","to":["sender@example.com"],"subject":"Re: Alias","body_text":"Reply"}"#
    StubProtocol.handler = { request in
        let input = try requestBody(request)
        try expect(input["from_address"] as? String == "support@example.com")
        try expect(input["include_original"] as? Bool == false)
        return (200, Data(preview.utf8))
    }
    let result = try await api.prepareReply(id: "m", operationId: "op", body: "Reply", fromAddress: "support@example.com")
    try expect(result.senderAddress == "support@example.com")
    // A mismatched/older server must never produce a sendable alias preview.
    StubProtocol.handler = { _ in (200, Data(preview.replacingOccurrences(of: "support@example.com", with: "a@example.com").utf8)) }
    do {
        _ = try await api.prepareReply(id: "m", operationId: "op2", body: "Reply", fromAddress: "support@example.com")
        throw CheckFailure(message: "Expected sender mismatch rejection")
    } catch ClientError.invalidResponse {}
}
