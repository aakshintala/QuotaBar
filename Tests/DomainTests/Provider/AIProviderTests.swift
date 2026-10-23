import Testing
import Foundation
import Mockable
@testable import Domain

@Suite("AIProvider Tests")
@MainActor
struct AIProviderTests {

    private func makeSettingsRepository() -> MockProviderSettingsRepository {
        let mock = MockRepositoryFactory.makeSettingsRepository()
        return mock
    }

    @Test
    func `provider is enabled by default`() {
        let settings = makeSettingsRepository()
        let claude = AIProvider(id: "claude", name: "Claude", probe: MockUsageProbe(), settingsRepository: settings)
        #expect(claude.isEnabled == true)
    }

    // MARK: - State

    @Test
    func `provider starts with no snapshot`() {
        let settings = makeSettingsRepository()
        let claude = AIProvider(id: "claude", name: "Claude", probe: MockUsageProbe(), settingsRepository: settings)
        #expect(claude.snapshot == nil)
    }

    @Test
    func `provider starts not syncing`() {
        let settings = makeSettingsRepository()
        let claude = AIProvider(id: "claude", name: "Claude", probe: MockUsageProbe(), settingsRepository: settings)
        #expect(claude.isSyncing == false)
    }

    @Test
    func `provider starts with no error`() {
        let settings = makeSettingsRepository()
        let claude = AIProvider(id: "claude", name: "Claude", probe: MockUsageProbe(), settingsRepository: settings)
        #expect(claude.lastError == nil)
    }

    // MARK: - Delegation

    @Test
    func `provider delegates isAvailable to probe`() async {
        let settings = makeSettingsRepository()
        let mockProbe = MockUsageProbe()
        given(mockProbe).isAvailable().willReturn(true)
        let claude = AIProvider(id: "claude", name: "Claude", probe: mockProbe, settingsRepository: settings)

        let isAvailable = await claude.isAvailable()
        #expect(isAvailable == true)
    }

    @Test
    func `provider delegates refresh to probe`() async throws {
        let settings = makeSettingsRepository()
        let expectedSnapshot = UsageSnapshot(providerId: "claude", quotas: [], capturedAt: Date())
        let mockProbe = MockUsageProbe()
        given(mockProbe).probe().willReturn(expectedSnapshot)
        let claude = AIProvider(id: "claude", name: "Claude", probe: mockProbe, settingsRepository: settings)

        let snapshot = try await claude.refresh()
        #expect(snapshot.quotas.isEmpty)
    }

    // MARK: - Snapshot Storage

    @Test
    func `provider stores snapshot after refresh`() async throws {
        let settings = makeSettingsRepository()
        let expectedSnapshot = UsageSnapshot(
            providerId: "claude",
            quotas: [UsageQuota(percentRemaining: 50, quotaType: .session, providerId: "claude")],
            capturedAt: Date()
        )
        let mockProbe = MockUsageProbe()
        given(mockProbe).probe().willReturn(expectedSnapshot)
        let claude = AIProvider(id: "claude", name: "Claude", probe: mockProbe, settingsRepository: settings)

        #expect(claude.snapshot == nil)
        _ = try await claude.refresh()
        #expect(claude.snapshot != nil)
        #expect(claude.snapshot?.quotas.first?.percentRemaining == 50)
    }

    // MARK: - Error Handling

    @Test
    func `provider stores error on refresh failure`() async {
        let settings = makeSettingsRepository()
        let mockProbe = MockUsageProbe()
        given(mockProbe).probe().willThrow(ProbeError.timeout)
        let claude = AIProvider(id: "claude", name: "Claude", probe: mockProbe, settingsRepository: settings)

        #expect(claude.lastError == nil)
        do {
            _ = try await claude.refresh()
        } catch {
            // Expected
        }
        #expect(claude.lastError != nil)
    }

    @Test
    func `provider keeps stale snapshot and error when refresh fails with a last known observation`() async {
        let settings = makeSettingsRepository()
        let stale = UsageSnapshot(providerId: "muse", quotas: [UsageQuota(percentRemaining: 50, quotaType: .weekly, providerId: "muse")], capturedAt: Date())
        let mockProbe = MockUsageProbe()
        given(mockProbe).probe().willThrow(ProbeError.staleSnapshot(stale, reason: "Muse refresh failed"))
        let muse = AIProvider(id: "muse", name: "Muse", probe: mockProbe, settingsRepository: settings)

        do { _ = try await muse.refresh() } catch { }
        #expect(muse.snapshot == stale)
        #expect(muse.lastError?.localizedDescription == "Muse refresh failed")
    }

    // MARK: - Syncing State

    @Test
    func `provider resets isSyncing after refresh completes`() async throws {
        let settings = makeSettingsRepository()
        let mockProbe = MockUsageProbe()
        given(mockProbe).probe().willReturn(UsageSnapshot(providerId: "claude", quotas: [], capturedAt: Date()))
        let claude = AIProvider(id: "claude", name: "Claude", probe: mockProbe, settingsRepository: settings)

        #expect(claude.isSyncing == false)
        _ = try await claude.refresh()
        #expect(claude.isSyncing == false)
    }
}
