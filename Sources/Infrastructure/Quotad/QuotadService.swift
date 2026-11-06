import Foundation
import Mockable
import Observation

/// Observable state of the quotad daemon as managed by the app.
public enum QuotadServiceState: Equatable, Sendable {
    /// quotad is registered with launchd (bundled agent or a previous
    /// self-install bootstrapped it). The feed error path still reports an
    /// unreachable daemon, so this is also the optimistic initial state.
    case runningViaLaunchd
    /// The user must allow QuotaBar in System Settings › General › Login Items.
    case needsApproval
    /// The bundled agent could not register; quotad runs from ~/.local/bin
    /// via a self-installed LaunchAgent plist (the old mac-deploy.sh steps).
    case selfInstalled
    /// Neither registration nor the fallback worked.
    case failed(String)
}

/// Normalized `SMAppService.status` for the bundled quotad LaunchAgent.
public enum QuotadAgentStatus: String, Sendable, Equatable {
    case enabled
    case notRegistered
    case notFound
    case requiresApproval
}

/// Normalized registration failure from the agent gateway.
public enum QuotadAgentError: Error, Sendable, Equatable {
    /// Registering needs the user to allow QuotaBar in Login Items.
    case requiresApproval
    /// Any other registration failure; the service falls back to self-install.
    case registrationFailed(String)
}

/// Test seam over `SMAppService`'s LaunchAgent API. The live implementation
/// normalizes SM errors into `QuotadAgentError` so the service — and its
/// tests — never touch ServiceManagement types.
@Mockable
public protocol QuotadAgentGateway: Sendable {
    func status() async -> QuotadAgentStatus
    func register() async throws
    func openLoginItemsSettings() async
}

/// Test seam over the self-install file work (the old mac-deploy.sh steps).
@Mockable
public protocol QuotadFileStore: Sendable {
    /// The quotad binary inside this app's bundle (`Contents/MacOS/quotad`).
    func bundledBinaryURL() -> URL
    /// `~/.local/bin/quotad`.
    func installedBinaryURL() -> URL
    /// `~/Library/LaunchAgents/com.aakshintala.quotad.plist`.
    func selfInstalledPlistURL() -> URL
    /// True when the installed binary is missing or differs from the bundled one.
    func binaryNeedsInstall() throws -> Bool
    /// Copies the bundled binary to the installed path.
    func installBinary() throws
    /// True when a self-installed plist from a previous fallback exists.
    func selfInstalledPlistExists() -> Bool
    /// Writes the given plist bytes to the self-installed path.
    func writeSelfInstalledPlist(_ data: Data) throws
    /// Removes the self-installed plist.
    func removeSelfInstalledPlist() throws
}

/// Test seam over `/bin/launchctl`. Arguments are arrays, never a shell string.
@Mockable
public protocol QuotadProcessRunner: Sendable {
    func run(executablePath: String, arguments: [String]) throws
}

/// Registers the bundled quotad LaunchAgent at launch and keeps exactly one
/// copy of quotad running.
///
/// Flow (`ensureRegistered`, called once at launch):
/// - `.enabled` → nothing to do.
/// - `.requiresApproval` → state `needsApproval`; the UI points at Login Items.
/// - `.notRegistered` / `.notFound` → `register()`; on success any leftover
///   self-install is booted out and removed, on approval failure the state is
///   `needsApproval`, on any other error the service falls back to the
///   self-install (copy binary, write plist, bootout + bootstrap).
@MainActor
@Observable
public final class QuotadService {
    public nonisolated static let agentLabel = "com.aakshintala.quotad"
    public nonisolated static let agentPlistName = "com.aakshintala.quotad.plist"
    public nonisolated static let launchctlPath = "/bin/launchctl"

    public private(set) var state: QuotadServiceState = .runningViaLaunchd

    private let agent: any QuotadAgentGateway
    private let files: any QuotadFileStore
    private let process: any QuotadProcessRunner
    private let homeDirectoryURL: URL
    private let uid: UInt32

    public init(
        agent: any QuotadAgentGateway,
        files: any QuotadFileStore,
        process: any QuotadProcessRunner,
        homeDirectoryURL: URL,
        uid: UInt32 = getuid()
    ) {
        self.agent = agent
        self.files = files
        self.process = process
        self.homeDirectoryURL = homeDirectoryURL
        self.uid = uid
    }

    /// Live service for the running app: bundled binary, real SMAppService,
    /// real files and launchctl.
    public convenience init() {
        self.init(
            agent: LiveQuotadAgentGateway(),
            files: LiveQuotadFileStore(),
            process: LiveQuotadProcessRunner(),
            homeDirectoryURL: FileManager.default.homeDirectoryForCurrentUser
        )
    }

    /// Registers the bundled LaunchAgent, falling back to a self-install.
    public func ensureRegistered() async {
        switch await agent.status() {
        case .enabled:
            AppLog.probes.info("quotad LaunchAgent already registered")
        case .requiresApproval:
            AppLog.probes.warning("quotad LaunchAgent requires approval in Login Items")
            state = .needsApproval
        case .notRegistered, .notFound:
            await registerOrFallback()
        }
    }

    /// Opens System Settings at the Login Items panel (approval banner button).
    public func openLoginItemsSettings() async {
        await agent.openLoginItemsSettings()
    }

    // MARK: - Registration

    private func registerOrFallback() async {
        do {
            try await agent.register()
            AppLog.probes.info("quotad LaunchAgent registered")
            removeSelfInstallIfPresent()
            if case .failed = state { return }
            state = .runningViaLaunchd
        } catch QuotadAgentError.requiresApproval {
            AppLog.probes.warning("quotad registration requires approval in Login Items")
            state = .needsApproval
        } catch {
            AppLog.probes.warning("quotad registration failed (\(error.localizedDescription)); self-installing")
            selfInstall()
        }
    }

    /// After a successful registration, bootout and remove a leftover
    /// self-installed copy so only one quotad runs.
    private func removeSelfInstallIfPresent() {
        guard files.selfInstalledPlistExists() else { return }
        AppLog.probes.info("removing leftover self-installed quotad")
        try? process.run(
            executablePath: Self.launchctlPath,
            arguments: ["bootout", launchdDomain()]
        )
        do {
            try files.removeSelfInstalledPlist()
        } catch {
            AppLog.probes.error("could not remove self-installed quotad plist: \(error.localizedDescription)")
            state = .failed(error.localizedDescription)
        }
    }

    // MARK: - Self-install fallback

    private func selfInstall() {
        do {
            if try files.binaryNeedsInstall() {
                try files.installBinary()
                AppLog.probes.info("installed quotad to ~/.local/bin")
            } else {
                AppLog.probes.info("installed quotad binary is current; skipping copy")
            }
            try files.writeSelfInstalledPlist(Self.selfInstalledPlistData(homeDirectory: homeDirectoryURL))
            // Bootout ignores failure: the job may simply not be loaded.
            try? process.run(
                executablePath: Self.launchctlPath,
                arguments: ["bootout", launchdDomain()]
            )
            try process.run(
                executablePath: Self.launchctlPath,
                arguments: ["bootstrap", "gui/\(uid)", files.selfInstalledPlistURL().path]
            )
            AppLog.probes.info("self-installed quotad via launchctl")
            state = .selfInstalled
        } catch {
            AppLog.probes.error("quotad self-install failed: \(error.localizedDescription)")
            state = .failed(error.localizedDescription)
        }
    }

    private func launchdDomain() -> String {
        "gui/\(uid)/\(Self.agentLabel)"
    }

    // MARK: - Self-installed plist

    /// LaunchAgent plist for the `~/.local/bin` fallback: absolute paths, the
    /// same keys as the bundled plist except `BundleProgram` (meaningless
    /// outside the bundle; launchd runs `ProgramArguments[0]`).
    public nonisolated static func selfInstalledPlistData(homeDirectory: URL) -> Data {
        let binary = homeDirectory
            .appendingPathComponent(".local/bin/quotad").path
        let log = homeDirectory
            .appendingPathComponent("Library/Logs/QuotaBar/quotad.log").path
        let plist: [String: Any] = [
            "Label": agentLabel,
            "ProgramArguments": [binary, "-log", log],
            "RunAtLoad": true,
            "KeepAlive": ["SuccessfulExit": false],
            "ThrottleInterval": 10,
            "AssociatedBundleIdentifiers": ["com.aakshintala.subscriptionusagebar"],
        ]
        // The dictionary is plist-shaped by construction.
        // swiftlint:disable:next force_try
        return try! PropertyListSerialization.data(
            fromPropertyList: plist, format: .xml, options: 0
        )
    }
}
