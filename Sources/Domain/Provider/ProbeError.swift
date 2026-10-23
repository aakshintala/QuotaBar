import Foundation

/// Errors that can occur when probing a CLI
public enum ProbeError: Error, Sendable, LocalizedError {
    /// The CLI binary was not found on the system
    case cliNotFound(String)

    /// User needs to log in to the CLI
    case authenticationRequired

    /// The provider's logged-in account changed; the prior account snapshot is unsafe to display.
    case accountChanged

    /// Muse has an OAuth login but has not completed its one-time key issuance in the CLI.
    case standingKeyRequired

    /// Provider credentials exist but cannot be safely read or validated.
    case credentialAccess(String)

    /// A refresh failed, but the last unexpired observation remains available for display.
    case staleSnapshot(UsageSnapshot, reason: String)

    /// The provider has no current quota observation to display.
    case noCurrentQuota(reason: String)

    /// OAuth session has expired - user needs to re-authenticate.
    /// The optional hint provides provider-specific recovery instructions.
    case sessionExpired(hint: String? = nil)

    /// The CLI output could not be parsed
    case parseFailed(String)

    /// The probe timed out waiting for a response
    case timeout

    /// The CLI needs to be updated
    case updateRequired

    /// Command execution failed
    case executionFailed(String)

    /// The provider's API is rate-limiting us (HTTP 429). `retryAt` is the
    /// earliest time we should try again, derived from the `Retry-After`
    /// header when present or a sensible default otherwise.
    case rateLimited(retryAt: Date)

    // MARK: - LocalizedError

    public var errorDescription: String? {
        switch self {
        case .cliNotFound(let binary):
            return "CLI not found: \(binary)"
        case .authenticationRequired:
            return "Authentication required. Please log in."
        case .accountChanged:
            return "Muse account changed; refresh again to load the new account's quota."
        case .standingKeyRequired:
            return "Muse OAuth login is present, but its standing API key is missing. Complete `muse login` in the Muse CLI; QuotaBar will not issue the key itself."
        case .credentialAccess(let reason):
            return reason
        case .staleSnapshot(_, let reason), .noCurrentQuota(let reason):
            return reason
        case .sessionExpired(let hint):
            if let hint {
                return "Session expired. \(hint)"
            }
            return "Session expired. Please log in again."
        case .parseFailed(let reason):
            return "Failed to parse output: \(reason)"
        case .timeout:
            return "Request timed out"
        case .updateRequired:
            return "CLI update required"
        case .executionFailed(let reason):
            return reason
        case .rateLimited(let retryAt):
            // Relative formatting ("in 30 minutes") is unambiguous across
            // midnight rollovers and more glance-able than an absolute clock
            // time. errorDescription is recomputed on each access, so the
            // string updates naturally as the window ticks down.
            let formatter = RelativeDateTimeFormatter()
            formatter.unitsStyle = .full
            let relative = formatter.localizedString(for: retryAt, relativeTo: Date())
            return "Rate limited. Retrying \(relative)."
        }
    }
}

// MARK: - Equatable (hint ignored for sessionExpired)

extension ProbeError: Equatable {
    public static func == (lhs: ProbeError, rhs: ProbeError) -> Bool {
        switch (lhs, rhs) {
        case (.cliNotFound(let a), .cliNotFound(let b)):
            return a == b
        case (.authenticationRequired, .authenticationRequired), (.accountChanged, .accountChanged), (.standingKeyRequired, .standingKeyRequired):
            return true
        case (.credentialAccess(let a), .credentialAccess(let b)):
            return a == b
        case (.staleSnapshot(let a, let reasonA), .staleSnapshot(let b, let reasonB)):
            return a == b && reasonA == reasonB
        case (.noCurrentQuota(let a), .noCurrentQuota(let b)):
            return a == b
        case (.sessionExpired, .sessionExpired):
            return true
        case (.parseFailed(let a), .parseFailed(let b)):
            return a == b
        case (.timeout, .timeout):
            return true
        case (.updateRequired, .updateRequired):
            return true
        case (.executionFailed(let a), .executionFailed(let b)):
            return a == b
        case (.rateLimited(let a), .rateLimited(let b)):
            return a == b
        default:
            return false
        }
    }
}
