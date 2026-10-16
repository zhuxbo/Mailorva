import Foundation
import Darwin

public struct Connection: Codable, Equatable, Sendable {
    public let url: URL
    public let token: String
    public init(address: String, token: String, allowLocalHTTP: Bool = false) throws {
        guard var parts = URLComponents(string: address.trimmingCharacters(in: .whitespacesAndNewlines)),
              let host = parts.host, !host.isEmpty, parts.user == nil, parts.password == nil,
              parts.query == nil, parts.fragment == nil,
              !token.isEmpty, token.unicodeScalars.allSatisfy({ (33...126).contains(Int($0.value)) })
        else { throw ClientError.invalidConnection }
        var localHTTP = false
        #if DEBUG
        localHTTP = allowLocalHTTP && parts.scheme == "http" && ["localhost", "127.0.0.1", "[::1]", "::1"].contains(host)
        #endif
        let privateHTTP = parts.scheme == "http" && Self.isPrivateIP(host)
        guard parts.scheme == "https" || privateHTTP || localHTTP else { throw ClientError.invalidConnection }
        if !parts.path.hasSuffix("/") { parts.path += "/" }
        guard let url = parts.url else { throw ClientError.invalidConnection }
        self.url = url
        self.token = token
    }
    private static func isPrivateIP(_ host: String) -> Bool {
        var address = in_addr()
        if inet_pton(AF_INET, host, &address) == 1 {
            return withUnsafeBytes(of: address) { bytes in
                bytes[0] == 10 || (bytes[0] == 172 && (16...31).contains(bytes[1])) ||
                    (bytes[0] == 192 && bytes[1] == 168)
            }
        }
        let literal = host.hasPrefix("[") && host.hasSuffix("]") ? String(host.dropFirst().dropLast()) : host
        var address6 = in6_addr()
        guard inet_pton(AF_INET6, literal, &address6) == 1 else { return false }
        return withUnsafeBytes(of: address6) { $0[0] & 0xfe == 0xfc }
    }
    public func validateReplacement(pendingServerAddress: String?, busy: Bool) throws {
        guard !busy, pendingServerAddress == nil || pendingServerAddress == url.absoluteString
        else { throw ClientError.pendingReply }
    }
}
public enum ClientError: Error, LocalizedError {
    case invalidConnection, unauthorized, forbidden, notFound, unavailable, invalidResponse, keychain, invalidReply, pendingReply, invalidRequest, conflict
    public var errorDescription: String? {
        switch self {
        case .invalidConnection: return "请输入有效的服务地址和访问令牌。内网 IP 可使用 HTTP，公网地址须使用 HTTPS。"
        case .unauthorized: return "认证失败，请检查访问令牌。"
        case .forbidden: return "此操作需要可写令牌，请在连接设置中更换令牌。"
        case .notFound: return "邮件已不存在或超出保留范围，请刷新搜索。"
        case .unavailable: return "请求失败，请检查服务器连接后重试。"
        case .invalidResponse: return "服务器响应无效，请确认服务地址和版本。"
        case .keychain: return "无法访问钥匙串，未保存连接或发送记录。"
        case .invalidReply: return "请输入回复正文。"
        case .pendingReply: return "有未确认的发送记录，请先查询状态或核对邮箱。"
        case .invalidRequest: return "服务器未接受这些信息，请检查填写内容。邮箱最多可添加 10 个。"
        case .conflict: return "操作与现有记录冲突，请检查邮箱是否已添加，或刷新后重试。"
        }
    }
}
