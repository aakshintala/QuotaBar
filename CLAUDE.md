# CLAUDE.md

QuotaBar is a macOS menu bar app (Swift 6, SwiftUI, macOS 15+) that shows how much of each AI coding subscription is left and serves the same data to other tools over a localhost HTTP feed. It started as a fork of ClaudeBar and was cut down to five providers; read the code, not old upstream docs or issues.

## Providers

| id | Name | Probe | Credentials read |
|----|------|-------|------------------|
| `claude` | Claude | `ClaudeAPIUsageProbe` | Claude Code OAuth: `~/.claude/.credentials.json` or Keychain item `Claude Code-credentials` |
| `codex` | Codex | `CodexAPIUsageProbe` | `~/.codex/auth.json` |
| `cursor` | Cursor | `CursorUsageProbe` | token from Cursor's `state.vscdb` via `sqlite3` |
| `opencode-go` | OpenCode Go | `OpenCodeUsageProbe` | `$XDG_DATA_HOME/opencode/auth.json` |
| `muse` | Muse | `MuseUsageProbe` | Muse CLI OAuth: `$XDG_CONFIG_HOME/muse/auth.json` or `~/.config/muse/auth.json` |

All five call HTTP APIs; nothing drives an interactive CLI. The id is the settings key and the feed id: never change one that has shipped.

The Keychain item `Claude Code-credentials` belongs to Claude Code and trusts only `/usr/bin/security`, so the Claude loader reads and writes it through `security` (over stdin, never argv). Calling `SecItem*` on it would prompt the user.

## Layers

| Layer | Path | Holds |
|-------|------|-------|
| Domain | `Sources/Domain/` | `AIProvider`, `UsageProbe`, `UsageSnapshot`/`UsageQuota`, `QuotaMonitor`, `ProviderSettingsRepository`, `QuotaAlerter` |
| Infrastructure | `Sources/Infrastructure/` | probes and credential loaders (one folder per provider), settings file, feed server, notifications, logging, `runProcess` |
| App | `Sources/App/` | `QuotaBarApp` (composition root), popover and settings views, `AppSettings`, themes, `StatusBarIconDriver` |

- `QuotaMonitor` is the single source of truth. Views read it directly; there is no view model.
- `QuotaMonitor.refresh(force:)` refreshes all enabled providers concurrently, skips a snapshot younger than `minimumSnapshotAge` (60 s) unless forced, and joins a refresh already in flight. The popover, the refresh button (`force: true`), the background loop and the feed all go through it.
- `ClaudeAPIUsageProbe` also caches its snapshot for `snapshotCacheTTL` (300 s), even on a forced refresh, because the usage endpoint rate-limits hard.
- Muse persists its per-login cooldown and last quota observation under `~/.quotabar/muse`; a failed refresh keeps any unexpired observation visible with the failure in `lastError`, including after restart. Its CLI OAuth record must also contain an already-issued `providers.meta.api_key`; QuotaBar deliberately does not invoke Muse's undocumented key-issuance endpoint.
- `UsageQuota.status` is pace-aware, so the popover, alerts and feed agree on healthy/warning/critical/depleted.
- A quota is a percent meter, a balance meter (`balanceRemaining`/`balanceUsed`/`balanceCap` in `usd` or `credits`), or a units count (`unitsUsed`/`unitsLimit`). Probes set `resetsAt`; reset text is formatted once, at the edge.

## Adding a provider

Every provider is the one `AIProvider` class; what differs is its `UsageProbe`. Use the `add-provider` skill (`.claude/skills/add-provider/SKILL.md`). In short:

1. Write the probe test-first in `Sources/Infrastructure/<Name>/`, with a static parser tested on a captured response and the HTTP call tested through `MockNetworkClient`. Reuse `NetworkClient.send(_:label:)` and `parseISO8601` from `Shared/ProbeHelpers.swift`.
2. Register it with one line in the `providers` list in `Sources/App/QuotaBarApp.swift`.
3. Add its colour, symbol and icon asset to `Sources/App/Views/ProviderVisualIdentity.swift` and `Assets.xcassets`.

Add a settings sub-protocol of `ProviderSettingsRepository` only when the provider needs its own setting. API keys (OpenRouter is next) belong in the Keychain, in items QuotaBar creates and reads with `SecItem*`, not in the settings file or UserDefaults. That credential store does not exist yet; the first API-key provider builds it. Muse uses the CLI's existing OAuth file; its inference key alone cannot fetch subscription quota.

## Settings

One JSON file, `~/.quotabar/settings.json`, modelled by the Codable `SettingsFile` and owned by `JSONSettingsRepository` (read once at launch, whole file rewritten atomically on each change). `AppSettings` is the `@Observable` facade SwiftUI binds to.

| Key | Meaning |
|-----|---------|
| `app.themeMode` | `dark` or `light` |
| `app.backgroundSyncEnabled`, `app.backgroundSyncInterval` | background refresh loop (60 s floor, doubled on battery) |
| `app.quotaAlertsEnabled` | system notifications when a quota's status worsens |
| `feed.enabled`, `feed.port` | localhost feed (default off, port 8787) |
| `providers.<id>.isEnabled` | per-provider toggle |

Keys the struct does not model are dropped on the next write. Migrations run at load: `~/.claudebar/settings.json` moves to the new path once if the new file is missing, and an old `mcp` section is read as `feed` when `feed` is absent.

## Quota feed

When `feed.enabled` is on, `FeedServerController` runs `QuotaHTTPServer` on `127.0.0.1:<feed.port>` (loopback only; requests whose `Host` is not `127.0.0.1` or `localhost` are rejected).

| Endpoint | Does |
|----------|------|
| `GET /quotas` | refreshes through the monitor (waits at most 20 s), then returns `QuotaFeedDTO` JSON |
| `POST /hooks/session-start` | Claude Code SessionStart hook: the cached feed as context text |
| `POST /hooks/prompt` | Claude Code UserPromptSubmit hook: speaks only when a bucket got worse this session |

The hooks never trigger a probe. `/quotas` has two other consumers, the pi status extension at `~/work/pi-extensions/extensions/status/quota.ts` and the agent script `~/.agents/bin/quota` (behind the `quota` skill); change both in step with any change to the DTO shape.

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

Green is `** TEST SUCCEEDED **` with no `✘`. xcodebuild prints one `Test run with N tests` line per bundle; the suite total is their sum (343 as of 2026-09-26: 18 + 141 + 184). A run with no such lines ran nothing. Record the new total in commit messages.

Tests are Swift Testing (`@Test`, `#expect`) in Chicago style: stub `@Mockable` protocols to return data and assert on resulting state; use `verify()` only at real boundaries. Shared fixtures live in `Tests/Support/`.

Bundle id is `com.aakshintala.subscriptionusagebar`; keep it, since changing it resets notification permission. Version lives in `Sources/App/Info.plist`. There is no auto-update and no release workflow; CI (`.github/workflows/build.yml`, `tests.yml`) builds and tests on push and PR.

## Dependencies

`Mockable` (protocol mocks for tests) and `MenuBarExtraAccess` (status item access for `MenuBarExtra`), declared in `Tuist/Package.swift`.
