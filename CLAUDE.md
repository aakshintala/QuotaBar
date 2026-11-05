# CLAUDE.md

QuotaBar is a macOS menu bar app (Swift 6, SwiftUI, macOS 15+) that shows how much of each AI coding subscription is left. It is a client of the `quotad` Go daemon: quotad runs the five provider probes and serves `GET http://127.0.0.1:8787/quotas`, and the app reads that feed to drive its menu bar icon, popover, settings window, themes and quota alerts. The app never probes a provider API and never serves HTTP itself. It started as a fork of ClaudeBar and was cut down to five providers; read the code, not old upstream docs or issues.

## Providers

| id | Name |
|----|------|
| `claude` | Claude |
| `codex` | Codex |
| `cursor` | Cursor |
| `opencode-go` | OpenCode Go |
| `muse` | Muse |

Every provider is the one `AIProvider` class with a `FeedUsageProbe(id:client:)` reading its entry from quotad's feed. The id is the settings key and the feed id: never change one that has shipped. Probes, credential handling, the feed server and the Claude Code hooks all live in `quotad/`; see `docs/plans/go-daemon.md`.

## Layers

| Layer | Path | Holds |
|-------|------|-------|
| Domain | `Sources/Domain/` | `AIProvider`, `UsageProbe`, `UsageSnapshot`/`UsageQuota`, `QuotaMonitor`, `ProviderSettingsRepository`, `QuotaAlerter` |
| Infrastructure | `Sources/Infrastructure/` | feed client (`Feed/`), settings file, notifications, logging |
| App | `Sources/App/` | `QuotaBarApp` (composition root), popover and settings views, `AppSettings`, themes, `StatusBarIconDriver` |

- `QuotaMonitor` is the single source of truth. Views read it directly; there is no view model.
- `QuotaMonitor.refresh(force:)` refreshes all enabled providers concurrently, skips a snapshot younger than `minimumSnapshotAge` (60 s) unless forced, and joins a refresh already in flight. The popover, the refresh button (`force: true`) and the background loop all go through it.
- `FeedClient` shares one in-flight `GET /quotas` across all five probes of a refresh and reuses a response younger than 5 s. `FeedUsageProbe` maps its provider's `QuotaFeedProviderDTO` to a `UsageSnapshot` (quota keys invert `QuotaType.quotaKey`; tier becomes `AccountTier.custom`).
- `UsageQuota.status` is pace-aware, so the popover and alerts agree with the feed on healthy/warning/critical/depleted.
- A quota is a percent meter, a balance meter (`balanceRemaining`/`balanceUsed`/`balanceCap` in `usd` or `credits`), or a units count (`unitsUsed`/`unitsLimit`). quotad sets `resetsAt`; reset text is formatted once, at the edge.

## Adding a provider

Add the probe and credentials to `quotad/` first. On the Swift side every provider is the one `AIProvider` class; what differs is only the feed id:

1. Register it with one line in the `providers` list in `Sources/App/QuotaBarApp.swift`.
2. Add its colour, symbol and icon asset to `Sources/App/Views/ProviderVisualIdentity.swift` and `Assets.xcassets`.

Add a settings sub-protocol of `ProviderSettingsRepository` only when the provider needs its own setting.

## Settings

One JSON file, `~/.quotabar/settings.json`, modelled by the Codable `SettingsFile` and owned by `JSONSettingsRepository` (read once at launch, whole file rewritten atomically on each change). `AppSettings` is the `@Observable` facade SwiftUI binds to.

| Key | Meaning |
|-----|---------|
| `app.themeMode` | `dark` or `light` |
| `app.backgroundSyncEnabled`, `app.backgroundSyncInterval` | background refresh loop (60 s floor, doubled on battery) |
| `app.quotaAlertsEnabled` | system notifications when a quota's status worsens |
| `feed.port` | quotad's localhost feed port (default 8787); `feed.enabled` is owned by quotad and the app never writes it |
| `providers.<id>.isEnabled` | per-provider toggle |

Keys the struct does not model are dropped on the next write. Migrations run at load: `~/.claudebar/settings.json` moves to the new path once if the new file is missing, and an old `mcp` section is read as `feed` when `feed` is absent.

## Quota feed

quotad serves the feed on `127.0.0.1:<feed.port>` (`/quotas`, `/hooks/session-start`, `/hooks/prompt`); the app is a read-only client via `FeedClient`/`FeedUsageProbe` in `Sources/Infrastructure/Feed/`. The wire types stay in `Sources/Infrastructure/Server/QuotaFeedDTO.swift` so the DTO shape cannot drift from what quotad serves.

`/quotas` has two other consumers, the pi status extension at `~/work/pi-extensions/extensions/status/quota.ts` and the agent script `~/.agents/bin/quota` (behind the `quota` skill); change both in step with any change to the DTO shape.

## Themes

`AppTheme` is a concrete struct with two values, `AppTheme.dark` and `AppTheme.light` (`Sources/App/Theme/AppTheme.swift`), chosen by `app.themeMode` and passed through the SwiftUI environment. A new theme is a new static value added to `AppTheme.all`.

## Logging

`AppLog.<category>` (`monitor`, `providers`, `probes`, `network`, `credentials`, `ui`, `notifications`) writes to OSLog under subsystem `com.aakshintala.subscriptionusagebar` and, from `info` up, to `~/Library/Logs/QuotaBar/QuotaBar.log` (rotated to `QuotaBar.old.log` at 5 MB). Messages are plain strings with public privacy, so redact tokens and keys before logging.

```bash
tail -f ~/Library/Logs/QuotaBar/QuotaBar.log
log stream --predicate 'subsystem == "com.aakshintala.subscriptionusagebar"' --info --debug
```

## Build and test

Tuist generates `QuotaBar.xcworkspace` (git-ignored). Targets: `QuotaBar` (app), `Domain`, `Infrastructure`, and the test bundles `DomainTests`, `InfrastructureTests`, `AcceptanceTests`, all run by the shared `QuotaBar` scheme.

```bash
tuist install
tuist generate --no-open
xcodebuild test -workspace QuotaBar.xcworkspace -scheme QuotaBar -destination 'platform=macOS' \
  2>&1 | grep -E 'error:|✘|Test run with|TEST (SUCCEEDED|FAILED)'
tuist build                     # Debug
tuist build QuotaBar -C Release # Release: QuotaBar.app, executable Contents/MacOS/QuotaBar
```

Run tests only through `xcodebuild` as above. `tuist test` regenerates the scheme without its test action, runs nothing and exits 0, a silent false pass; if it has been run, `tuist generate --no-open` restores the scheme.

Green is `** TEST SUCCEEDED **` with no `✘`. xcodebuild prints one `Test run with N tests` line per bundle; the suite total is their sum (192: 142 + 34 + 16 for DomainTests, InfrastructureTests, AcceptanceTests). A run with no such lines ran nothing. Record the new total in commit messages.

Tests are Swift Testing (`@Test`, `#expect`) in Chicago style: stub `@Mockable` protocols to return data and assert on resulting state; use `verify()` only at real boundaries. Shared fixtures live in `Tests/Support/`.

Bundle id is `com.aakshintala.subscriptionusagebar`; keep it, since changing it resets notification permission. Version lives in `Sources/App/Info.plist`. There is no auto-update and no release workflow; CI (`.github/workflows/build.yml`, `tests.yml`) builds and tests on push and PR.

## Dependencies

`Mockable` (protocol mocks for tests) and `MenuBarExtraAccess` (status item access for `MenuBarExtra`), declared in `Tuist/Package.swift`.
