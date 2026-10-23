import Foundation

/// Muse's token-bearing request must not follow redirects to another origin.
public final class MuseURLSessionNetworkClient: NetworkClient, @unchecked Sendable {
    public static let shared = MuseURLSessionNetworkClient()

    private static let maximumBytes = 256 * 1024
    private let delegate: RedirectDelegate
    private let session: URLSession

    public convenience init() {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 15
        self.init(configuration: configuration)
    }

    init(configuration: URLSessionConfiguration) {
        let delegate = RedirectDelegate()
        self.delegate = delegate
        self.session = URLSession(configuration: configuration, delegate: delegate, delegateQueue: nil)
    }

    public func request(_ request: URLRequest) async throws -> (Data, URLResponse) {
        let (data, response) = try await session.data(for: request)
        guard data.count <= Self.maximumBytes else { throw URLError(.dataLengthExceedsMaximum) }
        return (data, response)
    }

    private final class RedirectDelegate: NSObject, URLSessionTaskDelegate, URLSessionDataDelegate, @unchecked Sendable {
        private let lock = NSLock()
        private var receivedBytes: [Int: Int] = [:]
        private let maximumBytes = MuseURLSessionNetworkClient.maximumBytes

        func urlSession(_ session: URLSession, task: URLSessionTask,
                        willPerformHTTPRedirection response: HTTPURLResponse,
                        newRequest request: URLRequest,
                        completionHandler: @escaping (URLRequest?) -> Void) {
            completionHandler(nil)
        }

        func urlSession(_ session: URLSession, dataTask: URLSessionDataTask,
                        didReceive response: URLResponse,
                        completionHandler: @escaping (URLSession.ResponseDisposition) -> Void) {
            completionHandler(response.expectedContentLength > maximumBytes ? .cancel : .allow)
        }

        func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
            lock.lock()
            let total = receivedBytes[dataTask.taskIdentifier, default: 0] + data.count
            receivedBytes[dataTask.taskIdentifier] = total
            lock.unlock()
            if total > maximumBytes { dataTask.cancel() }
        }

        func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
            lock.lock()
            receivedBytes.removeValue(forKey: task.taskIdentifier)
            lock.unlock()
        }
    }
}
