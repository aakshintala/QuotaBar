import Foundation
import CryptoKit
import Security
import Domain

protocol MuseKeychainReader: Sendable {
    func read(service: String, account: String, withoutUI: Bool) -> (status: Int32, data: Data?)
}

struct SystemMuseKeychainReader: MuseKeychainReader {
    func read(service: String, account: String, withoutUI: Bool) -> (status: Int32, data: Data?) {
        var query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne
        ]
        if withoutUI {
            query[kSecUseAuthenticationUI as String] = kSecUseAuthenticationUIFail
        }
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        return (status, result as? Data)
    }
}

/// Reads the official Muse CLI's existing OAuth login without modifying it.
public struct MuseCredentialLoader: Sendable {
    struct Credentials: Sendable {
        let accessToken: String
        let fingerprint: String
    }

    private let environment: [String: String]
    private let keychainReader: any MuseKeychainReader

    public init(environment: [String: String] = ProcessInfo.processInfo.environment) {
        self.environment = environment
        self.keychainReader = SystemMuseKeychainReader()
    }

    init(environment: [String: String], keychainReader: any MuseKeychainReader) {
        self.environment = environment
        self.keychainReader = keychainReader
    }

    enum LoadResult: Sendable {
        case unavailable
        case standingKeyRequired
        case failed(ProbeError)
        case credentials(Credentials)
    }

    func load() -> LoadResult {
        guard let path = credentialURL()?.path,
              let data = try? Data(contentsOf: URL(fileURLWithPath: path), options: .mappedIfSafe),
              data.count <= 1_048_576,
              let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let providers = root["providers"] as? [String: Any],
              let meta = providers["meta"] as? [String: Any],
              meta["mechanism"] as? String == "oauth"
        else { return .unavailable }

        let credentialData: [String: Any]
        if meta["storage"] as? String == "keychain" {
            let result = keychainReader.read(service: "ai.meta.dev.credentials", account: "meta", withoutUI: true)
            guard result.status == errSecSuccess else {
                return .failed(keychainError(status: result.status))
            }
            guard let data = result.data else {
                return .failed(.credentialAccess("Muse Keychain lookup returned no credential data. Check QuotaBar's access to the Muse credential item."))
            }
            guard data.count <= 1_048_576,
                  let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
                return .failed(.credentialAccess("Muse Keychain credentials are malformed. Complete or repair the Muse CLI login."))
            }
            credentialData = json
        } else {
            credentialData = meta
        }

        guard let token = credentialData["access_token"] as? String,
              token.hasPrefix("dca:"), token.count > 4 else {
            return .failed(.credentialAccess("Muse OAuth access token is missing or malformed. Complete the Muse CLI login."))
        }
        guard let key = credentialData["api_key"] as? String,
              !key.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return .standingKeyRequired
        }
        let digest = SHA256.hash(data: Data(token.utf8))
        return .credentials(Credentials(accessToken: token, fingerprint: digest.map { String(format: "%02x", $0) }.joined()))
    }

    private func keychainError(status: Int32) -> ProbeError {
        switch status {
        case errSecInteractionNotAllowed, errSecAuthFailed:
            return .credentialAccess("Muse credentials are in Keychain, but QuotaBar needs Keychain access. Allow QuotaBar to read the Muse credential item in Keychain Access, then refresh. QuotaBar will not prompt for access.")
        case errSecItemNotFound:
            return .credentialAccess("Muse's Keychain credential item is missing. Complete Muse login in the CLI and verify its Keychain entry.")
        default:
            return .credentialAccess("Muse credentials could not be read from Keychain (status \(status)). Check Keychain availability and QuotaBar's access to the Muse credential item.")
        }
    }

    private func credentialURL() -> URL? {
        if let override = environment["MUSE_AUTH_PATH"], !override.isEmpty {
            return URL(fileURLWithPath: override)
        }
        let home = FileManager.default.homeDirectoryForCurrentUser
        if let xdg = environment["XDG_CONFIG_HOME"], xdg.hasPrefix("/") {
            return URL(fileURLWithPath: xdg).appendingPathComponent("muse/auth.json")
        }
        return home.appendingPathComponent(".config/muse/auth.json")
    }
}
