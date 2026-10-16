import Foundation
import MailSearchCore

func checkConnection() throws {
    let connection = try Connection(address: " https://mail.example.com/root ", token: "read-token")
    try expect(connection.url.absoluteString == "https://mail.example.com/root/")
    let replacement = try Connection(address: "https://mail.example.com/root", token: "rotated-token")
    try replacement.validateReplacement(pendingServerAddress: connection.url.absoluteString, busy: false)
    try expectThrow { try replacement.validateReplacement(pendingServerAddress: "https://other.example.com/", busy: false) }
    try expectThrow { try replacement.validateReplacement(pendingServerAddress: connection.url.absoluteString, busy: true) }
    for address in ["http://mail.example.com", "https://user:pass@mail.example.com", "https://mail.example.com?token=x", "https://mail.example.com#x", "file:///tmp/mail"] {
        try expectThrow { _ = try Connection(address: address, token: "token", allowLocalHTTP: true) }
    }
    for token in ["", "abc\ndef", "token with space", "中文"] {
        try expectThrow { _ = try Connection(address: "https://mail.example.com", token: token) }
    }
    for host in ["10.0.0.1", "10.255.255.254", "172.16.0.1", "172.31.255.254", "192.168.3.113", "[fd00::113]", "[fc00::1]"] {
        let privateConnection = try Connection(address: "http://\(host):8080", token: "token")
        try expect(privateConnection.url.scheme == "http")
    }
    for host in ["8.8.8.8", "172.15.255.255", "172.32.0.1", "192.169.0.1", "169.254.1.1", "100.64.0.1", "0.0.0.0", "192.168.3.113.example.com", "[2001:db8::1]", "[::ffff:192.168.3.113]", "[fe80::1]"] {
        try expectThrow { _ = try Connection(address: "http://\(host):8080", token: "token") }
    }
    try expectThrow { _ = try Connection(address: "http://127.0.0.1:8080", token: "token") }
    #if DEBUG
    let local = try Connection(address: "http://127.0.0.1:8080", token: "token", allowLocalHTTP: true)
    try expect(local.url.scheme == "http")
    #else
    try expectThrow { _ = try Connection(address: "http://127.0.0.1:8080", token: "token", allowLocalHTTP: true) }
    #endif
}
