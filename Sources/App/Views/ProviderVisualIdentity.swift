import SwiftUI
import Domain

// MARK: - Static Provider Identity Lookup

/// Static helpers to look up provider visual identity by ID string.
/// Used by views that only have a providerId, not the full AIProvider object.
enum ProviderVisualIdentityLookup {
    /// Get provider theme color by ID
    static func color(for providerId: String, scheme: ColorScheme) -> Color {
        switch providerId {
        case "claude":
            return scheme == .dark
                ? BaseTheme.coralAccent
                : Color(red: 0.95, green: 0.48, blue: 0.38)
        case "codex":
            return scheme == .dark
                ? BaseTheme.tealBright
                : Color(red: 0.18, green: 0.72, blue: 0.68)
        case "cursor":
            return scheme == .dark
                ? Color(red: 0.20, green: 0.78, blue: 0.82)
                : Color(red: 0.12, green: 0.62, blue: 0.66)
        case "opencode-go":
            return scheme == .dark
                ? Color(red: 0.52, green: 0.36, blue: 1.0)
                : Color(red: 0.42, green: 0.28, blue: 1.0)
        case "muse":
            return scheme == .dark ? Color(red: 0.42, green: 0.56, blue: 0.94) : Color(red: 0.24, green: 0.38, blue: 0.78)
        default:
            return BaseTheme.purpleVibrant
        }
    }

    /// Get provider gradient by ID
    static func gradient(for providerId: String, scheme: ColorScheme) -> LinearGradient {
        let primaryColor = color(for: providerId, scheme: scheme)
        let secondaryColor: Color

        switch providerId {
        case "claude":
            secondaryColor = scheme == .dark
                ? BaseTheme.pinkHot
                : Color(red: 0.92, green: 0.45, blue: 0.72)
        case "codex":
            secondaryColor = scheme == .dark
                ? Color(red: 0.25, green: 0.65, blue: 0.85)
                : Color(red: 0.12, green: 0.52, blue: 0.72)
        case "cursor":
            secondaryColor = scheme == .dark
                ? Color(red: 0.15, green: 0.55, blue: 0.75)
                : Color(red: 0.08, green: 0.45, blue: 0.60)
        case "opencode-go":
            secondaryColor = scheme == .dark
                ? Color(red: 0.36, green: 0.20, blue: 0.90)
                : Color(red: 0.30, green: 0.15, blue: 0.80)
        case "muse":
            secondaryColor = scheme == .dark ? Color(red: 0.68, green: 0.43, blue: 0.93) : Color(red: 0.54, green: 0.30, blue: 0.80)
        default:
            return LinearGradient(
                colors: [BaseTheme.coralAccent, BaseTheme.pinkHot],
                startPoint: .leading,
                endPoint: .trailing
            )
        }

        return LinearGradient(
            colors: [primaryColor, secondaryColor],
            startPoint: .topLeading,
            endPoint: .bottomTrailing
        )
    }

    /// Get provider icon asset name by ID
    static func iconAssetName(for providerId: String) -> String {
        switch providerId {
        case "claude": return "ClaudeIcon"
        case "codex": return "CodexIcon"
        case "cursor": return "CursorIcon"
        case "opencode-go": return "OpenCodeIcon"
        case "muse": return "MuseIcon"
        default: return "QuestionIcon"
        }
    }

    /// Get provider SF symbol icon by ID
    static func symbolIcon(for providerId: String) -> String {
        switch providerId {
        case "claude": return "brain.fill"
        case "codex": return "chevron.left.forwardslash.chevron.right"
        case "cursor": return "cursorarrow.rays"
        case "opencode-go": return "square.stack.3d.up.fill"
        case "muse": return "infinity"
        default: return "questionmark.circle.fill"
        }
    }
}
