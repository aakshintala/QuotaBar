import Foundation
import Domain

/// Tri-state reachability of one provider id in quotad's feed: `unknown`
/// means quotad could not be reached at all, not that the provider is missing.
public enum FeedProviderAvailability: Sendable, Equatable {
    case available
    case unavailable
    case unknown
}

/// A `UsageProbe` that reads one provider's snapshot from quotad's local feed
/// instead of probing a provider API. All probes share one `FeedClient`, so one
/// HTTP request serves every provider of a refresh.
public struct FeedUsageProbe: UsageProbe, Sendable {
    public let id: String
    private let client: FeedClient

    public init(id: String, client: FeedClient = .shared) {
        self.id = id
        self.client = client
    }

    public func probe() async throws -> UsageSnapshot {
        let feed = try await client.fetch()
        guard let provider = feed.providers.first(where: { $0.id == id }) else {
            throw ProbeError.noCurrentQuota(reason: "quotad has no provider '\(id)'")
        }
        if let retryAt = provider.throttledUntil {
            throw ProbeError.rateLimited(retryAt: retryAt)
        }
        // The feed does not always stamp a capture time; derive it from the
        // feed's own clock so a missing `capturedAt` never invents one.
        let capturedAt: Date
        if let ageSeconds = provider.ageSeconds {
            capturedAt = feed.generatedAt.addingTimeInterval(-TimeInterval(ageSeconds))
        } else {
            capturedAt = feed.generatedAt
        }
        let snapshot = UsageSnapshot(
            providerId: id,
            quotas: provider.quotas.map { mapQuota($0) },
            capturedAt: capturedAt,
            accountTier: provider.tier.map(AccountTier.custom)
        )
        if snapshot.quotas.isEmpty {
            if let unavailable = provider.unavailable {
                throw ProbeError.executionFailed(unavailable)
            }
            throw ProbeError.noCurrentQuota(reason: "quotad has no data yet for provider '\(id)'")
        }
        // Stale quotas still display: `AIProvider.refresh()` keeps the snapshot
        // and records the message in `lastError`, so the popover shows the data
        // with its stale warning instead of clearing both.
        if let unavailable = provider.unavailable {
            throw ProbeError.staleSnapshot(snapshot, reason: unavailable)
        }
        return snapshot
    }

    /// False when quotad reports this id as disabled or not present. A daemon
    /// that cannot be reached reads as available so `probe()` can surface the
    /// connection error instead of the refresh silently skipping.
    public func isAvailable() async -> Bool {
        guard let feed = try? await client.fetch() else { return true }
        if feed.disabledProviderIds.contains(id) { return false }
        return feed.providers.contains { $0.id == id }
    }

    /// Tri-state reachability for settings UI: `unknown` when quotad cannot be
    /// reached at all, distinct from a reachable feed that omits this id.
    public func availability() async -> FeedProviderAvailability {
        guard let feed = try? await client.fetch() else { return .unknown }
        if feed.disabledProviderIds.contains(id) { return .unavailable }
        return feed.providers.contains { $0.id == id } ? .available : .unavailable
    }

    // MARK: - Mapping

    private func mapQuota(_ quota: QuotaFeedQuotaDTO) -> UsageQuota {
        UsageQuota(
            percentRemaining: quota.percentRemaining,
            quotaType: Self.quotaType(for: quota.key),
            providerId: id,
            resetsAt: quota.resetsAt,
            balanceRemaining: quota.balanceRemaining,
            balanceUsed: quota.balanceUsed,
            balanceCap: quota.balanceCap,
            balanceUnit: Self.balanceUnit(for: quota.balanceUnit),
            unitsUsed: quota.unitsUsed,
            unitsLimit: quota.unitsLimit
        )
    }

    /// The inverse of `QuotaType.quotaKey`.
    static func quotaType(for key: String) -> QuotaType {
        switch key {
        case "session":
            return .session
        case "weekly":
            return .weekly
        default:
            if key.hasPrefix("model:") {
                return .modelSpecific(String(key.dropFirst("model:".count)))
            }
            if key.hasPrefix("time:") {
                return .timeLimit(String(key.dropFirst("time:".count)))
            }
            return .timeLimit(key)
        }
    }

    private static func balanceUnit(for raw: String?) -> BalanceUnit? {
        switch raw {
        case "usd": return .usd
        case "credits": return .credits
        default: return nil
        }
    }
}
