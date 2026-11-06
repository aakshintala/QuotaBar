import SwiftUI
import Domain
import Infrastructure

/// Inline settings content view that fits within the menu bar popup.
struct SettingsContentView: View {
    @Binding var showSettings: Bool
    let monitor: QuotaMonitor
    var quotadService: QuotadService?
    @Environment(\.appTheme) private var theme
    @State private var settings = AppSettings.shared

    private enum ProviderID {
        static let claude = "claude"
        static let codex = "codex"
    }

    private var maxSettingsHeight: CGFloat {
        let screenHeight = NSScreen.main?.visibleFrame.height ?? 800
        return min(screenHeight * 0.8, 550)
    }

    var body: some View {
        VStack(spacing: 0) {
            header
                .padding(.horizontal, 16)
                .padding(.top, 16)
                .padding(.bottom, 16)

            ScrollView(.vertical, showsIndicators: true) {
                VStack(spacing: 12) {
                    if let quotadService, quotadService.state == .needsApproval {
                        QuotadApprovalBanner(quotadService: quotadService)
                    }

                    themeCard
                    providersCard
                    generalCard
                    feedCard
                }
                .padding(.horizontal, 16)
                .padding(.bottom, 16)
            }

            footer
                .padding(.horizontal, 16)
                .padding(.bottom, 12)
        }
        .frame(maxHeight: maxSettingsHeight)
    }

    // MARK: - Header

    private var header: some View {
        HStack {
            Button {
                showSettings = false
            } label: {
                HStack(spacing: 4) {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 10, weight: .bold))
                    Text("Back")
                        .font(.system(size: 11, weight: .medium, design: theme.fontDesign))
                }
                .foregroundStyle(theme.textPrimary)
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
                .background(
                    Capsule()
                        .fill(theme.glassBackground)
                        .overlay(
                            Capsule()
                                .stroke(theme.glassBorder, lineWidth: 1)
                        )
                )
            }
            .buttonStyle(.plain)

            Spacer()

            Text("Settings")
                .font(.system(size: 16, weight: .bold, design: theme.fontDesign))
                .foregroundStyle(theme.textPrimary)

            Spacer()

            Color.clear
                .frame(width: 60, height: 1)
        }
    }

    // MARK: - Theme

    private var themeCard: some View {
        settingsCard(title: "Appearance", subtitle: "Choose your theme", icon: "paintbrush") {
            LazyVGrid(columns: [
                GridItem(.flexible(), spacing: 8),
                GridItem(.flexible(), spacing: 8)
            ], spacing: 8) {
                ForEach(AppTheme.all, id: \.id) { registeredTheme in
                    ThemeOptionButton(
                        themeProvider: registeredTheme,
                        isSelected: settings.themeMode == registeredTheme.id
                    ) {
                        withAnimation(.spring(response: 0.3, dampingFraction: 0.7)) {
                            settings.themeMode = registeredTheme.id
                        }
                    }
                }
            }
        }
    }

    // MARK: - Providers

    private var providersCard: some View {
        settingsCard(title: "Providers", subtitle: "Enable or disable AI providers", icon: "cpu") {
            VStack(spacing: 8) {
                ForEach(monitor.allProviders, id: \.id) { provider in
                    providerRow(provider: provider)
                    if provider.id == ProviderID.claude, provider.isEnabled {
                        ClaudeConfigCard(provider: provider)
                            .transition(.opacity.combined(with: .move(edge: .top)))
                    }
                    if provider.id == ProviderID.codex, provider.isEnabled {
                        CodexConfigCard(provider: provider)
                            .transition(.opacity.combined(with: .move(edge: .top)))
                    }
                }
            }
        }
    }

    private func providerRow(provider: AIProvider) -> some View {
        HStack(spacing: 10) {
            ProviderIconView(providerId: provider.id, size: 20)

            Text(provider.name)
                .font(.system(size: 12, weight: .medium, design: theme.fontDesign))
                .foregroundStyle(theme.textPrimary)

            Spacer()

            Toggle("", isOn: Binding(
                get: { provider.isEnabled },
                set: { newValue in
                    withAnimation(.easeInOut(duration: 0.2)) {
                        monitor.setProviderEnabled(provider.id, enabled: newValue)
                    }
                }
            ))
            .toggleStyle(.switch)
            .tint(theme.accentPrimary)
            .scaleEffect(0.8)
            .labelsHidden()
        }
        .padding(.vertical, 4)
    }

    // MARK: - Quota Feed

    private var feedCard: some View {
        settingsCard(
            title: "Quota Feed",
            subtitle: "Served by the quotad daemon",
            icon: "antenna.radiowaves.left.and.right"
        ) {
            VStack(alignment: .leading, spacing: 10) {
                Text("http://127.0.0.1:\(settings.feedPort)/quotas")
                    .font(.system(size: 12, weight: .medium, design: theme.fontDesign))
                    .foregroundStyle(theme.textPrimary)

                Text("QuotaBar reads quotas from quotad, which owns the feed, the Claude Code hooks and the provider credentials. quotad reads the port from the same settings file; QuotaBar never changes it.")
                    .font(.system(size: 9, weight: .semibold, design: theme.fontDesign))
                    .foregroundStyle(theme.textTertiary)
            }
        }
    }

    // MARK: - General

    private var generalCard: some View {
        settingsCard(title: "General", subtitle: "Alerts and background refresh", icon: "gearshape") {
            VStack(alignment: .leading, spacing: 14) {
                Toggle(isOn: $settings.quotaAlertsEnabled) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Quota alerts")
                            .font(.system(size: 12, weight: .medium, design: theme.fontDesign))
                            .foregroundStyle(theme.textPrimary)
                        Text("Notify when quota status degrades")
                            .font(.system(size: 9, weight: .medium, design: theme.fontDesign))
                            .foregroundStyle(theme.textTertiary)
                    }
                }
                .toggleStyle(.switch)
                .tint(theme.accentPrimary)

                VStack(alignment: .leading, spacing: 6) {
                    Text("REFRESH INTERVAL")
                        .font(.system(size: 9, weight: .semibold, design: theme.fontDesign))
                        .foregroundStyle(theme.textSecondary)
                        .tracking(0.5)

                    Picker("", selection: $settings.refreshInterval) {
                        ForEach(RefreshInterval.allCases, id: \.self) { interval in
                            Text(interval.label).tag(interval)
                        }
                    }
                    .pickerStyle(.segmented)

                    Text("Keep the menu-bar number fresh in the background. \"Off\" updates only when you open the menu.")
                        .font(.system(size: 9, weight: .semibold, design: theme.fontDesign))
                        .foregroundStyle(theme.textTertiary)
                }
            }
        }
    }

    // MARK: - Footer

    private var footer: some View {
        HStack {
            Spacer()

            Button {
                withAnimation(.easeInOut(duration: 0.2)) {
                    showSettings = false
                }
            } label: {
                Text("Done")
                    .font(.system(size: 11, weight: .medium, design: theme.fontDesign))
                    .foregroundStyle(.white)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 7)
                    .background(
                        Capsule()
                            .fill(theme.accentGradient)
                            .shadow(color: theme.accentSecondary.opacity(0.25), radius: 6, y: 2)
                    )
            }
            .buttonStyle(.plain)
        }
    }

    // MARK: - Card Helper

    private func settingsCard<Content: View>(
        title: String,
        subtitle: String,
        icon: String,
        @ViewBuilder content: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                ZStack {
                    Circle()
                        .fill(theme.accentGradient)
                        .frame(width: 32, height: 32)

                    Image(systemName: icon)
                        .font(.system(size: 12, weight: .bold))
                        .foregroundStyle(.white)
                }

                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(.system(size: 14, weight: .bold, design: theme.fontDesign))
                        .foregroundStyle(theme.textPrimary)

                    Text(subtitle)
                        .font(.system(size: 10, weight: .medium, design: theme.fontDesign))
                        .foregroundStyle(theme.textTertiary)
                }

                Spacer()
            }

            content()
        }
        .padding(14)
        .background(
            RoundedRectangle(cornerRadius: theme.cardCornerRadius)
                .fill(theme.cardGradient)
                .overlay(
                    RoundedRectangle(cornerRadius: theme.cardCornerRadius)
                        .stroke(theme.glassBorder, lineWidth: 1)
                )
        )
    }
}

// MARK: - Theme Option Button

struct ThemeOptionButton: View {
    let themeProvider: AppTheme
    let isSelected: Bool
    let action: () -> Void

    @Environment(\.appTheme) private var theme
    @State private var isHovering = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                ZStack {
                    Circle()
                        .fill(themeProvider.accentGradient)
                        .frame(width: 28, height: 28)

                    Image(systemName: themeProvider.icon)
                        .font(.system(size: 11, weight: .bold))
                        .foregroundStyle(.white)
                }

                VStack(alignment: .leading, spacing: 1) {
                    Text(themeProvider.displayName)
                        .font(.system(size: 11, weight: .medium, design: themeProvider.fontDesign))
                        .foregroundStyle(theme.textPrimary)
                        .lineLimit(1)
                }

                Spacer()

                if isSelected {
                    Image(systemName: "checkmark.circle.fill")
                        .font(.system(size: 14))
                        .foregroundStyle(theme.statusHealthy)
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .background(
                RoundedRectangle(cornerRadius: themeProvider.cardCornerRadius)
                    .fill(isSelected ? theme.accentPrimary.opacity(0.15) : (isHovering ? theme.hoverOverlay : Color.clear))
                    .overlay(
                        RoundedRectangle(cornerRadius: themeProvider.cardCornerRadius)
                            .stroke(isSelected ? theme.accentPrimary : theme.glassBorder.opacity(0.5), lineWidth: isSelected ? 2 : 1)
                    )
            )
            .scaleEffect(isHovering ? 1.02 : 1.0)
        }
        .buttonStyle(.plain)
        .onHover { isHovering = $0 }
    }
}

// MARK: - Preview

#Preview("Settings - Dark") {
    ZStack {
        AppTheme.dark.backgroundGradient
        SettingsContentView(
            showSettings: .constant(true),
            monitor: QuotaMonitor(providers: [])
        )
    }
    .appThemeProvider(themeModeId: "dark")
    .frame(width: 380, height: 420)
}

#Preview("Settings - Light") {
    ZStack {
        AppTheme.light.backgroundGradient
        SettingsContentView(
            showSettings: .constant(true),
            monitor: QuotaMonitor(providers: [])
        )
    }
    .appThemeProvider(themeModeId: "light")
    .frame(width: 380, height: 420)
}
