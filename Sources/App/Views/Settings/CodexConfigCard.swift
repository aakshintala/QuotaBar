import SwiftUI
import Domain

/// Codex provider configuration card for SettingsView.
struct CodexConfigCard: View {
    let provider: AIProvider

    @Environment(\.appTheme) private var theme

    @State private var codexConfigExpanded: Bool = false
    @State private var hasCredentials = false

    var body: some View {
        ConfigCard(isExpanded: $codexConfigExpanded) {
            codexConfigForm
        } label: {
            codexConfigHeader
        }
    }

    private var codexConfigHeader: some View {
        HStack(spacing: 10) {
            ZStack {
                Circle()
                    .fill(
                        LinearGradient(
                            colors: [theme.accentPrimary.opacity(0.2), theme.accentSecondary.opacity(0.1)],
                            startPoint: .topLeading,
                            endPoint: .bottomTrailing
                        )
                    )
                    .frame(width: 28, height: 28)

                Image(systemName: "terminal")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(theme.accentPrimary)
            }

            VStack(alignment: .leading, spacing: 2) {
                Text("Codex Configuration")
                    .font(.system(size: 14, weight: .bold, design: theme.fontDesign))
                    .foregroundStyle(theme.textPrimary)

                Text("ChatGPT API status")
                    .font(.system(size: 10, weight: .medium, design: theme.fontDesign))
                    .foregroundStyle(theme.textTertiary)
            }

            Spacer()
        }
    }

    private var codexConfigForm: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: "network")
                    .font(.system(size: 10))
                    .foregroundStyle(theme.accentPrimary)
                    .frame(width: 16)

                VStack(alignment: .leading, spacing: 2) {
                    Text("ChatGPT API")
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
                Text("Run `codex` in terminal to authenticate, then credentials will be available.")
                    .font(.system(size: 9, weight: .medium, design: theme.fontDesign))
                    .foregroundStyle(theme.textTertiary)
            }
        }
        .task { hasCredentials = await provider.isAvailable() }
    }
}
