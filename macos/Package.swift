// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "Mailorva",
    platforms: [.macOS(.v13)],
    products: [.executable(name: "Mailorva", targets: ["MailSearchApp"])],
    targets: [
        .target(name: "MailSearchCore"),
        .executableTarget(name: "MailSearchApp", dependencies: ["MailSearchCore"]),
        .executableTarget(name: "MailSearchChecks", dependencies: ["MailSearchCore"], path: "Tests/MailSearchCoreTests")
    ]
)
