import Foundation
import CryptoKit
import Testing
import Mockable
@testable import Infrastructure
@testable import Domain

@Suite struct MuseUsageProbeTests {
    private func authFile(_ directory: URL, token: String = "dca:test-oauth") throws -> MuseCredentialLoader {
        let auth = directory.appendingPathComponent("auth.json")
        try JSONSerialization.data(withJSONObject: ["providers": ["meta": [
            "mechanism": "oauth", "access_token": token, "api_key": "standing-inference-key"
        ]]]).write(to: auth)
        return MuseCredentialLoader(environment: ["MUSE_AUTH_PATH": auth.path])
    }

    private func response(_ body: String, status: Int = 200) -> (Data, URLResponse) {
        (Data(body.utf8), httpResponse("https://api.meta.ai/muse-code/key", statusCode: status))
    }

    private let body = #"{"is_subs_active":true,"subs_tier_name":"Muse Pro","api_key":"returned-secret","user_email":"private@example.invalid","subs_usage":{"window":{"used_percent":25,"window_duration_mins":300,"resets_at":2000000300},"weekly":{"used_percent":40,"resets_at":2000600000}}}"#

    @Test func parsesUsageAndDiscardsReturnedAccountData() throws {
        let snapshot = try MuseUsageProbe.parseUsageResponse(Data(body.utf8), capturedAt: Date(timeIntervalSince1970: 2_000_000_000))
        #expect(snapshot.providerId == "muse")
        #expect(snapshot.accountTier == .custom("Muse Pro"))
        #expect(snapshot.quotas.count == 2)
        #expect(snapshot.quotas[0].quotaType == .session)
        #expect(snapshot.quotas[0].percentRemaining == 75)
        #expect(snapshot.quotas[1].quotaType == .weekly)
        #expect(snapshot.quotas[1].percentRemaining == 60)
    }

    @Test func omittedOrExpiredWindowsAreUnknownRatherThanFull() throws {
        let idle = try MuseUsageProbe.parseUsageResponse(Data(#"{"is_subs_active":true}"#.utf8), capturedAt: Date(timeIntervalSince1970: 2_000_000_000))
        #expect(idle.quotas.isEmpty)
        let expired = #"{"is_subs_active":true,"subs_usage":{"window":{"used_percent":0,"resets_at":1999999999},"weekly":{"used_percent":20,"resets_at":2000000001}}}"#
        let partial = try MuseUsageProbe.parseUsageResponse(Data(expired.utf8), capturedAt: Date(timeIntervalSince1970: 2_000_000_000))
        #expect(partial.quotas.count == 1)
        #expect(partial.quotas[0].quotaType == .weekly)
    }

    @Test func credentialMustBeOAuthAndFileIsReadOnly() throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let loader = try authFile(directory)
        let file = directory.appendingPathComponent("auth.json")
        let before = try Data(contentsOf: file)
        guard case .credentials(let credentials) = loader.load() else { Issue.record("Expected valid Muse credentials"); return }
        #expect(credentials.accessToken == "dca:test-oauth")
        #expect(try Data(contentsOf: file) == before)
        guard case .credentials = loader.load() else { Issue.record("Expected credentials"); return }
        try JSONSerialization.data(withJSONObject: ["providers": ["meta": ["mechanism": "api_key", "access_token": "inference-only", "api_key": "key"]]]).write(to: file)
        guard case .unavailable = loader.load() else { Issue.record("API-key login must not count as OAuth"); return }
    }

    @Test func validOAuthWithoutStandingKeyHasDistinctActionableOutcome() async throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let auth = directory.appendingPathComponent("auth.json")
        try JSONSerialization.data(withJSONObject: ["providers": ["meta": ["mechanism": "oauth", "access_token": "dca:valid-oauth"]]]).write(to: auth)
        let loader = MuseCredentialLoader(environment: ["MUSE_AUTH_PATH": auth.path])
        #expect(await MuseUsageProbe(credentialLoader: loader).isAvailable())
        let probe = MuseUsageProbe(credentialLoader: loader, cacheDirectory: directory.appendingPathComponent("cache"))
        await #expect(throws: ProbeError.standingKeyRequired) { try await probe.probe() }
    }

    @Test func requestUsesFixedOAuthEndpointAndPersistsNoReturnedSecretsOrRefreshesWithinCooldown() async throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let loader = try authFile(directory)
        let network = MockNetworkClient()
        given(network).request(.any).willReturn(response(body))
        let moment = Date(timeIntervalSince1970: 2_000_000_000)
        let probe = MuseUsageProbe(credentialLoader: loader, networkClient: network,
                                   cacheDirectory: directory.appendingPathComponent("cache"), now: { moment })
        let first = try await probe.probe()
        let again = try await probe.probe()
        #expect(first.capturedAt == again.capturedAt)
        verify(network).request(.matching { request in
            request.url?.absoluteString == "https://api.meta.ai/muse-code/key" &&
            request.httpMethod == "POST" && request.value(forHTTPHeaderField: "x-api-version") == "1.0.0" &&
            request.httpBody == Data(#"{"onboard":false}"#.utf8)
        }).called(1)
        let files = try FileManager.default.contentsOfDirectory(at: directory.appendingPathComponent("cache"), includingPropertiesForKeys: nil)
        for file in files where file.pathExtension != "lock" {
            let text = try String(contentsOf: file, encoding: .utf8)
            #expect(!text.contains("returned-secret"))
            #expect(!text.contains("private@example"))
            #expect(!text.contains("dca:test-oauth"))
            #expect(!text.contains("standing-inference-key"))
        }
    }

    @Test func failedRequestsAlsoConsumeThePersistedCooldown() async throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let loader = try authFile(directory)
        let network = MockNetworkClient()
        given(network).request(.any).willReturn(response("private response body", status: 500))
        let probe = MuseUsageProbe(credentialLoader: loader, networkClient: network,
                                   cacheDirectory: directory.appendingPathComponent("cache"), now: { Date(timeIntervalSince1970: 2_000_000_000) })
        await #expect(throws: ProbeError.self) { try await probe.probe() }
        await #expect(throws: ProbeError.self) { try await probe.probe() }
        verify(network).request(.any).called(1)
    }

    @Test func transportFailurePersistsStaleObservationAcrossFreshProbeInstances() async throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let loader = try authFile(directory)
        let cache = directory.appendingPathComponent("cache")
        let captured = Date(timeIntervalSince1970: 2_000_000_000)
        let initialNetwork = MockNetworkClient()
        given(initialNetwork).request(.any).willReturn(response(body))
        let initial = try await MuseUsageProbe(credentialLoader: loader, networkClient: initialNetwork, cacheDirectory: cache, now: { captured }).probe()

        let failingNetwork = MockNetworkClient()
        given(failingNetwork).request(.any).willThrow(URLError(.notConnectedToInternet))
        let failedProbe = MuseUsageProbe(credentialLoader: loader, networkClient: failingNetwork, cacheDirectory: cache,
                                         now: { captured.addingTimeInterval(350) })
        await #expect(throws: ProbeError.self) { try await failedProbe.probe() }

        let cooldownProbe = MuseUsageProbe(credentialLoader: loader, cacheDirectory: cache, now: { captured.addingTimeInterval(400) })
        do {
            _ = try await cooldownProbe.probe()
            Issue.record("Expected persisted failure state during cooldown")
        } catch let ProbeError.staleSnapshot(stale, _) {
            #expect(stale.capturedAt == initial.capturedAt)
            #expect(stale.quotas.map(\.quotaType) == [.weekly])
        }
    }

    @Test func httpFailureDoesNotReturnOldSnapshotAsSuccessWithinCooldown() async throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let loader = try authFile(directory)
        let cache = directory.appendingPathComponent("cache")
        let current = Date(timeIntervalSince1970: 2_000_000_000)
        let successful = MockNetworkClient()
        given(successful).request(.any).willReturn(response(body))
        _ = try await MuseUsageProbe(credentialLoader: loader, networkClient: successful, cacheDirectory: cache, now: { current }).probe()

        let failed = MockNetworkClient()
        given(failed).request(.any).willReturn(response("not persisted", status: 503))
        let probe = MuseUsageProbe(credentialLoader: loader, networkClient: failed, cacheDirectory: cache, now: { current.addingTimeInterval(301) })
        await #expect(throws: ProbeError.self) { try await probe.probe() }
        let cooldownProbe = MuseUsageProbe(credentialLoader: loader, cacheDirectory: cache, now: { current.addingTimeInterval(302) })
        do {
            _ = try await cooldownProbe.probe()
            Issue.record("Expected stale snapshot state")
        } catch let ProbeError.staleSnapshot(stale, _) {
            #expect(stale.capturedAt == current)
        }
    }

    @Test func concurrentCooldownProbesShareOneRequestAndFailureState() async throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let loader = try authFile(directory)
        let network = MockNetworkClient()
        given(network).request(.any).willThrow(URLError(.timedOut))
        let probe = MuseUsageProbe(credentialLoader: loader, networkClient: network,
                                   cacheDirectory: directory.appendingPathComponent("cache"), now: { Date(timeIntervalSince1970: 2_000_000_000) })
        async let first: Void = expectFailure(probe)
        async let second: Void = expectFailure(probe)
        _ = await (first, second)
        verify(network).request(.any).called(1)
    }

    private func expectFailure(_ probe: MuseUsageProbe) async {
        do { _ = try await probe.probe(); Issue.record("Expected request failure") } catch { }
    }

    @MainActor @Test func expiredPersistedQuotaClearsProviderAndFeedAfterRefreshFailure() async throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let loader = try authFile(directory)
        let cache = directory.appendingPathComponent("cache")
        let captured = Date(timeIntervalSince1970: 2_000_000_000)
        let network = MuseResponseSequence([
            .response(Data(#"{"is_subs_active":true,"subs_usage":{"window":{"used_percent":25,"resets_at":2000000005},"weekly":{"used_percent":40,"resets_at":2000000010}}}"#.utf8), httpResponse("https://api.meta.ai/muse-code/key", statusCode: 200)),
            .failure(URLError(.notConnectedToInternet))
        ])
        let clock = MutableMuseDate(captured)
        let probe = MuseUsageProbe(credentialLoader: loader, networkClient: network, cacheDirectory: cache, now: { clock.value })
        let provider = AIProvider(id: "muse", name: "Muse", probe: probe, settingsRepository: MockRepositoryFactory.makeSettingsRepository())

        let first = try await provider.refresh()
        #expect(first.quotas.count == 2)
        clock.value = captured.addingTimeInterval(301)
        var failureDescription: String?
        do {
            _ = try await provider.refresh()
            Issue.record("Expected expired quota to become unavailable")
        } catch let ProbeError.noCurrentQuota(reason) {
            failureDescription = reason
            #expect(reason.contains("Network error"))
        }

        let expectedError = try #require(failureDescription)
        #expect(provider.snapshot == nil)
        #expect(provider.lastError?.localizedDescription == expectedError)
        let feed = QuotaFeedDTO.make(from: [provider], at: clock.value).providers[0]
        #expect(feed.status == "unknown")
        #expect(feed.quotas.isEmpty)
        #expect(feed.unavailable == expectedError)
    }

    @Test func incompleteLockFileDoesNotBlockProbeAfterOwnerInterruption() async throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let loader = try authFile(directory, token: "dca:interrupted-owner")
        let cache = directory.appendingPathComponent("cache")
        try FileManager.default.createDirectory(at: cache, withIntermediateDirectories: true)
        let fingerprint = SHA256.hash(data: Data("dca:interrupted-owner".utf8)).map { String(format: "%02x", $0) }.joined()
        let orphanedLock = cache.appendingPathComponent("\(fingerprint).lock")
        #expect(FileManager.default.createFile(atPath: orphanedLock.path, contents: Data()))

        let network = MockNetworkClient()
        given(network).request(.any).willReturn(response(body))
        let probe = MuseUsageProbe(credentialLoader: loader, networkClient: network, cacheDirectory: cache,
                                   now: { Date(timeIntervalSince1970: 2_000_000_000) })
        #expect(try await probe.probe().quotas.count == 2)
        #expect(FileManager.default.fileExists(atPath: orphanedLock.path))
    }

    @Test func accountChangeDoesNotReusePreviousAccountCache() async throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let loader = try authFile(directory, token: "dca:account-one")
        let network = MockNetworkClient()
        given(network).request(.any).willReturn(response(body))
        let probe = MuseUsageProbe(credentialLoader: loader, networkClient: network,
                                   cacheDirectory: directory.appendingPathComponent("cache"), now: { Date(timeIntervalSince1970: 2_000_000_000) })
        _ = try await probe.probe()
        let auth = directory.appendingPathComponent("auth.json")
        try JSONSerialization.data(withJSONObject: ["providers": ["meta": ["mechanism": "oauth", "access_token": "dca:account-two", "api_key": "standing"]]]).write(to: auth)
        let changed = MuseUsageProbe(credentialLoader: MuseCredentialLoader(environment: ["MUSE_AUTH_PATH": auth.path]), networkClient: network,
                                     cacheDirectory: directory.appendingPathComponent("cache"), now: { Date(timeIntervalSince1970: 2_000_000_001) })
        await #expect(throws: ProbeError.self) { try await changed.probe() }
    }
}

private final class MutableMuseDate: @unchecked Sendable {
    private let lock = NSLock()
    private var date: Date

    init(_ date: Date) { self.date = date }

    var value: Date {
        get { lock.lock(); defer { lock.unlock() }; return date }
        set { lock.lock(); defer { lock.unlock() }; date = newValue }
    }
}

private final class MuseResponseSequence: NetworkClient, @unchecked Sendable {
    enum Outcome {
        case response(Data, URLResponse)
        case failure(Error)
    }

    private let lock = NSLock()
    private var outcomes: [Outcome]

    init(_ outcomes: [Outcome]) { self.outcomes = outcomes }

    func request(_ request: URLRequest) async throws -> (Data, URLResponse) {
        switch next() {
        case .response(let data, let response): return (data, response)
        case .failure(let error): throw error
        }
    }

    private func next() -> Outcome {
        lock.lock()
        defer { lock.unlock() }
        return outcomes.removeFirst()
    }
}
