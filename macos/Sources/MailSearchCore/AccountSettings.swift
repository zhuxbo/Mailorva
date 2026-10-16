import Foundation
import Combine

public struct ManagedAccount: Decodable, Identifiable, Sendable {
    public let id: String
    public let email: String
    public let displayName: String
    public let imapHost: String
    public let imapPort: Int
    public let smtpHost: String
    public let smtpPort: Int
    public let folders: [String]
    public let aliases: [String]?
}
public struct AccountSettingsResponse: Decodable, Sendable {
    public let accounts: [ManagedAccount]
}
public struct AccountInput: Encodable, Sendable {
    public var email: String
    public var displayName: String
    public var imapHost: String
    public var imapPort: Int
    public var smtpHost: String
    public var smtpPort: Int
    public var folders: [String]
    public var password: String?
    public var aliases: [String]

    public init(email: String, displayName: String, imapHost: String, imapPort: Int = 993,
                smtpHost: String, smtpPort: Int = 465, folders: [String] = ["INBOX"], password: String? = nil, aliases: [String] = []) {
        self.email = email; self.displayName = displayName
        self.imapHost = imapHost; self.imapPort = imapPort
        self.smtpHost = smtpHost; self.smtpPort = smtpPort
        self.folders = folders; self.password = password; self.aliases = aliases
    }
}
public protocol AccountSettingsAPI: Sendable {
    func accountSettings() async throws -> AccountSettingsResponse
    func createAccount(_ input: AccountInput) async throws -> ManagedAccount
    func updateAccount(id: String, input: AccountInput) async throws -> ManagedAccount
    func deleteAccount(id: String) async throws
}

@MainActor public final class AccountSettingsStore: ObservableObject {
    @Published public private(set) var accounts: [ManagedAccount] = []
    @Published public private(set) var busy = false
    @Published public private(set) var loaded = false
    @Published public private(set) var error: String?
    @Published public private(set) var revision = 0
    private let api: any AccountSettingsAPI
    public init(api: any AccountSettingsAPI) { self.api = api }

    public func load() async {
        guard !busy else { return }
        busy = true; error = nil
        defer { busy = false }
        do { accounts = try await api.accountSettings().accounts; loaded = true }
        catch { self.error = error.localizedDescription }
    }
    public func save(id: String?, input: AccountInput) async -> Bool {
        guard !busy else { return false }
        busy = true; error = nil
        defer { busy = false }
        do {
            let saved: ManagedAccount
            if let id { saved = try await api.updateAccount(id: id, input: input) }
            else { saved = try await api.createAccount(input) }
            if let index = accounts.firstIndex(where: { $0.id == saved.id }) { accounts[index] = saved }
            else { accounts.append(saved) }
            revision += 1
            return true
        } catch {
            self.error = error.localizedDescription + " 如连接中断，请先刷新邮箱列表确认结果，再重试。"
            return false
        }
    }
    public func delete(id: String) async {
        guard !busy else { return }
        busy = true; error = nil
        defer { busy = false }
        do {
            try await api.deleteAccount(id: id)
            accounts.removeAll { $0.id == id }
            revision += 1
        } catch { self.error = error.localizedDescription + " 请刷新列表确认账户是否已删除。" }
    }
    public func clearError() { error = nil }
}
