import Foundation
import Domain

/// HTTP client for quotad's local feed (`GET http://127.0.0.1:<port>/quotas`).
/// The port comes from `feed.port` in `~/.quotabar/settings.json` (default 8787).
/// One request serves every provider of a refresh: concurrent callers share the
/// in-flight request, and a response younger than `reuseWindow` is reused.
public actor FeedClient {
    /// A response younger than this is reused instead of re-fetching.
    public static let reuseWindow: TimeInterval = 5

    /// How long a feed request may take before it fails.
    public static let requestTimeout: TimeInterval = 25

    public static let defaultPort = 8787

    public static let shared = FeedClient()

    private let settings: JSONSettingsRepository
    private let networkClient: any NetworkClient

    private var cached: (at: Date, feed: QuotaFeedDTO)?
    private var inFlight: Task<QuotaFeedDTO, Error>?

    public init(
        settings: JSONSettingsRepository = .shared,
        networkClient: any NetworkClient = URLSession.shared
    ) {
        self.settings = settings
        self.networkClient = networkClient
    }

    public func fetch() async throws -> QuotaFeedDTO {
        if let cached, Date().timeIntervalSince(cached.at) < Self.reuseWindow {
            return cached.feed
        }
        if let inFlight {
            return try await inFlight.value
        }
        let task = Task { try await download() }
        inFlight = task
        defer { inFlight = nil }
        let feed = try await task.value
        cached = (Date(), feed)
        return feed
    }

    private func download() async throws -> QuotaFeedDTO {
        let port = settings.settings.feed.port
        guard let url = URL(string: "http://127.0.0.1:\(port)/quotas") else {
            throw ProbeError.executionFailed("Invalid quotad feed port: \(port)")
        }
        var request = URLRequest(url: url, timeoutInterval: Self.requestTimeout)
        request.httpMethod = "GET"

        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await networkClient.request(request)
        } catch {
            AppLog.probes.error("quotad feed: daemon unreachable on 127.0.0.1:\(port)")
            throw ProbeError.executionFailed("quotad is not running on 127.0.0.1:\(port)")
        }

        guard let http = response as? HTTPURLResponse, http.statusCode == 200 else {
            let status = (response as? HTTPURLResponse)?.statusCode ?? -1
            AppLog.probes.error("quotad feed: HTTP \(status)")
            throw ProbeError.executionFailed("quotad feed returned HTTP \(status)")
        }

        do {
            return try feedDecoder().decode(QuotaFeedDTO.self, from: data)
        } catch {
            AppLog.probes.error("quotad feed: decode failed: \(error.localizedDescription)")
            throw ProbeError.parseFailed("quotad feed: \(error.localizedDescription)")
        }
    }
}

/// JSONDecoder for the feed: quotad emits RFC3339 without fractional seconds,
/// but fractional timestamps are tolerated too.
func feedDecoder() -> JSONDecoder {
    let decoder = JSONDecoder()
    decoder.dateDecodingStrategy = .custom { decoder in
        let container = try decoder.singleValueContainer()
        let string = try container.decode(String.self)
        if let date = parseISO8601(string) { return date }
        throw DecodingError.dataCorruptedError(
            in: container, debugDescription: "Invalid date: \(string)")
    }
    return decoder
}
