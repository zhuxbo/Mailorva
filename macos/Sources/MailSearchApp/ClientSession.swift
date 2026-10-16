import Foundation
import Combine
import MailSearchCore

@MainActor final class ClientSession: ObservableObject {
    @Published private(set) var connection: Connection?
    @Published private(set) var search: SearchStore?
    @Published private(set) var reply: ReplyController?
    @Published private(set) var accountSettings: AccountSettingsStore?
    @Published var error: String?
    private let vault = KeychainVault()
    init() {
        do { if let saved = try vault.connection() { try activate(saved) } }
        catch { self.error = error.localizedDescription }
    }
    func configure(address: String, token: String, allowLocalHTTP: Bool) throws {
        let connection = try Connection(address: address, token: token, allowLocalHTTP: allowLocalHTTP)
        try connection.validateReplacement(pendingServerAddress: reply?.pending?.serverAddress, busy: reply?.busy == true)
        let api = HTTPMailAPI(connection: connection)
        let reply = try ReplyController(api: api, vault: vault, serverAddress: connection.url.absoluteString)
        try vault.saveConnection(connection)
        install(connection, api: api, reply: reply)
    }
    private func activate(_ connection: Connection) throws {
        let api = HTTPMailAPI(connection: connection)
        let reply = try ReplyController(api: api, vault: vault, serverAddress: connection.url.absoluteString)
        install(connection, api: api, reply: reply)
    }
    private func install(_ connection: Connection, api: HTTPMailAPI, reply: ReplyController) {
        self.connection = connection; self.reply = reply
        accountSettings = AccountSettingsStore(api: api)
        let search = SearchStore(api: api)
        self.search = search; error = nil
        search.refreshAccounts(); search.search(query: "", accountId: nil)
    }
    func disconnect() throws {
        guard reply?.pending == nil, reply?.busy != true else { throw ClientError.pendingReply }
        try vault.saveConnection(nil)
        connection = nil; search = nil; reply = nil; accountSettings = nil; error = nil
    }
}
