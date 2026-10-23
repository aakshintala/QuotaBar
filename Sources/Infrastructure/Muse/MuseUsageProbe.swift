import Foundation
import Darwin
import Domain

/// Reads Muse CLI OAuth and requests subscription quota from Meta's fixed API origin.
public struct MuseUsageProbe: UsageProbe, @unchecked Sendable {
    private let credentialLoader: MuseCredentialLoader
    private let networkClient: any NetworkClient
    private let cacheDirectory: URL
    private let now: @Sendable () -> Date
    private static let endpoint = URL(string: "https://api.meta.ai/muse-code/key")!
    private static let cooldown: TimeInterval = 300
    private static let maximumBodyBytes = 256 * 1024

    public init(
        credentialLoader: MuseCredentialLoader = MuseCredentialLoader(),
        networkClient: any NetworkClient = MuseURLSessionNetworkClient.shared,
        cacheDirectory: URL? = nil,
        now: @escaping @Sendable () -> Date = Date.init
    ) {
        self.credentialLoader = credentialLoader
        self.networkClient = networkClient
        self.cacheDirectory = cacheDirectory ?? FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".quotabar/muse", isDirectory: true)
        self.now = now
    }

    public func isAvailable() async -> Bool {
        // Let probe() capture missing, denied, and malformed credentials on the provider;
        // an unavailable result here would make QuotaMonitor silently skip the error.
        true
    }

    public func probe() async throws -> UsageSnapshot {
        let credentials: MuseCredentialLoader.Credentials
        switch credentialLoader.load() {
        case .unavailable: throw ProbeError.credentialAccess("Muse OAuth credentials are missing or malformed. Verify the Muse CLI login and its auth metadata.")
        case .standingKeyRequired: throw ProbeError.standingKeyRequired
        case .failed(let error): throw error
        case .credentials(let loaded): credentials = loaded
        }
        let fingerprint = credentials.fingerprint
        let stateURL = cacheDirectory.appendingPathComponent("\(fingerprint).json")
        let timestampURL = cacheDirectory.appendingPathComponent("\(fingerprint).request")
        let staleURL = cacheDirectory.appendingPathComponent("\(fingerprint).stale")
        let current = now()
        try FileManager.default.createDirectory(at: cacheDirectory, withIntermediateDirectories: true)

        // Lock across the cooldown check, reservation, and request so parallel refreshes
        // cannot exceed the persisted five-minute bound.
        let lockURL = cacheDirectory.appendingPathComponent("\(fingerprint).lock")
        let lock = try acquireLock(at: lockURL)
        defer { lock.release() }
        let accountURL = cacheDirectory.appendingPathComponent("active-account")
        if let previous = try? String(contentsOf: accountURL, encoding: .utf8), previous != fingerprint {
            try Data(fingerprint.utf8).write(to: accountURL, options: .atomic)
            throw ProbeError.accountChanged
        }
        try Data(fingerprint.utf8).write(to: accountURL, options: .atomic)

        if let stampData = try? Data(contentsOf: timestampURL),
           let stamp = Double(String(decoding: stampData, as: UTF8.self)), current.timeIntervalSince1970 - stamp < Self.cooldown {
            if FileManager.default.fileExists(atPath: staleURL.path) {
                throw staleError(snapshot: loadSnapshot(at: stateURL, providerId: "muse", at: current))
            }
            if let snapshot = loadSnapshot(at: stateURL, providerId: "muse", at: current), snapshot.capturedAt <= current {
                return snapshot
            }
            throw ProbeError.noCurrentQuota(reason: "Muse quota refresh is limited to once every five minutes; no current reading is available.")
        }

        // Reserve and mark stale before the request; interruption also consumes cooldown
        // and cannot make the previous observation look freshly successful.
        try Data(String(current.timeIntervalSince1970).utf8).write(to: timestampURL, options: .atomic)
        try Data("refresh pending".utf8).write(to: staleURL, options: .atomic)
        var request = URLRequest(url: Self.endpoint)
        request.httpMethod = "POST"
        request.setValue("Bearer \(credentials.accessToken)", forHTTPHeaderField: "Authorization")
        request.setValue("1.0.0", forHTTPHeaderField: "x-api-version")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.httpBody = Data(#"{"onboard":false}"#.utf8)
        request.timeoutInterval = 15

        do {
            let (data, response) = try await networkClient.send(request, label: "Muse")
            guard data.count <= Self.maximumBodyBytes else { throw ProbeError.parseFailed("Muse quota response exceeded the size limit") }
            let snapshot = try Self.parseUsageResponse(data, capturedAt: current)
            guard let httpResponse = response as HTTPURLResponse?, httpResponse.statusCode == 200 else {
                throw ProbeError.executionFailed("Muse quota request returned an invalid response")
            }
            if snapshot.quotas.isEmpty,
               let previous = loadSnapshot(at: stateURL, providerId: "muse", at: current), !previous.quotas.isEmpty {
                throw staleError(snapshot: previous)
            }
            try save(snapshot, to: stateURL)
            try? FileManager.default.removeItem(at: staleURL)
            return snapshot
        } catch let error as ProbeError {
            if error == .authenticationRequired {
                throw staleError(snapshot: loadSnapshot(at: stateURL, providerId: "muse", at: current),
                                 reason: "Muse login rejected; sign in again with `muse login`.")
            }
            throw staleError(snapshot: loadSnapshot(at: stateURL, providerId: "muse", at: current),
                             reason: error.localizedDescription)
        } catch {
            throw staleError(snapshot: loadSnapshot(at: stateURL, providerId: "muse", at: current),
                             reason: "Muse quota request failed.")
        }
    }

    static func parseUsageResponse(_ data: Data, capturedAt: Date = Date()) throws -> UsageSnapshot {
        struct Window: Decodable {
            let used_percent: Double?
            let resets_at: Double?
        }
        struct Usage: Decodable {
            let window: Window?
            let weekly: Window?
        }
        struct Response: Decodable {
            let is_subs_active: Bool
            let subs_tier_name: String?
            let subs_usage: Usage?
        }
        guard data.count <= maximumBodyBytes,
              let response = try? JSONDecoder().decode(Response.self, from: data) else {
            throw ProbeError.parseFailed("Muse quota response could not be decoded")
        }
        guard response.is_subs_active else { throw ProbeError.executionFailed("Muse subscription is not active.") }
        var quotas: [UsageQuota] = []
        for (window, type) in [(response.subs_usage?.window, QuotaType.session), (response.subs_usage?.weekly, .weekly)] {
            guard let window, let used = window.used_percent, used.isFinite else { continue }
            let reset = window.resets_at.map { Date(timeIntervalSince1970: $0) }
            // A reset in the past is not a current quota window.
            guard reset.map({ $0 > capturedAt }) ?? true else { continue }
            quotas.append(UsageQuota(
                percentRemaining: 100 - min(100, max(0, used)),
                quotaType: type,
                providerId: "muse",
                resetsAt: reset
            ))
        }
        return UsageSnapshot(
            providerId: "muse",
            quotas: quotas,
            capturedAt: capturedAt,
            accountTier: response.subs_tier_name.map { .custom($0) }
        )
    }

    private func acquireLock(at url: URL) throws -> ProbeFileLock {
        let descriptor = open(url.path, O_CREAT | O_RDWR, S_IRUSR | S_IWUSR)
        guard descriptor >= 0 else {
            throw ProbeError.executionFailed("Muse quota refresh could not open its cache lock.")
        }
        let deadline = Date().addingTimeInterval(10)
        while Date() < deadline {
            if flock(descriptor, LOCK_EX | LOCK_NB) == 0 {
                return ProbeFileLock(descriptor: descriptor)
            }
            guard errno == EWOULDBLOCK || errno == EAGAIN else {
                close(descriptor)
                throw ProbeError.executionFailed("Muse quota refresh could not acquire its cache lock.")
            }
            Thread.sleep(forTimeInterval: 0.05)
        }
        close(descriptor)
        throw ProbeError.executionFailed("Muse quota refresh is already running.")
    }

    private func staleError(snapshot: UsageSnapshot?, reason: String = "Muse has not returned current quota data; showing the last known reading.") -> ProbeError {
        guard let snapshot, !snapshot.quotas.isEmpty else {
            return .noCurrentQuota(reason: reason)
        }
        return .staleSnapshot(snapshot, reason: reason)
    }

    private func loadSnapshot(at url: URL, providerId: String, at now: Date) -> UsageSnapshot? {
        guard let data = try? Data(contentsOf: url),
              let cached = try? JSONDecoder().decode(CachedSnapshot.self, from: data), cached.providerId == providerId
        else { return nil }
        let snapshot = cached.snapshot
        return UsageSnapshot(providerId: snapshot.providerId,
                             quotas: snapshot.quotas.filter { $0.resetsAt.map { $0 > now } ?? true },
                             capturedAt: snapshot.capturedAt, accountTier: snapshot.accountTier)
    }

    private func save(_ snapshot: UsageSnapshot, to url: URL) throws {
        try JSONEncoder().encode(CachedSnapshot(snapshot: snapshot)).write(to: url, options: .atomic)
    }

    private struct CachedSnapshot: Codable {
        let providerId: String
        let capturedAt: Date
        let tier: String?
        let windows: [Window]
        init(snapshot: UsageSnapshot) {
            providerId = snapshot.providerId
            capturedAt = snapshot.capturedAt
            tier = snapshot.accountTier?.badgeText
            windows = snapshot.quotas.compactMap { quota in
                guard let remaining = quota.percentRemaining else { return nil }
                return Window(type: quota.quotaType == .session ? "session" : "weekly", remaining: remaining, resetsAt: quota.resetsAt)
            }
        }
        var snapshot: UsageSnapshot {
            UsageSnapshot(providerId: providerId, quotas: windows.map {
                UsageQuota(percentRemaining: $0.remaining, quotaType: $0.type == "session" ? .session : .weekly, providerId: providerId, resetsAt: $0.resetsAt)
            }, capturedAt: capturedAt, accountTier: tier.map { .custom($0) })
        }
    }
    private struct Window: Codable {
        let type: String
        let remaining: Double
        let resetsAt: Date?
    }
}

private final class ProbeFileLock: @unchecked Sendable {
    private let mutex = NSLock()
    private var descriptor: Int32?

    init(descriptor: Int32) {
        self.descriptor = descriptor
    }

    func release() {
        mutex.lock()
        defer { mutex.unlock() }
        guard let descriptor else { return }
        self.descriptor = nil
        _ = flock(descriptor, LOCK_UN)
        _ = close(descriptor)
    }

    deinit { release() }
}
