import Foundation
import ServiceManagement

/// `QuotadAgentGateway` over the real `SMAppService.agent(plistName:)`.
/// SM errors are normalized into `QuotadAgentError`: approval-denied becomes
/// `requiresApproval`, already-registered is success, anything else is
/// `registrationFailed` and triggers the self-install fallback.
/// Async because the SDK isolates `SMAppService` to the main actor.
struct LiveQuotadAgentGateway: QuotadAgentGateway {
    func status() async -> QuotadAgentStatus {
        let service = SMAppService.agent(plistName: QuotadService.agentPlistName)
        switch service.status {
        case .enabled: return .enabled
        case .notRegistered: return .notRegistered
        case .notFound: return .notFound
        case .requiresApproval: return .requiresApproval
        @unknown default: return .notFound
        }
    }

    func register() async throws {
        let service = SMAppService.agent(plistName: QuotadService.agentPlistName)
        do {
            try service.register()
        } catch {
            let code = (error as NSError).code
            if code == kSMErrorLaunchDeniedByUser {
                throw QuotadAgentError.requiresApproval
            }
            if code == kSMErrorAlreadyRegistered {
                return
            }
            throw QuotadAgentError.registrationFailed(error.localizedDescription)
        }
    }

    func openLoginItemsSettings() async {
        SMAppService.openSystemSettingsLoginItems()
    }
}

/// `QuotadFileStore` over the real bundle and home directory.
struct LiveQuotadFileStore: QuotadFileStore {
    private let bundledBinary: URL
    private let home: URL

    init(
        bundledBinaryURL: URL = Bundle.main.bundleURL
            .appendingPathComponent("Contents/MacOS/quotad"),
        homeDirectoryURL: URL = FileManager.default.homeDirectoryForCurrentUser
    ) {
        self.bundledBinary = bundledBinaryURL
        self.home = homeDirectoryURL
    }

    func bundledBinaryURL() -> URL { bundledBinary }

    func installedBinaryURL() -> URL {
        home.appendingPathComponent(".local/bin/quotad")
    }

    func selfInstalledPlistURL() -> URL {
        home.appendingPathComponent(
            "Library/LaunchAgents/\(QuotadService.agentPlistName)"
        )
    }

    /// Missing or byte-different from the bundled binary.
    func binaryNeedsInstall() throws -> Bool {
        guard FileManager.default.fileExists(atPath: installedBinaryURL().path) else {
            return true
        }
        let installed = try Data(contentsOf: installedBinaryURL())
        let bundled = try Data(contentsOf: bundledBinaryURL())
        return installed != bundled
    }

    func installBinary() throws {
        let fileManager = FileManager.default
        let destination = installedBinaryURL()
        try fileManager.createDirectory(
            at: destination.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        if fileManager.fileExists(atPath: destination.path) {
            try fileManager.removeItem(at: destination)
        }
        try fileManager.copyItem(at: bundledBinaryURL(), to: destination)
        try fileManager.setAttributes(
            [.posixPermissions: 0o755], ofItemAtPath: destination.path
        )
    }

    func selfInstalledPlistExists() -> Bool {
        FileManager.default.fileExists(atPath: selfInstalledPlistURL().path)
    }

    func writeSelfInstalledPlist(_ data: Data) throws {
        let fileManager = FileManager.default
        let destination = selfInstalledPlistURL()
        try fileManager.createDirectory(
            at: destination.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        try data.write(to: destination, options: .atomic)
    }

    func removeSelfInstalledPlist() throws {
        try FileManager.default.removeItem(at: selfInstalledPlistURL())
    }
}

/// `QuotadProcessRunner` over `Foundation.Process` (`/bin/launchctl`).
struct LiveQuotadProcessRunner: QuotadProcessRunner {
    struct LaunchError: Error {
        let executablePath: String
        let arguments: [String]
        let status: Int32
    }

    func run(executablePath: String, arguments: [String]) throws {
        let process = Process()
        process.executableURL = URL(filePath: executablePath)
        process.arguments = arguments
        try process.run()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            throw LaunchError(
                executablePath: executablePath,
                arguments: arguments,
                status: process.terminationStatus
            )
        }
    }
}
