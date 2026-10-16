import SwiftUI
import MailSearchCore

struct ConnectedView: View {
    @ObservedObject var store: SearchStore
    @ObservedObject var reply: ReplyController
    @ObservedObject var accounts: AccountSettingsStore
    var openSettings: () -> Void
    @State private var searchRevision = 0
    private var canManage: Bool { store.canWrite && reply.pending == nil && !reply.busy }

    var body: some View {
        Group {
            if store.accountsLoaded && store.accounts.isEmpty && store.accountsError == nil && reply.pending == nil {
                VStack(spacing: 16) {
                    Image(systemName: "envelope.badge").font(.system(size: 42)).foregroundStyle(.tint)
                    Text("添加你的第一个邮箱").font(.title2.weight(.semibold))
                    Text("服务器已连接。添加 QQ 邮箱、腾讯企业邮或 Gmail，即可集中搜索和回复。")
                        .foregroundStyle(.secondary)
                    if canManage {
                        Button("添加邮箱") { openSettings() }.buttonStyle(.borderedProminent)
                        Text("最多管理 10 个邮箱，添加后可继续新增。").font(.caption).foregroundStyle(.secondary)
                    } else {
                        Text("请在连接设置中使用可写令牌；如有待确认回复，请先核对发送状态。")
                            .font(.callout).foregroundStyle(.secondary)
                    }
                }.padding(32).frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                SearchView(store: store, reply: reply).id(searchRevision)
            }
        }
        .onChange(of: accounts.revision) { _ in
            store.refreshAccounts()
            store.search(query: "", accountId: nil)
            searchRevision += 1
        }
    }
}

private struct AccountEditorTarget: Identifiable {
    let id = UUID()
    let account: ManagedAccount?
}

struct AccountSettingsView: View {
    @ObservedObject var store: AccountSettingsStore
    @Environment(\.dismiss) private var dismiss
    @State private var editor: AccountEditorTarget?
    @State private var deleting: ManagedAccount?

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                VStack(alignment: .leading, spacing: 5) {
                    Text("邮箱管理").font(.title2.bold())
                    Text("\(store.accounts.count) / 10 个邮箱 · 授权码仅保存在服务器")
                        .font(.callout).foregroundStyle(.secondary)
                }
                Spacer()
                if store.busy { ProgressView().controlSize(.small) }
                Button { Task { await store.load() } } label: { Image(systemName: "arrow.clockwise") }
                    .help("刷新邮箱列表").disabled(store.busy)
                Button("添加邮箱", systemImage: "plus") { openEditor(nil) }
                    .buttonStyle(.borderedProminent).disabled(store.busy || !store.loaded || store.accounts.count >= 10)
            }
            if let error = store.error {
                Text(error).foregroundStyle(.red).font(.callout).fixedSize(horizontal: false, vertical: true)
            }
            if store.loaded && store.accounts.isEmpty {
                VStack(spacing: 12) {
                    Image(systemName: "envelope.badge").font(.largeTitle).foregroundStyle(.secondary)
                    Text("还没有邮箱").font(.headline)
                    Text("先在邮箱侧开启 IMAP / SMTP，并准备授权码或应用专用密码。")
                        .foregroundStyle(.secondary)
                    Button("添加第一个邮箱") { openEditor(nil) }.disabled(store.busy)
                }.frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                List {
                    ForEach(store.accounts) { account in
                        HStack(spacing: 16) {
                            Image(systemName: "envelope").font(.title2).foregroundStyle(.secondary)
                            VStack(alignment: .leading, spacing: 4) {
                                Text(account.displayName.isEmpty ? account.email : account.displayName).font(.headline)
                                Text(account.email).foregroundStyle(.secondary).textSelection(.enabled)
                                if let aliases = account.aliases, !aliases.isEmpty { Text("别名：" + aliases.joined(separator: "、")).font(.caption).foregroundStyle(.secondary).lineLimit(2) }
                                Text("同步文件夹：\(account.folders.joined(separator: "、"))")
                                    .font(.caption).foregroundStyle(.secondary).lineLimit(2)
                            }
                            Spacer()
                            Button("编辑") { openEditor(account) }
                            Button("删除", role: .destructive) { deleting = account }
                        }.padding(.vertical, 8).disabled(store.busy)
                    }
                }.listStyle(.inset).frame(maxHeight: .infinity)
            }
            Divider()
            HStack {
                Text("新邮箱默认只接收之后的新邮件，服务器定期同步。")
                    .font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("完成") { dismiss() }.keyboardShortcut(.cancelAction).disabled(store.busy)
            }
        }.padding(24).frame(width: 720, height: 500)
            .interactiveDismissDisabled(store.busy)
            .task { await store.load() }
            .sheet(item: $editor) { target in AccountEditorView(store: store, account: target.account) }
            .alert("删除邮箱？", isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } })) {
                Button("取消", role: .cancel) { deleting = nil }
                Button("删除", role: .destructive) {
                    if let account = deleting { Task { await store.delete(id: account.id) } }
                    deleting = nil
                }
            } message: {
                Text("将停止同步 \(deleting?.email ?? "")，并清理此服务器上的邮件缓存和待发送内容。邮箱中的原始邮件不会删除。")
            }
    }
    private func openEditor(_ account: ManagedAccount?) {
        store.clearError()
        editor = AccountEditorTarget(account: account)
    }
}

private enum MailProvider: String, CaseIterable, Identifiable {
    case qq = "QQ 邮箱", tencent = "腾讯企业邮", gmail = "Gmail", custom = "其他 IMAP 邮箱"
    var id: String { rawValue }
    var hosts: (String, String) {
        switch self {
        case .qq: return ("imap.qq.com", "smtp.qq.com")
        case .tencent: return ("imap.exmail.qq.com", "smtp.exmail.qq.com")
        case .gmail: return ("imap.gmail.com", "smtp.gmail.com")
        case .custom: return ("", "")
        }
    }
}

struct AccountEditorView: View {
    @ObservedObject var store: AccountSettingsStore
    let account: ManagedAccount?
    @Environment(\.dismiss) private var dismiss
    @State private var provider = MailProvider.qq
    @State private var email = ""
    @State private var displayName = ""
    @State private var password = ""
    @State private var imapHost = "imap.qq.com"
    @State private var smtpHost = "smtp.qq.com"
    @State private var imapPort = "993"
    @State private var smtpPort = "465"
    @State private var folders = "INBOX"
    @State private var aliases = ""
    private var aliasAddresses: [String] { aliases.components(separatedBy: .newlines).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty } }
    private var folderNames: [String] { folders.components(separatedBy: .newlines).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty } }
    private var valid: Bool {
        let address = email.trimmingCharacters(in: .whitespacesAndNewlines)
        return address.contains("@") && !address.contains(where: { $0.isWhitespace }) &&
            !imapHost.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty &&
            !smtpHost.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty &&
            (1...65535).contains(Int(imapPort) ?? 0) && (1...65535).contains(Int(smtpPort) ?? 0) &&
            !folderNames.isEmpty && (account != nil || !password.isEmpty)
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(account == nil ? "添加邮箱" : "编辑邮箱").font(.title2.bold())
            Form {
                Section("账户") {
                    if account == nil {
                        Picker("服务商", selection: $provider) { ForEach(MailProvider.allCases) { Text($0.rawValue).tag($0) } }
                            .onChange(of: provider) { value in
                                (imapHost, smtpHost) = value.hosts; imapPort = "993"; smtpPort = "465"
                            }
                    }
                    TextField("邮箱地址", text: $email).disabled(account != nil)
                    TextField("显示名称", text: $displayName, prompt: Text("例如：工作邮箱（可选）"))
                    SecureField(account == nil ? "授权码 / 应用密码" : "新授权码（留空不变）", text: $password)
                    Text("授权码只发送到你的服务器，不保存在此 App，也不会回显。")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Section("邮件服务器 · TLS") {
                    HStack { TextField("IMAP 主机", text: $imapHost); TextField("端口", text: $imapPort).frame(width: 100) }
                        .disabled(account != nil)
                    HStack { TextField("SMTP 主机", text: $smtpHost); TextField("端口", text: $smtpPort).frame(width: 100) }
                    if account != nil {
                        Text("更换邮箱地址或 IMAP 服务器请新增账户，避免关联到原账户的邮件。")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                Section("邮箱别名") {
                    Text("填写此账号已在邮箱服务商处启用的收件及发件别名，每行一个。回复会优先使用邮件对应的收件地址。")
                        .font(.caption).foregroundStyle(.secondary)
                    TextEditor(text: $aliases).font(.body).frame(height: 60)
                        .accessibilityLabel("邮箱别名，每行一个")
                }
                Section("同步范围") {
                    Text("填写实际 IMAP 文件夹名称，每行一个；INBOX 表示收件箱。")
                        .font(.caption).foregroundStyle(.secondary)
                    TextEditor(text: $folders).font(.body).frame(height: 65)
                        .accessibilityLabel("同步文件夹，每行一个")
                }
            }.formStyle(.grouped).disabled(store.busy)
            if let error = store.error {
                Text(error).font(.callout).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                if store.error != nil {
                    Button("返回列表核对") { password = ""; dismiss(); Task { await store.load() } }.disabled(store.busy)
                }
                Spacer()
                if store.busy { ProgressView().controlSize(.small) }
                Button("取消") { password = ""; dismiss() }.keyboardShortcut(.cancelAction).disabled(store.busy)
                Button(account == nil ? "添加" : "保存") { save() }
                    .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction).disabled(store.busy || !valid)
            }
        }.padding(24).frame(width: 610, height: 650).interactiveDismissDisabled(store.busy)
            .onAppear {
                if let account {
                    email = account.email; displayName = account.displayName
                    imapHost = account.imapHost; imapPort = String(account.imapPort)
                    smtpHost = account.smtpHost; smtpPort = String(account.smtpPort)
                    folders = account.folders.joined(separator: "\n")
                    aliases = (account.aliases ?? []).joined(separator: "\n")
                }
            }
    }
    private func save() {
        let address = email.trimmingCharacters(in: .whitespacesAndNewlines)
        let name = displayName.trimmingCharacters(in: .whitespacesAndNewlines)
        let input = AccountInput(email: address,
            displayName: name.isEmpty ? address : name,
            imapHost: imapHost.trimmingCharacters(in: .whitespacesAndNewlines), imapPort: Int(imapPort) ?? 993,
            smtpHost: smtpHost.trimmingCharacters(in: .whitespacesAndNewlines), smtpPort: Int(smtpPort) ?? 465,
            folders: folderNames, password: password.isEmpty ? nil : password, aliases: aliasAddresses)
        Task { if await store.save(id: account?.id, input: input) { password = ""; dismiss() } }
    }
}
