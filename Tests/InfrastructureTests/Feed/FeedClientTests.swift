import Testing
import Foundation
import Mockable
@testable import Infrastructure
@testable import Domain

@Suite("Feed client of quotad")
struct FeedClientTests {

    // MARK: - Helpers

    private func repoRoot() -> URL {
        var url = URL(filePath: #filePath)
        // Tests/InfrastructureTests/Feed/FeedClientTests.swift -> repo root.
        for _ in 0..<4 { url.deleteLastPathComponent() }
        return url
    }

    private func fixtureData(_ path: String) throws -> Data {
        try Data(contentsOf: repoRoot().appendingPathComponent(path))
    }

    private func decodeFixture(_ path: String) throws -> QuotaFeedDTO {
        try feedDecoder().decode(QuotaFeedDTO.self, from: fixtureData(path))
    }

    private func client(
        serving data: Data,
        statusCode: Int = 200,
        port: Int = 8787
    ) -> (FeedClient, MockNetworkClient) {
        let network = MockNetworkClient()
        let response = httpResponse("http://127.0.0.1:\(port)/quotas", statusCode: statusCode)
        given(network).request(.any).willReturn((data, response))
        let settings = JSONSettingsRepository(
            fileURL: FileManager.default.temporaryDirectory
                .appendingPathComponent(UUID().uuidString + ".json"))
        settings.update { $0.feed.port = port }
        return (FeedClient(settings: settings, networkClient: network), network)
    }

    // MARK: - Fixture decoding

    @Test
    func `decodes the live quotad fixture with five providers`() throws {
        let feed = try decodeFixture("Tests/Support/Fixtures/quotad-quotas.json")

        #expect(feed.providers.map(\.id) == ["claude", "codex", "cursor", "opencode-go", "muse"])
        #expect(feed.disabledProviderIds.isEmpty)

        let claude = try #require(feed.providers.first { $0.id == "claude" })
        let weekly = try #require(claude.quotas.first { $0.key == "weekly" })
        #expect(weekly.percentRemaining == 90)
        #expect(claude.tier == "Ultra")

        let cursor = try #require(feed.providers.first { $0.id == "cursor" })
        let monthly = try #require(cursor.quotas.first { $0.key == "time:Monthly" })
        #expect(monthly.unitsUsed == 15927)
        #expect(monthly.unitsLimit == 27757)

        let opencode = try #require(feed.providers.first { $0.id == "opencode-go" })
        let opencodeMonthly = try #require(opencode.quotas.first { $0.key == "time:Monthly" })
        #expect(opencodeMonthly.percentRemaining == 0)
        #expect(opencodeMonthly.status == "depleted")
    }

    @Test
    func `decodes the Swift feed fixture with five providers`() throws {
        let feed = try decodeFixture("quotad/feed/testdata/swift-quotas.json")

        #expect(feed.providers.map(\.id) == ["claude", "codex", "cursor", "opencode-go", "muse"])

        let cursor = try #require(feed.providers.first { $0.id == "cursor" })
        let monthly = try #require(cursor.quotas.first { $0.key == "time:Monthly" })
        #expect(monthly.unitsUsed == 14586)
        #expect(monthly.unitsLimit == 26563)

        let muse = try #require(feed.providers.first { $0.id == "muse" })
        #expect(muse.tier == "Muse Code Power Usage")
        #expect(muse.quotas.count == 2)
    }

    // MARK: - Mapping

    @Test
    func `probe maps quota keys back to QuotaType with tier and capturedAt`() async throws {
        let (client, _) = client(serving: try fixtureData("Tests/Support/Fixtures/quotad-quotas.json"))

        let snapshot = try await FeedUsageProbe(id: "cursor", client: client).probe()

        let monthly = try #require(snapshot.quota(for: .timeLimit("Monthly")))
        #expect(monthly.percentRemaining == 42.621188630490956)
        #expect(monthly.unitsUsed == 15927)
        #expect(monthly.unitsLimit == 27757)
        #expect(snapshot.accountTier == .custom("PRO"))
        // ageSeconds is 0, so capturedAt is the feed's generatedAt.
        #expect(snapshot.capturedAt == parseISO8601("2026-11-05T23:23:51Z"))
    }

    @Test
    func `probe maps session weekly and model keys`() async throws {
        let (client, _) = client(serving: try fixtureData("Tests/Support/Fixtures/quotad-quotas.json"))

        let snapshot = try await FeedUsageProbe(id: "claude", client: client).probe()

        #expect(snapshot.quota(for: .weekly)?.percentRemaining == 90)
        #expect(snapshot.quota(for: .modelSpecific("fable"))?.percentRemaining == 100)
        #expect(snapshot.accountTier == .custom("Ultra"))
    }

    @Test
    func `probe maps the session key to session`() async throws {
        let (client, _) = client(serving: try fixtureData("Tests/Support/Fixtures/quotad-quotas.json"))

        let snapshot = try await FeedUsageProbe(id: "opencode-go", client: client).probe()

        #expect(snapshot.quota(for: .session)?.percentRemaining == 100)
    }

    @Test
    func `decoder tolerates fractional-second timestamps`() throws {
        let json = """
        {"generatedAt":"2026-11-05T23:23:51.123Z","providers":[{"id":"codex","name":"Codex",
        "capturedAt":"2026-11-05T23:23:51.456Z","status":"healthy",
        "quotas":[{"key":"weekly","label":"Weekly","percentRemaining":76,"status":"healthy"}]}],
        "disabledProviderIds":[]}
        """
        let feed = try feedDecoder().decode(QuotaFeedDTO.self, from: Data(json.utf8))

        #expect(feed.generatedAt == parseISO8601("2026-11-05T23:23:51.123Z"))
        #expect(feed.providers.first?.capturedAt == parseISO8601("2026-11-05T23:23:51.456Z"))
    }

    @Test
    func `quotas with unavailable throws staleSnapshot carrying the data`() async throws {
        let json = """
        {"generatedAt":"2026-11-05T23:23:51Z","providers":[{"id":"cursor","name":"Cursor",
        "capturedAt":"2026-11-05T23:23:50Z","ageSeconds":1,
        "status":"warning","unavailable":"quota expired",
        "quotas":[{"key":"time:Monthly","label":"Monthly","percentRemaining":42.5,
        "status":"warning"}]}],"disabledProviderIds":[]}
        """
        let (client, _) = client(serving: Data(json.utf8))

        let error = await #expect(throws: ProbeError.self) {
            try await FeedUsageProbe(id: "cursor", client: client).probe()
        }
        guard case .staleSnapshot(let snapshot, let reason) = error else {
            Issue.record("expected staleSnapshot, got \(String(describing: error))")
            return
        }
        #expect(reason == "quota expired")
        #expect(snapshot.providerId == "cursor")
        #expect(snapshot.quota(for: .timeLimit("Monthly"))?.percentRemaining == 42.5)
        #expect(snapshot.capturedAt == parseISO8601("2026-11-05T23:23:50Z"))
    }

    @Test
    func `empty quotas without unavailable throws saying quotad has no data yet`() async throws {
        let json = """
        {"generatedAt":"2026-11-05T23:23:51Z","providers":[{"id":"cursor","name":"Cursor",
        "status":"unknown","quotas":[]}],"disabledProviderIds":[]}
        """
        let (client, _) = client(serving: Data(json.utf8))

        let error = await #expect(throws: ProbeError.self) {
            try await FeedUsageProbe(id: "cursor", client: client).probe()
        }
        #expect(error == .noCurrentQuota(reason: "quotad has no data yet for provider 'cursor'"))
    }

    @Test
    func `capturedAt falls back to generatedAt without ageSeconds`() async throws {
        let json = """
        {"generatedAt":"2026-11-05T23:23:51Z","providers":[{"id":"codex","name":"Codex",
        "status":"healthy",
        "quotas":[{"key":"weekly","label":"Weekly","percentRemaining":76,"status":"healthy"}]}],
        "disabledProviderIds":[]}
        """
        let (client, _) = client(serving: Data(json.utf8))

        let snapshot = try await FeedUsageProbe(id: "codex", client: client).probe()

        #expect(snapshot.capturedAt == parseISO8601("2026-11-05T23:23:51Z"))
    }

    @Test
    func `probe maps balance meters`() async throws {
        let json = """
        {"generatedAt":"2026-11-05T23:23:51Z","providers":[{"id":"codex","name":"Codex",
        "capturedAt":"2026-11-05T23:23:51Z","status":"healthy",
        "quotas":[{"key":"weekly","label":"Weekly","percentRemaining":76,
        "balanceRemaining":9.5,"balanceUsed":2.5,"balanceCap":12.0,"balanceUnit":"usd",
        "status":"healthy"}]}],"disabledProviderIds":[]}
        """
        let (client, _) = client(serving: Data(json.utf8))

        let snapshot = try await FeedUsageProbe(id: "codex", client: client).probe()

        let weekly = try #require(snapshot.quota(for: .weekly))
        #expect(weekly.balanceRemaining == Decimal(string: "9.5"))
        #expect(weekly.balanceUsed == Decimal(string: "2.5"))
        #expect(weekly.balanceCap == Decimal(string: "12.0"))
        #expect(weekly.balanceUnit == .usd)
    }

    // MARK: - Error cases

    @Test
    func `throttled provider throws rateLimited`() async throws {
        let json = """
        {"generatedAt":"2026-11-05T23:23:51Z","providers":[{"id":"claude","name":"Claude",
        "capturedAt":"2026-11-05T23:22:43Z","status":"healthy",
        "throttledUntil":"2026-11-06T00:22:43Z","quotas":[]}],"disabledProviderIds":[]}
        """
        let (client, _) = client(serving: Data(json.utf8))

        await #expect(throws: ProbeError.rateLimited(retryAt: parseISO8601("2026-11-06T00:22:43Z")!)) {
            try await FeedUsageProbe(id: "claude", client: client).probe()
        }
    }

    @Test
    func `unavailable provider with no quotas throws carrying the message`() async throws {
        let json = """
        {"generatedAt":"2026-11-05T23:23:51Z","providers":[{"id":"cursor","name":"Cursor",
        "status":"unknown","unavailable":"Cursor credentials not found","quotas":[]}],
        "disabledProviderIds":[]}
        """
        let (client, _) = client(serving: Data(json.utf8))

        let error = await #expect(throws: ProbeError.self) {
            try await FeedUsageProbe(id: "cursor", client: client).probe()
        }
        #expect(error?.localizedDescription == "Cursor credentials not found")
    }

    @Test
    func `disabled or missing provider is not available`() async throws {
        let json = """
        {"generatedAt":"2026-11-05T23:23:51Z","providers":[{"id":"claude","name":"Claude",
        "status":"healthy","quotas":[]}],"disabledProviderIds":["cursor"]}
        """
        let (client, _) = client(serving: Data(json.utf8))

        #expect(await FeedUsageProbe(id: "cursor", client: client).isAvailable() == false)
        #expect(await FeedUsageProbe(id: "codex", client: client).isAvailable() == false)
        #expect(await FeedUsageProbe(id: "claude", client: client).isAvailable() == true)
        #expect(await FeedUsageProbe(id: "cursor", client: client).availability() == .unavailable)
        #expect(await FeedUsageProbe(id: "codex", client: client).availability() == .unavailable)
        #expect(await FeedUsageProbe(id: "claude", client: client).availability() == .available)
    }

    @Test
    func `unreachable daemon is unknown yet still available for the refresh path`() async throws {
        let network = MockNetworkClient()
        given(network).request(.any).willThrow(URLError(.cannotConnectToHost))
        let settings = JSONSettingsRepository(
            fileURL: FileManager.default.temporaryDirectory
                .appendingPathComponent(UUID().uuidString + ".json"))
        settings.update { $0.feed.port = 8787 }
        let client = FeedClient(settings: settings, networkClient: network)

        #expect(await FeedUsageProbe(id: "claude", client: client).availability() == .unknown)
        #expect(await FeedUsageProbe(id: "claude", client: client).isAvailable() == true)
    }

    @Test
    func `unreachable daemon throws saying quotad is not running`() async throws {
        let network = MockNetworkClient()
        given(network).request(.any).willThrow(URLError(.cannotConnectToHost))
        let settings = JSONSettingsRepository(
            fileURL: FileManager.default.temporaryDirectory
                .appendingPathComponent(UUID().uuidString + ".json"))
        settings.update { $0.feed.port = 8787 }
        let client = FeedClient(settings: settings, networkClient: network)

        let error = await #expect(throws: ProbeError.self) {
            try await FeedUsageProbe(id: "claude", client: client).probe()
        }
        #expect(error?.localizedDescription.contains("quotad is not running on 127.0.0.1:8787") == true)
    }

    // MARK: - Sharing

    @Test
    func `one HTTP call serves all five providers within the reuse window`() async throws {
        let data = try fixtureData("Tests/Support/Fixtures/quotad-quotas.json")
        let (client, network) = client(serving: data)
        let ids = ["claude", "codex", "cursor", "opencode-go", "muse"]

        let snapshots = try await withThrowingTaskGroup(of: UsageSnapshot.self) { group in
            for id in ids {
                group.addTask { try await FeedUsageProbe(id: id, client: client).probe() }
            }
            var collected: [UsageSnapshot] = []
            for try await snapshot in group { collected.append(snapshot) }
            return collected
        }

        #expect(snapshots.count == 5)
        #expect(Set(snapshots.map(\.providerId)) == Set(ids))
        verify(network).request(.any).called(1)
    }
}
