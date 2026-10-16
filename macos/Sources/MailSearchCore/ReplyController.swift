import Foundation
import Combine

@MainActor public final class ReplyController: ObservableObject {
    @Published public private(set) var preview: ReplyPreview?
    @Published public private(set) var pending: PendingReply?
    @Published public private(set) var status: String?
    @Published public private(set) var busy = false
    @Published public private(set) var error: String?
    private let api: any ReplyAPI
    private let vault: any CredentialStore
    private let serverAddress: String
    public init(api: any ReplyAPI, vault: any CredentialStore, serverAddress: String) throws {
        self.api = api; self.vault = vault; self.serverAddress = serverAddress
        pending = try vault.pendingReply()
        if let pending, pending.serverAddress == serverAddress { preview = pending.preview }
    }
    public var serverMatches: Bool { pending == nil || pending?.serverAddress == serverAddress }
    public var canSend: Bool { !busy && preview != nil && serverMatches && status == "prepared" }
    public func resetDraft() {
        guard pending == nil, !busy else { return }
        preview = nil; status = nil; error = nil
    }
    public func prepare(id: String, body: String, fromAddress: String? = nil) async {
        guard !busy, pending == nil else { error = ClientError.pendingReply.localizedDescription; return }
        guard !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { error = ClientError.invalidReply.localizedDescription; return }
        busy = true; error = nil
        defer { busy = false }
        do {
            preview = try await api.prepareReply(id: id, operationId: UUID().uuidString, body: body, fromAddress: fromAddress)
            status = preview?.status
        } catch { self.error = error.localizedDescription }
    }
    public func send() async {
        guard canSend, let preview else { return }
        let record = pending ?? PendingReply(serverAddress: serverAddress, preview: preview)
        busy = true; error = nil
        defer { busy = false }
        do {
            // Persist before starting the irreversible network request, including its exact preview.
            try vault.savePendingReply(record)
            pending = record; status = "sending"
            try apply(try await api.sendReply(operationId: record.preview.operationId))
        } catch {
            self.error = error.localizedDescription
            if pending != nil { status = "unconfirmed" }
        }
    }
    public func checkStatus() async {
        guard !busy, let pending, serverMatches else { return }
        busy = true; error = nil
        defer { busy = false }
        do { try apply(try await api.sendStatus(operationId: pending.preview.operationId)) }
        catch { self.error = error.localizedDescription }
    }
    private func apply(_ result: SendStatus) throws {
        guard result.operationId == pending?.preview.operationId else { throw ClientError.invalidResponse }
        status = result.status
        if result.status == "sent" {
            try vault.savePendingReply(nil)
            pending = nil
        }
    }
    public func clearAfterVerification() {
        guard !busy else { return }
        do { try vault.savePendingReply(nil); pending = nil; preview = nil; status = nil; error = nil }
        catch { self.error = error.localizedDescription }
    }
}
