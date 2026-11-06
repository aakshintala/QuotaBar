import SwiftUI
import Domain
import Infrastructure

/// Main menu-bar popover content.
struct PopoverView: View {
    let monitor: QuotaMonitor
    let quotaAlerter: QuotaAlerter
    var quotadService: QuotadService?

    @Environment(\.appTheme) private var theme
    @State private var showSettings = false
    @State private var settings = AppSettings.shared
    @State private var hasRequestedNotificationPermission = false

    /// Pinned window height — content scrolls inside (popover bounce fix).
    private let popoverHeight: CGFloat = 420

    var body: some View {
        ZStack {
            theme.backgroundGradient.ignoresSafeArea()

            if showSettings {
                SettingsContentView(
                    showSettings: $showSettings,
                    monitor: monitor,
                    quotadService: quotadService
                )
            } else {
                VStack(spacing: 0) {
                    header
                        .padding(.horizontal, 16)
                        .padding(.top, 14)
                        .padding(.bottom, 10)

                    if let quotadService, quotadService.state == .needsApproval {
                        QuotadApprovalBanner(quotadService: quotadService)
                            .padding(.horizontal, 16)
                            .padding(.bottom, 10)
                    }

                    ScrollView(.vertical, showsIndicators: true) {
                        LazyVStack(alignment: .leading, spacing: 16) {
                            ForEach(monitor.enabledProviders, id: \.id) { provider in
                                ProviderQuotaSection(provider: provider)
                            }
                        }
                        .padding(.horizontal, 16)
                        .padding(.bottom, 12)
                    }

                    Divider().overlay(theme.glassBorder)

                    footer
                        .padding(.horizontal, 16)
                        .padding(.vertical, 10)
                }
            }
        }
        .frame(width: 360, height: popoverHeight)
        .clipShape(RoundedRectangle(cornerRadius: 12))
        .task {
            if !hasRequestedNotificationPermission {
                hasRequestedNotificationPermission = true
                if settings.quotaAlertsEnabled {
                    _ = await quotaAlerter.requestPermission()
                }
            }
            await monitor.refresh()
        }
    }

    private var header: some View {
        HStack {
            Text("QuotaBar")
                .font(.system(size: 14, weight: .bold, design: theme.fontDesign))
                .foregroundStyle(theme.textPrimary)
            Spacer()
            Button {
                Task { await monitor.refresh(force: true) }
            } label: {
                Image(systemName: "arrow.clockwise")
            }
            .buttonStyle(.plain)
            .foregroundStyle(theme.textSecondary)

            Button {
                showSettings = true
            } label: {
                Image(systemName: "gearshape")
            }
            .buttonStyle(.plain)
            .foregroundStyle(theme.textSecondary)
        }
    }

    private var footer: some View {
        HStack {
            Text(syncLabel)
                .font(.system(size: 11, design: theme.fontDesign))
                .foregroundStyle(theme.textTertiary)
            Spacer()
            Button {
                NSApplication.shared.terminate(nil)
            } label: {
                Text("Quit")
                    .font(.system(size: 11, design: theme.fontDesign))
            }
            .buttonStyle(.plain)
            .foregroundStyle(theme.textTertiary)
            .help("Quit QuotaBar")
            .keyboardShortcut("q")
        }
    }

    private var syncLabel: String {
        if monitor.enabledProviders.contains(where: \.isSyncing) {
            return "Refreshing…"
        }
        return settings.refreshInterval.label
    }
}
