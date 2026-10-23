import Foundation
import Observation

/// The main domain service that coordinates quota monitoring across AI providers.
/// Providers are rich domain models that own their own snapshots.
/// QuotaMonitor coordinates refreshes and alerts users when status changes.
///
/// Isolated to `@MainActor` because its `@Observable` state (`isMonitoring`) is
/// consumed by SwiftUI. This keeps the background monitoring loop from mutating observable state off the main actor — the
/// crash in issue #182 — and lets the compiler reject any future off-main
/// mutation.
@MainActor
@Observable
public final class QuotaMonitor {
    /// All registered providers
    private let providers: [AIProvider]

    /// Optional alerter for quota changes (e.g., system notifications)
    private let alerter: (any QuotaAlerter)?

    /// Clock for scheduling intervals (injectable for tests)
    private let clock: any Clock

    /// Optional power-state source for energy-aware monitoring. `nil` disables
    /// energy-awareness entirely (the plain timed loop), which is the default for
    /// tests; the app injects a real provider via the convenience init.
    private let powerStateProvider: (any PowerStateProvider)?

    /// Previous status for change detection
    private var previousStatuses: [String: QuotaStatus] = [:]

    /// Current monitoring task
    private var monitoringTask: Task<Void, Never>?

    /// Whether monitoring is active
    public private(set) var isMonitoring: Bool = false

    // MARK: - Initialization

    /// Creates a QuotaMonitor with a set of providers.
    public init(
        providers: [AIProvider],
        alerter: (any QuotaAlerter)? = nil,
        clock: any Clock,
        powerStateProvider: (any PowerStateProvider)? = nil
    ) {
        self.providers = providers
        self.alerter = alerter
        self.clock = clock
        self.powerStateProvider = powerStateProvider
    }

    // MARK: - Monitoring Operations

    /// A snapshot younger than this is not re-probed unless the refresh is forced.
    public static let minimumSnapshotAge: TimeInterval = 60

    /// Per-provider refresh in flight; concurrent callers join it instead of re-probing.
    @ObservationIgnored private var inFlight: [String: Task<Void, Never>] = [:]

    /// Refreshes every enabled, available provider concurrently. Without `force`,
    /// a provider whose snapshot is younger than `minimumSnapshotAge` is skipped.
    /// The popover, the HTTP feed and the background loop all come through here.
    public func refresh(force: Bool = false) async {
        await withTaskGroup(of: Void.self) { group in
            for provider in providers where provider.isEnabled {
                if !force, let capturedAt = provider.snapshot?.capturedAt,
                   Date().timeIntervalSince(capturedAt) < Self.minimumSnapshotAge {
                    continue
                }
                let task = inFlight[provider.id] ?? startRefresh(provider)
                group.addTask { await task.value }
            }
        }
    }

    private func startRefresh(_ provider: AIProvider) -> Task<Void, Never> {
        let task = Task {
            await refreshProvider(provider)
            inFlight[provider.id] = nil
        }
        inFlight[provider.id] = task
        return task
    }

    private func refreshProvider(_ provider: AIProvider) async {
        guard await provider.isAvailable() else {
            return
        }

        do {
            let snapshot = try await provider.refresh()
            await handleSnapshotUpdate(provider: provider, snapshot: snapshot)
        } catch {
            // Provider stores error in lastError - no need for external observer
        }
    }

    /// Handles snapshot update and alerts user if status changed
    private func handleSnapshotUpdate(provider: AIProvider, snapshot: UsageSnapshot) async {
        // Missing data is not a healthy recovery and must not trigger a status alert.
        guard !snapshot.quotas.isEmpty else { return }
        let previousStatus = previousStatuses[provider.id] ?? .healthy
        let newStatus = snapshot.overallStatus

        previousStatuses[provider.id] = newStatus

        // Alert user only if status changed
        if previousStatus != newStatus, let alerter = alerter {
            await alerter.alert(
                providerId: provider.id,
                previousStatus: previousStatus,
                currentStatus: newStatus
            )
        }
    }

    // MARK: - Queries

    /// Returns all providers
    public var allProviders: [AIProvider] {
        providers
    }

    /// Returns only enabled providers
    public var enabledProviders: [AIProvider] {
        providers.filter { $0.isEnabled }
    }

    /// Sets a provider's enabled state.
    public func setProviderEnabled(_ id: String, enabled: Bool) {
        providers.first { $0.id == id }?.isEnabled = enabled
    }

    // MARK: - Continuous Monitoring

    /// The hard lower bound on the monitoring interval. Background refresh must
    /// never poll faster than once a minute (energy — issue #67).
    public static let minimumInterval: Duration = .seconds(60)

    /// How much to stretch the background cadence while on battery, to reduce
    /// drain when unplugged (issue #204). Applied on top of the effective
    /// interval each tick, so plugging back in restores the normal cadence.
    public static let batteryIntervalMultiplier: Int = 2

    /// Clamps a requested interval to the 1-minute floor. Exposed so the floor
    /// can be unit-tested directly and so every caller funnels through one rule.
    public static func clampedInterval(_ interval: Duration) -> Duration {
        max(interval, minimumInterval)
    }

    /// Starts continuous monitoring: every tick refreshes all enabled providers
    /// (respecting `minimumSnapshotAge`), then sleeps for the requested interval
    /// clamped to the 1-minute floor. Returns the loop task so tests can await it.
    @discardableResult
    public func startMonitoring(interval: Duration = .seconds(60)) -> Task<Void, Never> {
        // Stop any existing monitoring
        monitoringTask?.cancel()

        isMonitoring = true

        let task = Task {
            // Iterator over power transitions; nil when energy-awareness is
            // disabled (no power provider), so the loop below behaves exactly
            // like the plain timed loop in that case.
            var powerEvents = self.powerStateProvider?.events().makeAsyncIterator()

            while !Task.isCancelled {
                // Energy awareness: while the display/system is asleep, pause
                // — no refresh and no probe subprocess spawn — and wait for
                // the next wake event so we can refresh immediately when the
                // user returns (issue #204). A nil power provider skips this
                // entirely. `AsyncStream.next()` resumes with nil on task
                // cancellation, so `stopMonitoring()` unparks the loop.
                while let power = self.powerStateProvider,
                      power.isDisplayAsleep,
                      !Task.isCancelled {
                    guard await powerEvents?.next() != nil else { break }
                    // Re-check `isDisplayAsleep`: a `.didWake` clears it and
                    // we fall through to an immediate refresh.
                }
                if Task.isCancelled { break }

                await self.refresh()

                var sleepInterval = Self.clampedInterval(interval)

                // Stretch the cadence while on battery to reduce drain (#204).
                if self.powerStateProvider?.isOnBattery == true {
                    sleepInterval = sleepInterval * Self.batteryIntervalMultiplier
                }

                do {
                    try await clock.sleep(for: sleepInterval)
                } catch {
                    break
                }
            }
        }
        monitoringTask = task
        return task
    }

    /// Stops continuous monitoring
    public func stopMonitoring() {
        isMonitoring = false
        monitoringTask?.cancel()
        monitoringTask = nil
    }
}
