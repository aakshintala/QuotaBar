# Consumers and macOS migration findings

Research date: November 3, 2026.

Repository base: `e12da3525b94561afc3f68ec4eeba59e82762941`.

## Conclusion

The existing feed is a useful starting interface, but it cannot replace the Swift engine inside the menu bar app unchanged. The app needs explicit refresh and settings operations, plus cost data that the current feed omits.

Exact compatibility is not a requirement. The owner controls the clients and accepts behaviour changes, except for important provider lessons. Keep useful field names where that costs nothing; do not build a compatibility framework.

## Agent clients

### Pi extension

`/Users/aakshintala/work/pi-extensions/extensions/status/quota.ts` is the shared client for the footer, `get_quotas` and `/quota`.

Observed behaviour:

- fetches `http://127.0.0.1:<configured port>/quotas`
- uses a 5-second footer deadline and an 8-second tool deadline
- merges concurrent fetches and caches results according to the extension settings
- validates only that `providers` is an array
- displays percentages, balances and counts
- displays `throttledUntil` as last-known data
- does not carry `capturedAt` or `ageSeconds` in its TypeScript provider type
- does not warn about aged observations in its compact renderer unless a provider is throttled
- calls the producer `QuotaBar.app` in help and unavailable messages

The corresponding tests are in `extensions/status/quota.test.mjs`. The repository's `package.json` defines its full test command as `npm test`.

A new daemon should respond from cached state promptly. The current server's 20-second refresh wait exceeds both Pi deadlines. A bounded, immediate cached response with a background refresh is a reasonable intentional change, but it needs a cold-start state and explicit freshness.

The Pi client must render stale data explicitly, including a feed that stops advancing while HTTP requests still succeed. A successful HTTP fetch is not evidence that the quota observation is recent.

### Shell client

`/Users/aakshintala/.agents/bin/quota` fetches the same route, with a 25-second curl timeout. `QUOTA_FEED` overrides its URL. `--json` returns the raw response; otherwise jq formats providers and quota buckets.

It uses `id`, `status`, `unavailable`, `quotas`, `label`, `percentRemaining`, `balanceRemaining`, `balanceUnit` and `resetText`. It does not display age or throttle metadata. Its unavailable message assumes a menu app with a feed toggle.

Do not add a second Rust formatting CLI just to replace this small script. Update the script and its messaging when the daemon is deployed. Revisit a bundled CLI only if distribution makes the external script a real problem.

### Claude Code hooks

`Sources/Infrastructure/Server/QuotaHooks.swift` implements SessionStart and UserPromptSubmit responses. Both use cached observations and never call a provider. The wire response contains `hookSpecificOutput.hookEventName` and `additionalContext`.

SessionStart reports the whole feed and remembers bucket statuses for the session. Prompt hooks report only worsening statuses. Current session tracking never evicts entries and maps malformed or missing session IDs to the same empty-string session. Neither is a behaviour to preserve.

The README documents a 2-second curl deadline for SessionStart and an HTTP prompt hook. Hook bodies may contain prompts. Do not log bodies or persist them. Keep only the session identifier and the minimum status summary needed for suppression, with bounded retention. Validate the envelope against current first-party Claude Code hook documentation before implementation.

## The menu app is not currently a feed client

`Sources/App/QuotaBarApp.swift` constructs provider probes, `QuotaMonitor`, notifications, the listener and the status-item driver. Views read live `AIProvider` and `UsageQuota` objects.

The following app files need deliberate changes when the daemon replaces the engine:

| Existing code | Required change |
|---|---|
| `QuotaBarApp.swift` | Replace probe and listener composition with an observable daemon client and connection state. |
| `Views/Popover/PopoverView.swift` | Load daemon state on open and send explicit refresh requests; do not probe locally. |
| `Views/Popover/ProviderQuotaSection.swift` | Render daemon snapshots, provider errors, staleness and optional cost data. |
| `Views/Popover/QuotaBucketRow.swift` | Render daemon bucket fields and status; keep formatting and accessibility in Swift. |
| `Views/CostStatCard.swift` | Decode or map optional cost observations; do not silently lose existing cost rows. |
| `Views/SettingsView.swift` | Separate local appearance and notification settings from daemon provider and refresh settings. Replace the old feed-off control, which would disconnect the UI from its own engine. |
| `Settings/AppSettings.swift` | Stop writing daemon-owned settings through the legacy whole-file repository. |
| `StatusBarIconDriver.swift` | Keep status-item image recovery; remove ownership of provider polling. |
| `Views/Settings/ClaudeConfigCard.swift`, `CodexConfigCard.swift` | Replace dependencies on live providers with daemon availability and actionable login guidance. |

The current wire DTO in `Sources/Infrastructure/Server/QuotaFeedDTO.swift` includes quota buckets but omits `UsageSnapshot.costUsage`. It also lacks settings and per-provider refresh-in-flight state.

The daemon should compute quota health. The app can format dates and money, but should not reimplement pacing, status thresholds, provider cooldowns or credential loading.

Notifications remain a macOS UI responsibility unless a later requirement says they must run while the menu app is closed. Deduplicate notifications using observations rather than each UI poll. Do not notify simply because a connection recovers.

## Settings need a single writer

`Sources/Infrastructure/Storage/JSONSettingsRepository.swift` reads `~/.quotabar/settings.json` once, then rewrites the complete modeled file on every change. Unknown keys are dropped. Sharing this file between a Rust daemon and the unchanged Swift writer would lose updates.

Recommendation:

- give the Rust daemon a separate configuration file and make it the sole writer of that configuration at runtime
- keep appearance and macOS notification preferences local to the app
- import relevant legacy settings once at macOS cutover, without modifying or deleting the original file
- use authenticated local control operations for UI changes to daemon-owned settings rather than two independent file writers
- fail visibly on malformed configuration or failed persistence; do not report an unpersisted setting as successfully saved

Linux can begin with startup configuration and explicit service restart. Runtime control operations should be introduced only when the Mac UI needs them, with their security tests.

## Lifecycle and migration traps

`Sources/App/StatusBarIconDriver.swift` deliberately drives a static menu icon outside SwiftUI. Its KVO repair avoids image wipes when SwiftUI re-renders the label. Keep this UI-specific fix even after its background monitor code is removed.

The current app's entitlements enable network client and server access, but do not enable App Sandbox. Do not accidentally introduce sandbox restrictions during daemon packaging.

The bundle ID `com.aakshintala.subscriptionusagebar` must remain unchanged to preserve notification permission. This is a standing rule in `CLAUDE.md`.

Do not run the old Swift provider engine and new Rust daemon simultaneously on macOS during normal operation. They would contend for port 8787 and independently consume provider request budgets. Compare fixture outputs offline rather than shadow-polling sensitive endpoints.

A per-user daemon outlives the menu app. Closing the UI should not stop agent quota access. This changes the meaning of Quit and the old background-sync setting; document the new behaviour rather than preserving misleading labels.

## What remains unverified

No agent client has been modified. No tests or real provider requests were run for this note. The Linux worker is separately investigating the clients and credential sources on the remote machine. Paths above describe the local Mac checkout and must not be assumed identical on Linux.
