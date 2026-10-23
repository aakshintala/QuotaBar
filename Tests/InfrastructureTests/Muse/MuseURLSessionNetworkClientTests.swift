import Foundation
import Testing
@testable import Infrastructure

@Suite struct MuseURLSessionNetworkClientTests {
    @Test func refusesRedirectsAndCancelsOversizedResponses() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 2
        configuration.protocolClasses = [MuseTestURLProtocol.self]
        let client = MuseURLSessionNetworkClient(configuration: configuration)

        do {
            _ = try await client.request(URLRequest(url: URL(string: "https://muse.test/redirect")!))
            Issue.record("Expected redirect rejection")
        } catch { }
        #expect(MuseTestURLProtocol.redirectTargetWasRequested == false)

        do {
            _ = try await client.request(URLRequest(url: URL(string: "https://muse.test/large")!))
            Issue.record("Expected oversized response rejection")
        } catch { }
    }
}

private final class MuseTestURLProtocol: URLProtocol, @unchecked Sendable {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var didRequestRedirectTarget = false

    static var redirectTargetWasRequested: Bool {
        lock.lock()
        defer { lock.unlock() }
        return didRequestRedirectTarget
    }

    override class func canInit(with request: URLRequest) -> Bool {
        request.url?.host == "muse.test" || request.url?.host == "redirect-target.test"
    }

    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        guard let url = request.url else { return }
        if url.host == "redirect-target.test" {
            Self.lock.lock()
            Self.didRequestRedirectTarget = true
            Self.lock.unlock()
        }
        if url.path == "/redirect" {
            let response = HTTPURLResponse(url: url, statusCode: 302, httpVersion: "HTTP/1.1", headerFields: ["Location": "https://redirect-target.test/secret"])!
            client?.urlProtocol(self, wasRedirectedTo: URLRequest(url: URL(string: "https://redirect-target.test/secret")!), redirectResponse: response)
            return
        }
        let body = Data(repeating: 65, count: 300 * 1024)
        let response = HTTPURLResponse(url: url, statusCode: 200, httpVersion: "HTTP/1.1", headerFields: ["Content-Length": String(body.count)])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: body)
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() { }
}
