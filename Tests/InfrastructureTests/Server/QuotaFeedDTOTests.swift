import Testing
import Foundation
import Mockable
@testable import Domain
@testable import Infrastructure

@Suite("QuotaFeedDTO mapping")
@MainActor
struct QuotaFeedDTOTests {

    private struct TestClock: Clock {
        func sleep(for duration: Duration) async throws {}
        func sleep(nanoseconds: UInt64) async throws {}
    }

    private static let fixedNow = Date(timeIntervalSince1970: 1_700_000_000)

    private func makeSettings(enabled: Bool = true) -> MockProviderSettingsRepository {
        MockRepositoryFactory.makeSettingsRepository(enabled: enabled)
    }

    @Test
    func `modelSpecific quota maps quotaKey and displayName label`() async {
        let settings = makeSettings()
        let probe = MockUsageProbe()
        given(probe).isAvailable().willReturn(true)
        let capturedAt = Self.fixedNow.addingTimeInterval(-120)
        given(probe).probe().willReturn(UsageSnapshot(
            providerId: "claude",
            quotas: [
                UsageQuota(
                    percentRemaining: 42,
                    quotaType: .modelSpecific("opus"),
                    providerId: "claude",
                    resetsAt: Date().addingTimeInterval(3 * 3600 + 60)
                ),
                UsageQuota(
                    percentRemaining: 17.6,
                    quotaType: .timeLimit("Monthly"),
                    providerId: "claude",
                    unitsUsed: 412,
                    unitsLimit: 500
                ),
                UsageQuota(
                    percentRemaining: nil,
                    quotaType: .timeLimit("Credits"),
                    providerId: "claude",
                    balanceRemaining: 750,
                    balanceUnit: .credits
                )
            ],
            capturedAt: capturedAt,
            accountTier: .claudeMax
        ))

        let claude = AIProvider(id: "claude", name: "Claude", probe: probe, settingsRepository: settings)
        let monitor = QuotaMonitor(providers: [claude], clock: TestClock())
        await monitor.refresh()

        let feed = QuotaFeedDTO.make(from: monitor.allProviders, at: Self.fixedNow)

        #expect(feed.providers.count == 1)
        let quotas = feed.providers[0].quotas
        #expect(quotas[0].key == "model:opus")
        #expect(quotas[0].label == "Opus")
        #expect(quotas[0].resetText == "Resets in 3h")
        #expect(quotas[0].percentRemaining == 42)
        #expect(quotas[1].key == "time:Monthly")
        #expect(quotas[1].label == "Monthly")
        #expect(quotas[1].resetText == nil)
        #expect(quotas[1].unitsUsed == 412)
        #expect(quotas[1].unitsLimit == 500)
        // Balance-only: no meaningful percentage.
        #expect(quotas[2].percentRemaining == nil)
        #expect(quotas[2].balanceRemaining == 750)
        #expect(quotas[2].balanceCap == nil)
        #expect(quotas[2].balanceUnit == "credits")
    }

    @Test
    func `nil snapshot with no error yields null capturedAt`() {
        let settings = makeSettings()
        let probe = MockUsageProbe()
        given(probe).isAvailable().willReturn(true)

        let claude = AIProvider(id: "claude", name: "Claude", probe: probe, settingsRepository: settings)

        let feed = QuotaFeedDTO.make(from: [claude], at: Self.fixedNow)

        #expect(feed.providers.count == 1)
        #expect(feed.providers[0].capturedAt == nil)
        #expect(feed.providers[0].ageSeconds == nil)
        #expect(feed.providers[0].unavailable == nil)
        #expect(feed.providers[0].throttledUntil == nil)
    }

    @Test
    func `empty quota snapshot remains unknown in feed`() async {
        let settings = makeSettings()
        let probe = MockUsageProbe()
        given(probe).probe().willReturn(UsageSnapshot(providerId: "muse", quotas: [], capturedAt: Self.fixedNow))
        let muse = AIProvider(id: "muse", name: "Muse", probe: probe, settingsRepository: settings)
        _ = try? await muse.refresh()

        let provider = QuotaFeedDTO.make(from: [muse], at: Self.fixedNow).providers[0]
        #expect(provider.status == "unknown")
        #expect(provider.unavailable == "No current quota data")
        #expect(provider.quotas.isEmpty)
    }

    @Test
    func `lastError populates unavailable`() async {
        let settings = makeSettings()
        let probe = MockUsageProbe()
        given(probe).isAvailable().willReturn(true)
        given(probe).probe().willThrow(ProbeError.authenticationRequired)

        let cursor = AIProvider(id: "cursor", name: "Cursor", probe: probe, settingsRepository: settings)
        let monitor = QuotaMonitor(providers: [cursor], clock: TestClock())
        await monitor.refresh()

        let feed = QuotaFeedDTO.make(from: monitor.allProviders, at: Self.fixedNow)

        #expect(feed.providers[0].unavailable == ProbeError.authenticationRequired.localizedDescription)
        #expect(feed.providers[0].throttledUntil == nil)
    }

    @Test
    func `rateLimited populates throttledUntil not unavailable`() async {
        let settings = makeSettings()
        let capturedAt = Self.fixedNow.addingTimeInterval(-600)
        let retryAt = Self.fixedNow.addingTimeInterval(3600)
        let probe = SequentialUsageProbe(
            results: [
                .success(UsageSnapshot(
                    providerId: "claude",
                    quotas: [UsageQuota(percentRemaining: 50, quotaType: .session, providerId: "claude")],
                    capturedAt: capturedAt
                )),
                .failure(ProbeError.rateLimited(retryAt: retryAt))
            ]
        )

        let claude = AIProvider(id: "claude", name: "Claude", probe: probe, settingsRepository: settings)
        let monitor = QuotaMonitor(providers: [claude], clock: TestClock())
        await monitor.refresh()
        await monitor.refresh(force: true)

        let feed = QuotaFeedDTO.make(from: monitor.allProviders, at: Self.fixedNow)

        #expect(feed.providers[0].throttledUntil == retryAt)
        #expect(feed.providers[0].unavailable == nil)
        #expect(feed.providers[0].capturedAt == capturedAt)
    }

    @Test
    func `disabled providers appear in disabledProviderIds not providers`() {
        let enabledSettings = makeSettings(enabled: true)
        let disabledSettings = makeSettings(enabled: false)

        let claude = AIProvider(id: "claude", name: "Claude", probe: MockUsageProbe(), settingsRepository: enabledSettings)
        let opencode = AIProvider(id: "opencode-go", name: "OpenCode Go", probe: MockUsageProbe(), settingsRepository: disabledSettings)

        let feed = QuotaFeedDTO.make(from: [claude, opencode], at: Self.fixedNow)

        #expect(feed.providers.map(\.id) == ["claude"])
        #expect(feed.disabledProviderIds == ["opencode-go"])
    }
}

private final class SequentialUsageProbe: UsageProbe, @unchecked Sendable {
    private let results: [Result<UsageSnapshot, Error>]
    nonisolated(unsafe) private var index = 0

    init(results: [Result<UsageSnapshot, Error>]) {
        self.results = results
    }

    func isAvailable() async -> Bool { true }

    func probe() async throws -> UsageSnapshot {
        defer { index += 1 }
        guard index < results.count else {
            throw ProbeError.executionFailed("SequentialUsageProbe: no more results")
        }
        switch results[index] {
        case .success(let snapshot):
            return snapshot
        case .failure(let error):
            throw error
        }
    }
}
