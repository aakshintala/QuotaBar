import SwiftUI
import Infrastructure

/// Banner shown when quotad needs Login Items approval, with a button that
/// opens System Settings at Login Items. Shared by the popover and the
/// settings window. A feed that is unreachable for any other reason keeps the
/// existing per-provider "quotad is not running" message instead.
struct QuotadApprovalBanner: View {
    let quotadService: QuotadService
    @Environment(\.appTheme) private var theme

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 11, weight: .bold))
                .foregroundStyle(theme.statusWarning)

            Text("Allow QuotaBar in System Settings › General › Login Items")
                .font(.system(size: 11, design: theme.fontDesign))
                .foregroundStyle(theme.textPrimary)

            Spacer()

            Button("Open Login Items") {
                Task { await quotadService.openLoginItemsSettings() }
            }
            .buttonStyle(.plain)
            .font(.system(size: 11, weight: .semibold, design: theme.fontDesign))
            .foregroundStyle(theme.accentPrimary)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(
            RoundedRectangle(cornerRadius: 8)
                .fill(theme.statusWarning.opacity(0.12))
                .overlay(
                    RoundedRectangle(cornerRadius: 8)
                        .stroke(theme.statusWarning.opacity(0.4), lineWidth: 1)
                )
        )
    }
}
