import Foundation
import Observation

/// An AI provider: identity plus the observable state of its last refresh.
/// Every provider is this one class; what differs is the `UsageProbe` it is given.
/// Adding a provider = write a probe + one registration line in `QuotaBarApp`.
///
/// `@MainActor` isolates the observable state (isSyncing/snapshot/lastError) to the main
/// actor so its cheap writes land on the same thread the readers (QuotaMonitor, SwiftUI)
/// run on. The heavy probe work stays off-main: `refresh()` suspends at the non-isolated
/// `await probe.probe()`, which runs on the global executor.
@MainActor
@Observable
public final class AIProvider: Identifiable {
    /// Unique identifier; also the settings key and feed id (e.g. "claude", "opencode-go")
    public let id: String

    /// Display name (e.g. "Claude", "OpenCode Go")
    public let name: String

    /// Whether the provider is enabled (persisted via settingsRepository)
    public var isEnabled: Bool {
        didSet { settingsRepository.setEnabled(isEnabled, forProvider: id) }
    }

    public private(set) var isSyncing: Bool = false
    public private(set) var snapshot: UsageSnapshot?
    public private(set) var lastError: Error?

    private let probe: any UsageProbe
    private let settingsRepository: any ProviderSettingsRepository

    public init(
        id: String,
        name: String,
        probe: any UsageProbe,
        settingsRepository: any ProviderSettingsRepository
    ) {
        self.id = id
        self.name = name
        self.probe = probe
        self.settingsRepository = settingsRepository
        self.isEnabled = settingsRepository.isEnabled(forProvider: id)
    }

    /// Checks if the provider is available (credentials present, etc.)
    public func isAvailable() async -> Bool {
        await probe.isAvailable()
    }

    /// Refreshes the usage data and updates the snapshot.
    /// Sets isSyncing during refresh and captures any errors.
    @discardableResult
    public func refresh() async throws -> UsageSnapshot {
        isSyncing = true
        defer { isSyncing = false }

        do {
            let newSnapshot = try await probe.probe()
            snapshot = newSnapshot
            lastError = nil
            return newSnapshot
        } catch {
            if case ProbeError.accountChanged = error {
                snapshot = nil
            } else if case ProbeError.staleSnapshot(let lastKnown, _) = error {
                snapshot = lastKnown
            } else if case ProbeError.noCurrentQuota = error {
                snapshot = nil
            }
            lastError = error
            throw error
        }
    }
}

import Mockable

/// Protocol defining how to probe for usage data.
/// This is an internal implementation detail - callers use AIProvider.refresh() instead.
@Mockable
public protocol UsageProbe: Sendable {
    /// Fetches the current usage snapshot
    func probe() async throws -> UsageSnapshot

    /// Checks if the probe is available (CLI installed, credentials present, etc.)
    func isAvailable() async -> Bool
}
