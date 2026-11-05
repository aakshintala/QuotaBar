import Foundation
import Domain

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
        if provider.quotas.isEmpty, let unavailable = provider.unavailable {
            throw ProbeError.executionFailed(unavailable)
        }
        return UsageSnapshot(
            providerId: id,
            quotas: provider.quotas.map { mapQuota($0) },
            capturedAt: provider.capturedAt ?? Date(),
            accountTier: provider.tier.map(AccountTier.custom)
        )
    }

    /// False when quotad reports this id as disabled or not present. A daemon
    /// that cannot be reached reads as available so `probe()` can surface the
    /// connection error instead of the refresh silently skipping.
    public func isAvailable() async -> Bool {
        guard let feed = try? await client.fetch() else { return true }
        if feed.disabledProviderIds.contains(id) { return false }
        return feed.providers.contains { $0.id == id }
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
