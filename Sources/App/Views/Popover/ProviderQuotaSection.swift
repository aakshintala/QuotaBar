import SwiftUI
import Domain

/// Provider header + indented quota bucket rows + optional cost card.
struct ProviderQuotaSection: View {
    let provider: AIProvider
    @Environment(\.appTheme) private var theme

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(provider.name)
                .font(.system(size: 11, weight: .semibold, design: theme.fontDesign))
                .foregroundStyle(theme.textTertiary)
                .textCase(.uppercase)
                .tracking(0.6)

            if let snapshot = provider.snapshot {
                ForEach(snapshot.quotas, id: \.quotaType) { quota in
                    QuotaBucketRow(quota: quota)
                }
                if snapshot.quotas.isEmpty {
                    Text("No current quota data")
                        .font(.system(size: 12, design: theme.fontDesign))
                        .foregroundStyle(theme.textTertiary)
                        .padding(.leading, 12)
                }
                if provider.lastError != nil {
                    Text("Showing last known quota; refresh unavailable")
                        .font(.system(size: 10, design: theme.fontDesign))
                        .foregroundStyle(theme.statusWarning)
                        .padding(.leading, 12)
                }

                if let costUsage = snapshot.costUsage {
                    CostStatCard(costUsage: costUsage)
                        .padding(.top, 4)
                }
            } else if provider.isSyncing {
                Text("Refreshing…")
                    .font(.system(size: 12, design: theme.fontDesign))
                    .foregroundStyle(theme.textTertiary)
                    .padding(.leading, 12)
            } else if let error = provider.lastError {
                Text(error.localizedDescription)
                    .font(.system(size: 12, design: theme.fontDesign))
                    .foregroundStyle(theme.statusWarning)
                    .padding(.leading, 12)
            } else {
                Text("No data yet")
                    .font(.system(size: 12, design: theme.fontDesign))
                    .foregroundStyle(theme.textTertiary)
                    .padding(.leading, 12)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
