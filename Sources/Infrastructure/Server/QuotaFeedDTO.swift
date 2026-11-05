import Foundation

// MARK: - Wire Types

public struct QuotaFeedDTO: Codable, Sendable, Equatable {
    public let generatedAt: Date
    public let providers: [QuotaFeedProviderDTO]
    public let disabledProviderIds: [String]

    public init(generatedAt: Date, providers: [QuotaFeedProviderDTO], disabledProviderIds: [String]) {
        self.generatedAt = generatedAt
        self.providers = providers
        self.disabledProviderIds = disabledProviderIds
    }
}

public struct QuotaFeedProviderDTO: Codable, Sendable, Equatable {
    public let id: String
    public let name: String
    public let tier: String?
    public let capturedAt: Date?
    public let ageSeconds: Int?
    public let status: String
    public let unavailable: String?
    public let throttledUntil: Date?
    public let quotas: [QuotaFeedQuotaDTO]

    public init(
        id: String,
        name: String,
        tier: String?,
        capturedAt: Date?,
        ageSeconds: Int?,
        status: String,
        unavailable: String?,
        throttledUntil: Date?,
        quotas: [QuotaFeedQuotaDTO]
    ) {
        self.id = id
        self.name = name
        self.tier = tier
        self.capturedAt = capturedAt
        self.ageSeconds = ageSeconds
        self.status = status
        self.unavailable = unavailable
        self.throttledUntil = throttledUntil
        self.quotas = quotas
    }
}

public struct QuotaFeedQuotaDTO: Codable, Sendable, Equatable {
    public let key: String
    public let label: String
    /// nil for a balance-only bucket, which has no meaningful percentage.
    public let percentRemaining: Double?
    public let resetsAt: Date?
    /// Reset time only ("Resets in 3h"), derived from `resetsAt`.
    public let resetText: String?
    public let status: String
    public let balanceRemaining: Decimal?
    public let balanceUsed: Decimal?
    public let balanceCap: Decimal?
    /// "usd" or "credits".
    public let balanceUnit: String?
    public let unitsUsed: Int?
    public let unitsLimit: Int?

    public init(
        key: String,
        label: String,
        percentRemaining: Double?,
        resetsAt: Date?,
        resetText: String?,
        status: String,
        balanceRemaining: Decimal? = nil,
        balanceUsed: Decimal? = nil,
        balanceCap: Decimal? = nil,
        balanceUnit: String? = nil,
        unitsUsed: Int? = nil,
        unitsLimit: Int? = nil
    ) {
        self.key = key
        self.label = label
        self.percentRemaining = percentRemaining
        self.resetsAt = resetsAt
        self.resetText = resetText
        self.status = status
        self.balanceRemaining = balanceRemaining
        self.balanceUsed = balanceUsed
        self.balanceCap = balanceCap
        self.balanceUnit = balanceUnit
        self.unitsUsed = unitsUsed
        self.unitsLimit = unitsLimit
    }
}

// The snapshot builder that produced this DTO used to live here. quotad now
// owns the feed; this file keeps only the wire types the feed client decodes.
