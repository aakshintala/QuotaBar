import Foundation
import Testing
@testable import Infrastructure
@testable import Domain

@Suite struct MuseCredentialLoaderTests {
    @Test func keychainMetadataReadsExactOAuthItemWithoutUIAndKeepsStandingKey() throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let auth = directory.appendingPathComponent("auth.json")
        try JSONSerialization.data(withJSONObject: ["providers": ["meta": [
            "storage": "keychain", "mechanism": "oauth", "obtained_via": "device_code"
        ]]]).write(to: auth)
        let reader = FakeMuseKeychainReader(data: Data(#"{"access_token":"dca:oauth","refresh_token":"refresh","api_key":"standing-key"}"#.utf8))
        let loader = MuseCredentialLoader(environment: ["MUSE_AUTH_PATH": auth.path], keychainReader: reader)

        guard case .credentials(let credentials) = loader.load() else { Issue.record("Expected keychain OAuth credentials"); return }
        #expect(credentials.accessToken == "dca:oauth")
        #expect(credentials.fingerprint.isEmpty == false)
        #expect(reader.service == "ai.meta.dev.credentials")
        #expect(reader.account == "meta")
        #expect(reader.withoutUI == true)
    }

    @Test func keychainOAuthWithoutStandingKeyIsRejected() throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let auth = directory.appendingPathComponent("auth.json")
        try JSONSerialization.data(withJSONObject: ["providers": ["meta": ["storage": "keychain", "mechanism": "oauth"]]]).write(to: auth)
        let reader = FakeMuseKeychainReader(data: Data(#"{"access_token":"dca:oauth"}"#.utf8))
        let loader = MuseCredentialLoader(environment: ["MUSE_AUTH_PATH": auth.path], keychainReader: reader)
        guard case .standingKeyRequired = loader.load() else { Issue.record("Standing API key must remain required"); return }
    }

    @Test(arguments: [
        Int32(-25308), // errSecInteractionNotAllowed
        Int32(-25293), // errSecAuthFailed
    ]) func deniedOrInteractionRequiredIsActionable(status: Int32) throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let auth = directory.appendingPathComponent("auth.json")
        try JSONSerialization.data(withJSONObject: ["providers": ["meta": ["storage": "keychain", "mechanism": "oauth"]]]).write(to: auth)
        let loader = MuseCredentialLoader(environment: ["MUSE_AUTH_PATH": auth.path], keychainReader: FakeMuseKeychainReader(status: status))
        guard case .failed(let error) = loader.load() else { Issue.record("Expected a visible Keychain error"); return }
        #expect(error.localizedDescription.lowercased().contains("keychain"))
        #expect(error.localizedDescription.lowercased().contains("access"))
    }

    @Test @MainActor func monitorSurfacesKeychainFailureInsteadOfSkippingMuse() async throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let auth = directory.appendingPathComponent("auth.json")
        try JSONSerialization.data(withJSONObject: ["providers": ["meta": ["storage": "keychain", "mechanism": "oauth"]]]).write(to: auth)
        let loader = MuseCredentialLoader(environment: ["MUSE_AUTH_PATH": auth.path], keychainReader: FakeMuseKeychainReader(status: -25308))
        let probe = MuseUsageProbe(credentialLoader: loader)
        let provider = AIProvider(id: "muse", name: "Muse", probe: probe, settingsRepository: MockRepositoryFactory.makeSettingsRepository())
        let monitor = QuotaMonitor(providers: [provider], clock: ImmediateMuseClock())

        #expect(await probe.isAvailable())
        await monitor.refresh()
        #expect(provider.snapshot == nil)
        #expect(provider.lastError?.localizedDescription.contains("needs Keychain access") == true)
    }

    @Test func malformedKeychainDataIsActionableAndFileStorageRemainsSupported() throws {
        let directory = try makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let auth = directory.appendingPathComponent("auth.json")
        try JSONSerialization.data(withJSONObject: ["providers": ["meta": ["storage": "keychain", "mechanism": "oauth"]]]).write(to: auth)
        let invalid = MuseCredentialLoader(environment: ["MUSE_AUTH_PATH": auth.path], keychainReader: FakeMuseKeychainReader(data: Data("{}".utf8)))
        guard case .failed = invalid.load() else { Issue.record("Malformed Keychain data must be visible"); return }

        try JSONSerialization.data(withJSONObject: ["providers": ["meta": [
            "storage": "file", "mechanism": "oauth", "access_token": "dca:file", "api_key": "standing"
        ]]]).write(to: auth)
        let file = MuseCredentialLoader(environment: ["MUSE_AUTH_PATH": auth.path], keychainReader: FakeMuseKeychainReader(status: -25300))
        guard case .credentials(let credentials) = file.load() else { Issue.record("Expected existing file-token support"); return }
        #expect(credentials.accessToken == "dca:file")
    }
}

private struct ImmediateMuseClock: Clock {
    func sleep(for duration: Duration) async throws {}
    func sleep(nanoseconds: UInt64) async throws {}
}

private final class FakeMuseKeychainReader: MuseKeychainReader, @unchecked Sendable {
    let data: Data?
    let status: Int32
    private(set) var service: String?
    private(set) var account: String?
    private(set) var withoutUI: Bool?

    init(data: Data) { self.data = data; self.status = 0 }
    init(status: Int32) { self.data = nil; self.status = status }

    func read(service: String, account: String, withoutUI: Bool) -> (status: Int32, data: Data?) {
        self.service = service
        self.account = account
        self.withoutUI = withoutUI
        return (status, data)
    }
}
