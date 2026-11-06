import SwiftUI
import Domain
import Infrastructure
import MenuBarExtraAccess

@main
struct QuotaBarApp: App {
    /// The main domain service - monitors all AI providers
    /// This is the single source of truth for providers and their state
    @State private var monitor: QuotaMonitor

    /// Static menu-bar icon + sleep-safe background-refresh lifecycle,
    /// driven imperatively outside SwiftUI (issue #192).
    private let statusItemDriver: StatusBarIconDriver

    /// Binding required by `.menuBarExtraAccess`; also enables programmatic
    /// dropdown control if ever needed.
    @State private var isMenuPresented = false

    /// Alerts users when quota status degrades
    private let quotaAlerter = NotificationAlerter {
        JSONSettingsRepository.shared.settings.app.quotaAlertsEnabled
    }

    /// Registers the bundled quotad LaunchAgent at launch (or self-installs
    /// the fallback); owns the Login Items approval state the UI banners read.
    private let quotadService: QuotadService

    init() {
        let version = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "1.0.0"
        let build = Bundle.main.infoDictionary?["CFBundleVersion"] as? String ?? "1"
        AppLog.ui.info("QuotaBar v\(version) (\(build)) initializing...")

        // ~/.quotabar/settings.json, read once into memory.
        let settingsRepository = JSONSettingsRepository.shared

        // One AIProvider per (id, name, probe). The id is the settings key and feed id.
        // Snapshots come from quotad's local feed; one shared client serves all
        // five providers of a refresh with a single HTTP request.
        let feedClient = FeedClient(settings: settingsRepository)
        func provider(_ id: String, _ name: String) -> AIProvider {
            AIProvider(
                id: id, name: name,
                probe: FeedUsageProbe(id: id, client: feedClient),
                settingsRepository: settingsRepository
            )
        }
        let providers = [
            provider("claude", "Claude"),
            provider("codex", "Codex"),
            provider("cursor", "Cursor"),
            provider("opencode-go", "OpenCode Go"),
            provider("muse", "Muse"),
        ]
        settingsRepository.keepProviders(providers.map(\.id))
        AppLog.providers.info("Created \(providers.count) providers")

        // Initialize the domain service with quota alerter
        let monitor = QuotaMonitor(
            providers: providers,
            alerter: quotaAlerter
        )
        self.monitor = monitor
        AppLog.monitor.info("QuotaMonitor initialized")

        statusItemDriver = StatusBarIconDriver(
            monitor: monitor,
            settings: AppSettings.shared
        )
        statusItemDriver.startMonitoringLifecycle()

        // macOS keeps the bundled quotad running via SMAppService; when the
        // agent cannot be registered the service self-installs the fallback.
        let quotadService = QuotadService()
        self.quotadService = quotadService
        Task { @MainActor in await quotadService.ensureRegistered() }

        // Note: Notification permission is requested in onAppear, not here
        // Menu bar apps need the run loop to be active before requesting permissions

        AppLog.ui.info("QuotaBar initialization complete")
    }

    /// App settings for theme
    @State private var settings = AppSettings.shared

    var body: some Scene {
        MenuBarExtra {
            Group {
                PopoverView(
                    monitor: monitor,
                    quotaAlerter: quotaAlerter,
                    quotadService: quotadService
                )
                    .appThemeProvider(themeModeId: settings.themeMode)
            }
            // Opening/closing the dropdown flips `isMenuPresented`, which makes
            // SwiftUI re-evaluate the scene and wipe the AppKit-drawn button
            // image. The dropdown's lifecycle maps 1:1 to those flips, so
            // re-assert the menu-bar pixels on both edges.
            .onAppear { statusItemDriver.reassertPresentation() }
            .onDisappear { statusItemDriver.reassertPresentation() }
        } label: {
            // Deliberately static: the menu-bar icon is drawn by
            // StatusBarIconDriver into the status item's button image,
            // because this SwiftUI label hosting can permanently stop
            // re-evaluating after system sleep (issue #192). The placeholder
            // only gives the scene a label to anchor the dropdown to.
            Color.clear.frame(width: 1, height: 1)
        }
        // Must be the first scene modifier (extends MenuBarExtra, not Scene).
        .menuBarExtraAccess(isPresented: $isMenuPresented) { statusItem in
            statusItemDriver.attach(statusItem)
        }
        .menuBarExtraStyle(.window)
    }

}
