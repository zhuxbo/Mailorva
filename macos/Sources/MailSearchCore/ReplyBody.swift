import Foundation

public enum ReplyBody {
    public static func compose(_ body: String, original: MailMessage, includeOriginal: Bool = true) -> String {
        guard includeOriginal else { return body }
        let lines = [
            "-------- 原邮件 --------",
            "发件人：\(original.fromAddress)",
            "收件人：\(original.toAddresses.joined(separator: ", "))",
            "时间：\(original.receivedAt)",
            "主题：\(original.subject)",
            "",
            original.bodyText ?? "（无正文）"
        ].joined(separator: "\n")
        let quote = lines.replacingOccurrences(of: "\r\n", with: "\n")
            .replacingOccurrences(of: "\r", with: "\n")
            .components(separatedBy: "\n").map { "> " + $0 }.joined(separator: "\n")
        return body + "\n\n" + quote
    }
}
