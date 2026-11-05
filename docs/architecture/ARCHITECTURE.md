# QuotaBar architecture

How the pieces fit and how a refresh flows. Build, test, settings keys and logging live in [`CLAUDE.md`](../../CLAUDE.md).

## Layers

```
App (Sources/App)                 SwiftUI; reads QuotaMonitor directly, no view model
  QuotaBarApp                     composition root: builds providers, monitor, feed client, status item
  PopoverView, SettingsView       menu bar dropdown and its settings page
  StatusBarIconDriver             static status-item icon + background refresh lifecycle
  AppSettings                     @Observable facade over the settings file
  AppTheme (.dark, .light)        colours, passed through the environment
        │
Domain (Sources/Domain)           no I/O; protocols are @Mockable
  QuotaMonitor                    single source of truth: providers, refresh, alerts, background loop
  AIProvider                      one class for every provider: id, name, probe, isEnabled, snapshot, lastError
  UsageProbe                      protocol: isAvailable(), probe() -> UsageSnapshot
  UsageSnapshot, UsageQuota       quotas with status, reset time, balance or units
  QuotaStatus                     healthy < warning < critical < depleted
  ProviderSettingsRepository      per-provider isEnabled
  QuotaAlerter, Clock, PowerStateProvider
        │
Infrastructure (Sources/Infrastructure)
  Feed/FeedClient, FeedUsageProbe    read-only client of quotad's `GET /quotas` (port from `feed.port`); one shared request per refresh, 5 s reuse
  Server/QuotaFeedDTO                wire types for the feed; the shape must not drift from what quotad serves
  Storage/JSONSettingsRepository     SettingsFile <-> ~/.quotabar/settings.json
  Notifications/NotificationAlerter  QuotaAlerter -> UNUserNotificationCenter
  Shared/                            NetworkClient over URLSession, ProbeHelpers, system clock and power state
  Logging/                           AppLog: OSLog + ~/Library/Logs/QuotaBar/QuotaBar.log
```

## Refresh flow

```
popover open / refresh button (force) / background tick
        │
QuotaMonitor.refresh(force:)
        │  for each enabled provider, concurrently:
        │    skip if snapshot < 60 s old and not forced
        │    skip if isAvailable() is false (disabled or absent from the feed)
        │    join the in-flight Task if one exists
        ▼
AIProvider.refresh()     ── isSyncing, snapshot, lastError (main actor)
        │
FeedUsageProbe.probe()   ── off the main actor; GET /quotas via FeedClient
        │
QuotaMonitor compares snapshot.overallStatus with the last one
        └── changed → QuotaAlerter.alert(...) (if app.quotaAlertsEnabled)
```

- The app never probes a provider API and never serves HTTP. Probes, credential handling, the feed server and the Claude Code hooks live in `quotad/`; see `docs/plans/go-daemon.md`.
- `FeedClient` shares one in-flight `GET /quotas` across all five probes of a refresh and reuses a response younger than 5 s.
- `FeedUsageProbe` maps its provider's entry (quota keys invert `QuotaType.quotaKey`; tier becomes `AccountTier.custom`). Quotas with `unavailable` throw `staleSnapshot`, so the data still shows with the message in `lastError`; `capturedAt` is the feed's `generatedAt` minus `ageSeconds`.
- The background loop runs every `app.backgroundSyncInterval` (floor 60 s, doubled on battery) and pauses while the Mac sleeps.

## Quota status

`UsageQuota.status` is the one rule the popover, alerts and feed share:

- At 0% it is depleted.
- With no reset time: critical below 20%, warning below 50%, otherwise healthy.
- With a reset time it is pace-aware, and otherwise healthy. Burn rate = percent used ÷ percent of the window elapsed. Below 20% left, critical only when the burn rate exceeds 1 (it will run out before reset). Below 50% left, warning when the burn rate exceeds 1.5.
- A balance-only quota (no percentage) is always healthy.

## Design rules

- Domain types own their behaviour (status, pace, formatting helpers); views and the feed read them rather than recomputing.
- New external dependencies enter as a `@Mockable` protocol in Domain or a small injectable seam (`NetworkClient`, `Clock`, `PowerStateProvider`).
- Tests assert on resulting state (Chicago school). Mocks stub data; `verify()` is for real boundaries only.
