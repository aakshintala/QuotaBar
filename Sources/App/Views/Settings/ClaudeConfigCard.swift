import SwiftUI
import Domain

/// Claude provider configuration card for SettingsView.
struct ClaudeConfigCard: View {
    let provider: AIProvider

    @Environment(\.appTheme) private var theme

    @State private var claudeConfigExpanded: Bool = false
    @State private var hasCredentials = false

    var body: some View {
        ConfigCard(isExpanded: $claudeConfigExpanded) {
            configForm
        } label: {
            configHeader
        }
    }

    private var configHeader: some View {
        HStack(spacing: 10) {
            ZStack {
                Circle()
                    .fill(
                        LinearGradient(
                            colors: [
                                Color(red: 0.85, green: 0.55, blue: 0.35),
                                Color(red: 0.75, green: 0.40, blue: 0.30)
                            ],
                            startPoint: .topLeading,
                            endPoint: .bottomTrailing
                        )
                    )
                    .frame(width: 32, height: 32)

                Image(systemName: "gear")
                    .font(.system(size: 12, weight: .bold))
                    .foregroundStyle(.white)
            }

            VStack(alignment: .leading, spacing: 2) {
                Text("Claude Configuration")
                    .font(.system(size: 14, weight: .bold, design: theme.fontDesign))
                    .foregroundStyle(theme.textPrimary)

                Text("Anthropic API status")
                    .font(.system(size: 10, weight: .medium, design: theme.fontDesign))
                    .foregroundStyle(theme.textTertiary)
            }

            Spacer()
        }
    }

    private var configForm: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: "network")
                    .font(.system(size: 10))
                    .foregroundStyle(theme.accentPrimary)
                    .frame(width: 16)

                VStack(alignment: .leading, spacing: 2) {
                    Text("Anthropic API")
                        .font(.system(size: 10, weight: .semibold, design: theme.fontDesign))
                        .foregroundStyle(theme.textPrimary)

                    Text("Quotas come from quotad's local feed; quotad holds the credentials.")
                        .font(.system(size: 9, weight: .medium, design: theme.fontDesign))
                        .foregroundStyle(theme.textTertiary)
                }
            }

            HStack(spacing: 6) {
                Image(systemName: hasCredentials ? "checkmark.circle.fill" : "exclamationmark.triangle.fill")
                    .font(.system(size: 10))
                    .foregroundStyle(hasCredentials ? theme.statusHealthy : theme.statusWarning)

                Text(hasCredentials ? "Available via quotad" : "Not available via quotad")
                    .font(.system(size: 9, weight: .semibold, design: theme.fontDesign))
                    .foregroundStyle(hasCredentials ? theme.statusHealthy : theme.statusWarning)
            }

            if !hasCredentials {
                Text("Run `claude` in terminal to authenticate, then credentials will be available.")
                    .font(.system(size: 9, weight: .medium, design: theme.fontDesign))
                    .foregroundStyle(theme.textTertiary)
            }
        }
        .task { hasCredentials = await provider.isAvailable() }
    }
}
