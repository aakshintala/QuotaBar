import Foundation
import Mockable
import Testing
@testable import Infrastructure

@Suite("quotad service registration")
@MainActor
struct QuotadServiceTests {
    private let home = URL(filePath: "/Users/test")
    private let uid: UInt32 = 501

    private var plistURL: URL {
        home.appendingPathComponent("Library/LaunchAgents/com.aakshintala.quotad.plist")
    }

    private func makeService(
        status: QuotadAgentStatus = .notRegistered
    ) -> (QuotadService, MockQuotadAgentGateway, MockQuotadFileStore, MockQuotadProcessRunner) {
        let agent = MockQuotadAgentGateway()
        let files = MockQuotadFileStore()
        let process = MockQuotadProcessRunner()
        given(agent).status().willReturn(status)
        given(agent).openLoginItemsSettings().willReturn()
        given(files).installBinary().willReturn()
        given(files).selfInstalledPlistURL().willReturn(plistURL)
        given(files).writeSelfInstalledPlist(.any).willReturn()
        let service = QuotadService(
            agent: agent, files: files, process: process,
            homeDirectoryURL: home, uid: uid
        )
        return (service, agent, files, process)
    }

    // MARK: - Status → action

    @Test
    func `enabled agent needs nothing`() async {
        let (service, agent, _, process) = makeService(status: .enabled)
        given(process).run(executablePath: .any, arguments: .any).willReturn()

        await service.ensureRegistered()

        #expect(service.state == .runningViaLaunchd)
        verify(agent).register().called(0)
        verify(process).run(executablePath: .any, arguments: .any).called(0)
    }

    @Test
    func `agent requiring approval surfaces needsApproval without registering`() async {
        let (service, agent, _, _) = makeService(status: .requiresApproval)

        await service.ensureRegistered()

        #expect(service.state == .needsApproval)
        verify(agent).register().called(0)
    }

    @Test
    func `unregistered agent is registered`() async {
        let (service, agent, files, _) = makeService(status: .notRegistered)
        given(agent).register().willReturn()
        given(files).selfInstalledPlistExists().willReturn(false)

        await service.ensureRegistered()

        #expect(service.state == .runningViaLaunchd)
        verify(agent).register().called(1)
        verify(files).selfInstalledPlistExists().called(1)
    }

    @Test
    func `missing agent is registered`() async {
        let (service, agent, files, _) = makeService(status: .notFound)
        given(agent).register().willReturn()
        given(files).selfInstalledPlistExists().willReturn(false)

        await service.ensureRegistered()

        #expect(service.state == .runningViaLaunchd)
        verify(agent).register().called(1)
    }

    @Test
    func `registration denied by the user surfaces needsApproval`() async {
        let (service, agent, _, process) = makeService()
        given(agent).register().willThrow(QuotadAgentError.requiresApproval)
        given(process).run(executablePath: .any, arguments: .any).willReturn()

        await service.ensureRegistered()

        #expect(service.state == .needsApproval)
        verify(process).run(executablePath: .any, arguments: .any).called(0)
    }

    // MARK: - Self-install fallback

    @Test
    func `other registration errors fall back to the self-install`() async {
        let (service, agent, files, process) = makeService()
        given(agent).register().willThrow(NSError(domain: "test", code: 1))
        given(files).binaryNeedsInstall().willReturn(true)
        given(process).run(executablePath: .any, arguments: .any).willReturn()

        await service.ensureRegistered()

        #expect(service.state == .selfInstalled)
        verify(files).installBinary().called(1)
        verify(files).writeSelfInstalledPlist(.any).called(1)
        verify(process).run(
            executablePath: .matching { $0 == "/bin/launchctl" },
            arguments: .matching { $0 == ["bootout", "gui/501/com.aakshintala.quotad"] }
        ).called(1)
        verify(process).run(
            executablePath: .matching { $0 == "/bin/launchctl" },
            arguments: .matching { $0 == ["bootstrap", "gui/501", plistURL.path] }
        ).called(1)
    }

    @Test
    func `fallback skips the binary copy when it is unchanged`() async {
        let (service, agent, files, process) = makeService()
        given(agent).register().willThrow(NSError(domain: "test", code: 1))
        given(files).binaryNeedsInstall().willReturn(false)
        given(process).run(executablePath: .any, arguments: .any).willReturn()

        await service.ensureRegistered()

        #expect(service.state == .selfInstalled)
        verify(files).installBinary().called(0)
        verify(files).writeSelfInstalledPlist(.any).called(1)
        verify(process).run(executablePath: .any, arguments: .any).called(2)
    }

    @Test
    func `fallback ignores bootout failure but reports bootstrap failure`() async {
        let (ignoring, ignoringAgent, ignoringFiles, process) = makeService()
        given(ignoringAgent).register().willThrow(NSError(domain: "test", code: 1))
        given(ignoringFiles).binaryNeedsInstall().willReturn(false)
        given(process).run(
            executablePath: .any, arguments: .matching { $0.first == "bootout" }
        ).willThrow(NSError(domain: "test", code: 1))
        given(process).run(
            executablePath: .any, arguments: .matching { $0.first == "bootstrap" }
        ).willReturn()

        await ignoring.ensureRegistered()

        // bootout failed (ignored), bootstrap succeeded.
        #expect(ignoring.state == .selfInstalled)

        let (broken, brokenAgent, brokenFiles, brokenProcess) = makeService()
        given(brokenAgent).register().willThrow(NSError(domain: "test", code: 1))
        given(brokenFiles).binaryNeedsInstall().willReturn(false)
        given(brokenProcess).run(
            executablePath: .any, arguments: .matching { $0.first == "bootstrap" }
        ).willThrow(NSError(domain: "test", code: 2))
        given(brokenProcess).run(
            executablePath: .any, arguments: .matching { $0.first == "bootout" }
        ).willReturn()

        await broken.ensureRegistered()

        if case .failed = broken.state {} else {
            Issue.record("expected failed state, got \(broken.state)")
        }
    }

    // MARK: - Self-installed plist

    @Test
    func `self-installed plist uses absolute paths and the daemon keys`() throws {
        let data = QuotadService.selfInstalledPlistData(homeDirectory: home)
        let plist = try PropertyListSerialization.propertyList(
            from: data, format: nil
        ) as? [String: Any]
        let decoded = try #require(plist)

        #expect(decoded["Label"] as? String == "com.aakshintala.quotad")
        #expect(decoded["ProgramArguments"] as? [String] == [
            "/Users/test/.local/bin/quotad",
            "-log",
            "/Users/test/Library/Logs/QuotaBar/quotad.log",
        ])
        #expect(decoded["RunAtLoad"] as? Bool == true)
        #expect((decoded["KeepAlive"] as? [String: Bool]) == ["SuccessfulExit": false])
        #expect(decoded["ThrottleInterval"] as? Int == 10)
        #expect(decoded["AssociatedBundleIdentifiers"] as? [String]
            == ["com.aakshintala.subscriptionusagebar"])
    }

    @Test
    func `fallback writes the daemon plist content`() async {
        let (service, agent, files, process) = makeService()
        given(agent).register().willThrow(NSError(domain: "test", code: 1))
        given(files).binaryNeedsInstall().willReturn(false)
        given(process).run(executablePath: .any, arguments: .any).willReturn()

        await service.ensureRegistered()

        let expected = QuotadService.selfInstalledPlistData(homeDirectory: home)
        verify(files).writeSelfInstalledPlist(.matching { $0 == expected }).called(1)
    }

    // MARK: - Cleanup after a later successful registration

    @Test
    func `successful registration removes a leftover self-install`() async {
        let (service, agent, files, process) = makeService(status: .notRegistered)
        given(agent).register().willReturn()
        given(files).selfInstalledPlistExists().willReturn(true)
        given(files).removeSelfInstalledPlist().willReturn()
        given(process).run(executablePath: .any, arguments: .any).willReturn()

        await service.ensureRegistered()

        #expect(service.state == .runningViaLaunchd)
        verify(process).run(
            executablePath: .matching { $0 == "/bin/launchctl" },
            arguments: .matching { $0 == ["bootout", "gui/501/com.aakshintala.quotad"] }
        ).called(1)
        verify(files).removeSelfInstalledPlist().called(1)
    }

    @Test
    func `failed cleanup of the self-install surfaces failed`() async {
        let (service, agent, files, process) = makeService(status: .notRegistered)
        given(agent).register().willReturn()
        given(files).selfInstalledPlistExists().willReturn(true)
        given(files).removeSelfInstalledPlist().willThrow(NSError(domain: "test", code: 3))
        given(process).run(executablePath: .any, arguments: .any).willReturn()

        await service.ensureRegistered()

        if case .failed = service.state {} else {
            Issue.record("expected failed state, got \(service.state)")
        }
    }

    // MARK: - Login Items

    @Test
    func `approval banner button opens Login Items`() async {
        let (service, agent, _, _) = makeService(status: .requiresApproval)

        await service.openLoginItemsSettings()

        verify(agent).openLoginItemsSettings().called(1)
    }
}
