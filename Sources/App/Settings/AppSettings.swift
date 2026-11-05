import Foundation
import Domain
import Infrastructure

/// Observable settings manager for QuotaBar preferences.
/// Thin `@Observable` wrapper around `JSONSettingsRepository` for SwiftUI reactivity.
/// All persistence is delegated to the repository (`~/.quotabar/settings.json`).
@MainActor
@Observable
public final class AppSettings {
    public static let shared = AppSettings()

    /// The underlying repository (internal - views access settings through AppSettings properties/methods)
    private let repository: JSONSettingsRepository

    // MARK: - Theme Settings

    /// The current theme mode (light, dark)
    public var themeMode: String {
        didSet {
            repository.update { $0.app.themeMode = themeMode }
        }
    }

    // MARK: - Background Sync Settings

    /// Whether background sync is enabled (default: false)
    public var backgroundSyncEnabled: Bool {
        didSet {
            repository.update { $0.app.backgroundSyncEnabled = backgroundSyncEnabled }
        }
    }

    /// Background sync interval in seconds (default: 600)
    public var backgroundSyncInterval: TimeInterval {
        didSet {
            repository.update { $0.app.backgroundSyncInterval = backgroundSyncInterval }
        }
    }

    /// Whether quota-threshold notifications are enabled (default: true)
    public var quotaAlertsEnabled: Bool {
        didSet {
            repository.update { $0.app.quotaAlertsEnabled = quotaAlertsEnabled }
        }
    }

    /// Port of quotad's localhost quota feed (default: 8787). Read-only here:
    /// quotad owns `feed.enabled` in the same settings file, so QuotaBar never
    /// writes the feed section.
    public var feedPort: Int {
        didSet {
            repository.update { $0.feed.port = feedPort }
        }
    }

    /// The background-refresh cadence (Off / 1 / 5 / 15 min) as a single
    /// picker-friendly value. Computed over the legacy `backgroundSyncEnabled`
    /// + `backgroundSyncInterval` pair so `settings.json` stays backward
    /// compatible — "Off" maps to `backgroundSyncEnabled == false`, the others
    /// to enabled + 60/300/600/900s. Setting it persists both underlying keys.
    public var refreshInterval: RefreshInterval {
        get {
            RefreshInterval.migrating(
                enabled: backgroundSyncEnabled,
                storedSeconds: backgroundSyncInterval
            )
        }
        set {
            // Set the interval before flipping enabled so anything observing the
            // change sees the final cadence in a single pass.
            if let seconds = newValue.seconds {
                backgroundSyncInterval = TimeInterval(seconds)
            }
            backgroundSyncEnabled = newValue.isEnabled
        }
    }

    // MARK: - Initialization

    private init(repository: JSONSettingsRepository = .shared) {
        self.repository = repository

        let stored = repository.settings
        self.themeMode = stored.app.themeMode
        self.backgroundSyncEnabled = stored.app.backgroundSyncEnabled
        self.backgroundSyncInterval = stored.app.backgroundSyncInterval
        self.quotaAlertsEnabled = stored.app.quotaAlertsEnabled
        self.feedPort = stored.feed.port
    }

    // MARK: - Provider Settings Access

    /// Access provider-specific settings for reading/writing in Settings UI.
    /// These are non-observable (loaded into @State) - only app-level settings are @Observable.
    public var provider: ProviderSettingsRepository { repository }
}
