# Rust quota daemon migration plan

## Goal

Replace QuotaBar's Swift quota engine with one Rust daemon on the Mac and one on Linux. Agents and a small `quota` command will use the daemon over localhost HTTP. Do not build a new Mac interface.

Keep the existing Swift app untouched while the replacement is built and accepted. At cutover, stop the Swift engine before starting the daemon on that machine. Remove the Swift probes and server only after both hosts and all consumers work.

Use one Cargo package in `daemon/` that builds `quotabard`. Keep the design direct: Tokio, Axum, Reqwest with Rustls, Serde and `serde_json`. Do not add a database, plugin system, Docker requirement, updater, TUI or Swift-to-Rust bridge. Use native Rust bindings for macOS APIs only where required.

This plan uses baseline `e12da3525b94561afc3f68ec4eeba59e82762941` and the [provider research](../research/rust-migration-provider-gotchas.md), [consumer research](../research/rust-migration-consumers.md), [Linux credential research](../research/rust-migration-linux-credentials.md), and [runtime and deployment research](../research/rust-migration-runtime-deployment.md). Research records observations, not provider guarantees. Exact Swift wire parity is not required.

## Decisions from the owner

- Support Claude, Codex, Cursor, OpenCode Go and Muse.
- Run on-demand checks only. Starting the service does not call providers, and there is no periodic polling.
- Check eligible providers concurrently. An agent request waits no more than five seconds total. A timed-out caller does not cancel shared provider work.
- Reuse successful readings for five minutes. After that, the next request starts an eligible check. Return the latest reading with its age and error when a check is slow or fails. Return unknown when no reading exists.
- Keep cooldowns across daemon restarts, but do not save quota readings for any provider. After restart, providers start unknown. In particular, do not retain Muse's current saved-reading exception.
- Preserve actual future `Retry-After` values. Keep Claude's five-minute successful-result cache and Muse's five-minute failed-attempt cooldown. Do not impose a new universal failed-attempt cooldown on other providers.
- Renew Claude and Codex credentials automatically where their existing refresh flows allow it. Cursor and Muse remain read-only. Credential access must never cause a password dialog.
- Use every provider with a usable login unless the owner disables it. `quota enable <provider>` and `quota disable <provider>` change the same daemon setting.
- Keep healthy, warning, critical and depleted quota labels. Compute them from the quota data and current time when returning a response.
- Keep quota percentages, USD or credit balances, count meters and reset times. Drop Claude spend-history details.
- Keep both installed Claude Code hooks: SessionStart startup context and UserPromptSubmit cached-only quota warnings. Preserve their POST paths and response envelope. Per-prompt warnings may compare status history only; they never probe providers. Do not add notifications.
- Build Linux service persistence into the plan: the user service starts at boot and remains available after logout. This is approval to plan for lingering, not to enable it now.

The owner has not approved installs, service configuration changes or implementation. Do not make them as part of this planning work.

## Runtime behavior

The daemons on the two hosts are independent. Each reads credentials from that host's existing CLI or Keychain login. Do not copy credentials between machines. Their cooldowns are local; provider rate-limit scope across hosts is unknown.

### On-demand checks and cache

Every `GET /quotas` request uses the same on-demand path. It starts eligible checks together for enabled providers with no successful reading, or whose successful reading is at least five minutes old, then waits up to 4.5 seconds for results. Both cases respect existing cooldowns and join any check already in flight. The full daemon response must finish within five seconds. Provider HTTP attempts have finite deadlines. A refresh that needs an OAuth renewal can continue after the caller's deadline; the next request can use its result. The daemon coalesces overlapping requests into one in-flight attempt per provider. Do not add a cached-only query parameter or alternate GET policy. Increase the Pi footer transport timeout from five to eight seconds, matching its existing quota-tool timeout. An HTTP client timeout never cancels shared provider work.

Return the latest reading even when it is older than five minutes. Set `capturedAt` only when a provider returns a successful observation. Derive `ageSeconds`, `resetText`, bucket status, provider status and freshness when serializing the response. Use wall-clock capture time for age so sleep counts. A clock value in the future is invalid; report the provider unknown until a new successful observation.

Remove quota windows whose `resetsAt` is at or before response time. Do not invent a full quota for missing or expired windows. If no current observation exists, return unknown with empty quotas. Keep severity (`healthy`, `warning`, `critical`, `depleted`, `unknown`) separate from freshness (`unknown`, `fresh`, `stale`). A balance-only quota is not zero percent. Preserve pace-aware status where the window duration is meaningful; use absolute thresholds for Cursor billing-cycle meters whose duration is not represented by the generic window type.

The Claude Code hooks keep the installed `POST /hooks/session-start` and `POST /hooks/prompt` contracts and existing hook response envelope. The handlers call the shared refresh/cache service directly, not a nested HTTP request. SessionStart starts eligible checks, waits up to 1.5 seconds, and returns within two seconds using the shared formatter; this fits the installed two-second curl timeout and three-second Claude hook timeout. Provider work continues after the hook response. UserPromptSubmit only reads cached state and returns within two seconds; it never starts provider work. Keep a bounded in-memory history of the last reported severity per provider and bucket: require a valid session ID, expire idle sessions after 24 hours, and cap the map at 1,024 sessions with least-recently-used eviction. Missing or malformed IDs do not share an empty session or update history. Do not log or retain hook bodies or prompt text.

### Cooldowns and persisted state

Keep provider cooldown state under `~/.quotabar/daemon/`, in a mode-0700 directory with mode-0600 files. Store only provider ID, enforced `notBefore`, and the minimum metadata needed to read the record. Store no tokens, account identifiers, quota observations, provider bodies or prompts. Persist Muse's attempt guard before its request. For any provider, honor a valid future `Retry-After` when longer than an existing guard, and persist the resulting deadline before releasing the in-flight attempt.

Use wall-clock `notBefore` across restarts. On startup, wait only while `notBefore` is in the future. A missing cooldown record means no saved cooldown. For a corrupt or unreadable record, log a safe warning and atomically replace it with a valid `notBefore = now + 300 seconds` record before waiting. Restarts honor that saved deadline instead of restarting the recovery wait. If replacement fails, keep that provider blocked and report the persistence error. This one-time recovery rule applies to every provider; it is not a normal failure cooldown. If the daemon cannot persist a new guard, it must not make that provider request. This is a request-safety guard, not a defense against the machine owner changing the clock or deleting state.

Keep Claude's five-minute successful-result cache and its 300-second fallback after a 429 with no usable `Retry-After`. Keep Muse's five-minute guard across failed attempts. Only Muse reserves its 300-second guard before every attempt. Claude honors an existing cooldown and persists its actual or fallback 429 delay when received; do not reserve a new five-minute guard before every Claude request. Persist any longer `Retry-After` before releasing the attempt. For other providers, persist an actual `Retry-After` once received, but add no normal fixed failed-attempt cooldown. No request bypasses a known cooldown or future `Retry-After`; simultaneous requests still coalesce. A failure to persist an enforced guard blocks that provider request.

## Credentials

Read local credentials from the official tools' existing stores. Never print or log credential data. Resolve HOME and absolute XDG overrides from the service environment; ignore relative XDG paths. Do not invoke interactive CLIs to log in or refresh credentials.

| Provider | Mac | Linux | Renewal |
|---|---|---|---|
| Claude | Prefer `~/.claude/.credentials.json` when usable; otherwise read Keychain service `Claude Code-credentials` through `/usr/bin/security`, which its ACL trusts. | `~/.claude/.credentials.json` | Automatic when a refresh token is available. Save to the same source. |
| Codex | `~/.codex/auth.json` | `~/.codex/auth.json` | Automatic. Save rotated credentials to the same file. |
| Cursor | Use the installed Cursor CLI's Keychain store, not the desktop IDE database. The inspected CLI uses account `cursor-user` and services `cursor-access-token` and `cursor-refresh-token`. | `~/.config/cursor/auth.json` (`accessToken`) | Read-only. If expired or rejected, report that Cursor CLI login needs attention. |
| OpenCode Go | `$XDG_DATA_HOME/opencode/auth.json`, otherwise `~/.local/share/opencode/auth.json` | Same | Read-only API key. |
| Muse | `~/.config/muse/auth.json` describes storage. The current login points to Keychain service `ai.meta.dev.credentials`, account `meta`. | `$XDG_CONFIG_HOME/muse/auth.json`, otherwise `~/.config/muse/auth.json` | Read-only. Do not refresh OAuth or issue a standing key. |

The Mac Cursor CLI defaults to Keychain; `~/.config/cursor/auth.json` is absent on this Mac. Do not use the desktop Cursor database just because it exists. The CLI file-store alternative is `~/.cursor/auth.json`, but do not change the CLI's store to make the daemon easier to build.

No-UI Keychain access is a release requirement. The current Muse loader sets a no-UI flag, but its tests use a mock and Apple documents limitations for legacy Keychain items. First prove the chosen Rust read API does not show UI using disposable Keychain items, including a locked-keychain test. Do not query actual owner items while no-prompt behavior remains uncertain. After that proof, test actual Muse and Cursor items only with the owner present and explicit opt-in; if access could prompt or the disposable test fails, stop and ask before proceeding. Keep Claude on `/usr/bin/security`; never pass secrets in arguments or logs. Test Claude read and update paths with disposable items and credentials only during the spike. Do not use the real login for Keychain write tests. A failed read must return an actionable error without opening a dialog or retrying in a loop. Normal automatic renewal against the real login belongs to a later implementation/deployment gate, not this planning spike.

For Claude and Codex file refreshes, preserve unknown JSON fields and restrictive file modes. Re-read the file before writing, compare it with the version used for refresh, merge only credential changes and atomically replace it. If the CLI changed the file, reload rather than overwrite its newer data. This mitigates but cannot eliminate races because the official CLI does not share the daemon lock. Never claim the lock prevents those races.

Errors must distinguish missing credentials, expired/rejected login, Keychain access blocked, no active subscription when the provider explicitly says so, cooldown, rate limit and network failure. A 403 alone does not mean that a subscription ended. Include concise remediation for read-only logins, such as using the provider CLI to renew its login.

## Provider behavior to preserve

The endpoints and credential formats below are observed compatibility, not public API promises. Use captured fixtures and mocked HTTP for normal tests; never put real credentials or responses into fixtures.

| Provider | Important behavior |
|---|---|
| Claude | Preserve legacy and model-scoped windows, deduplication, negative remaining for over-100% use, five-minute success caching and actual `Retry-After`. Preserve spend parsing only if needed internally to identify a subscription state; do not expose or migrate spend-history details. |
| Codex | Prefer response-header usage percentages over body fallbacks. Preserve reset forms, optional account ID, positive credit balances without inventing a cap, and expired/reused refresh-token handling. Use correct form encoding. |
| Cursor | Preserve `totalPercentUsed`, bonus capacity, team on-demand and unlimited-plan behavior, and request counts. Validate JWT `sub` before using it in a header. Never refresh or write Cursor credentials. |
| OpenCode Go | Use `https://opencode.ai/zen/go/v1/usage`, not local session cost estimates. Preserve rolling, weekly and monthly used-to-remaining conversion. |
| Muse | Use the CLI's existing OAuth token and standing API key. Do not call the undocumented key-issuance endpoint. Preserve `onboard:false`, redirect refusal, the 256 KiB response cap and its five-minute failed-attempt cooldown. Do not persist quota observations. |

Never log provider response bodies, credentials, tokens, keys, prompts or secret headers. Log provider ID, elapsed time, HTTP status and a safe error code only.

## Local API and settings

Bind only to IPv4 loopback. Keep the existing default port, 8787, unless inspection during implementation shows a current consumer override that must be retained. Check the exact `Host` authority; reject any `Origin`; send no CORS headers. Bound request bodies, connections and concurrency. Do not add Fetch Metadata case-analysis. Read-only quota requests may be unauthenticated on loopback; require a random control token in a header for configuration changes. Store the token in a user-only file, never in a URL or logs.

Use `~/.quotabar/daemon.json` as the daemon-owned configuration. Its only v1 setting is `disabledProviders`, defaulting to an empty list. Missing configuration means defaults. Malformed JSON must stop startup with a clear error rather than silently discard a typo. Unknown provider IDs in the list are rejected with a useful message. Atomic writes preserve the previous file if saving fails.

The daemon is the sole programmatic writer; manual edits to `daemon.json` are supported. Read and validate this small file at each request boundary, including cached-only prompt-hook requests; no file watcher is needed. The `quota enable <provider>` and `quota disable <provider>` CLI operations call `PUT /providers/{id}/enabled` with `{"enabled":true|false}` and the control-token header; they do not edit the file directly. Each operation reloads the file before applying its change, then atomically writes and applies the new list immediately. If the file is malformed, do not probe providers or overwrite it; return a safe configuration error and recover on the next request after the file is corrected. Serialize CLI mutations within the daemon and reload immediately before writing; a simultaneous external manual edit may still win or be overwritten, so document that narrow race rather than adding a file-watching or fingerprint framework. A disabled provider starts no new credential read, quota request or token renewal. On disable, hide its cached reading immediately, prevent queued work from starting, and discard any in-flight result; a request already sent cannot be recalled. The command also supports a plain human-readable summary and `--json` for structured data; do not build a second CLI framework.

Use a non-blocking lock on a file in the state directory at startup. If another daemon holds it, exit with a clear error instead of racing provider requests or state files. Keep one daemon per user per host.

The v1 feed carries `schemaVersion`, generated time, provider IDs and names, tier when known, captured time, wall-clock age, freshness, safe error code, provider status, and quota buckets. A bucket carries its label, remaining percent when applicable, balance remaining/used/cap and unit when applicable, count used/limit when applicable, `resetsAt`, response-time `resetText` and response-time severity. Encode monetary balances as JSON numbers, as existing clients expect; spend-history and exact cost fields are out of scope. Include disabled provider IDs. No raw provider error text or credential metadata goes on the wire.

Update the Pi extension and `~/.agents/bin/quota` to show unknown, stale, busy, unavailable and actionable login states. Keep compact quota output within the Pi footer limit. The shell command must retain its existing `--json` behavior. Preserve both installed Claude Code hook POST routes, their hook configuration and response envelope. The startup hook and CLI share the daemon's formatter; the prompt hook uses only bounded cached severity history.

## Deployment

Build for each host natively. Use a systemd user service on Linux and a GUI-session LaunchAgent on macOS. Install, update and remove them only through explicit user-run scripts. Do not use root services or app-managed supervision. Keep the daemon alive independently from the Mac app.

On Linux, the plan is to enable the user manager at boot and keep it alive after logout using systemd lingering. The implementation must explain this setting and request explicit confirmation before changing it. Use the journal for logs. On macOS, use a LaunchAgent that ends at logout. Send logs to a bounded destination or unified logging; do not create an unbounded file.

Use `~/.local/bin/quotabard` on Linux and `~/Library/Application Support/QuotaBar/bin/quotabard` on macOS. Keep settings and cooldowns under `~/.quotabar`. Replace binaries atomically while the service is stopped. Do not delete config or cooldowns on uninstall. The service must start its listener even when providers are offline; provider errors must not make installation look like a successful quota refresh.

## Phases

### Phase 0: settle macOS credential access and interfaces

- First prove the chosen Cursor and Muse Keychain read APIs are noninteractive with disposable items, including a locked-keychain test. Only after that succeeds, ask the owner to be present and opt in before testing actual items. Stop if the disposable test fails or any path could show UI.
- Verify Claude `/usr/bin/security` access with disposable items and credentials only; do not write the real login during this spike.
- Freeze the feed schema, five-second daemon response ceiling, 4.5-second provider wait, the two-second startup and cached-only prompt hook budgets, control-token route and `disabledProviders` format.
- Extract only high-value provider regression responses into shared JSON fixtures where both Swift and Rust tests can read them. Do not refactor every existing Swift test or require exact parity.
- Record known evidence gaps: provider APIs are unofficial; current OpenCode Python checks returned 403, while the Mac app's native `URLSession` returned 200 in one request. Do not infer invalid credentials from Python's 403.

Gate: no provider APIs are called. Do not proceed with Mac implementation if a real Keychain read can prompt.

### Phase 1: prove one provider and the Linux service

- Create the Rust crate, listener, lock, config loader, cooldown persistence, in-memory cache, bounded on-demand scheduler and feed.
- Start with OpenCode Go and a fixture credential in tests. Validate response parsing and the local endpoint offline.
- Install the one-provider binary as a user systemd service on Linux early. Confirm listener and journal behavior, startup at boot, availability after logout only after the separately approved lingering change, and no provider requests at service startup.
- Verify the native Rust HTTP client against OpenCode Go only after obtaining explicit approval for one bounded request. The Python 403 and native Swift 200 samples show why the actual Rust client must be tested.

Gate: Rust format, clippy, tests and Linux release build pass. Loopback tests cover exact Host, Origin rejection, control-token protection, request bounds, single-instance lock and a total response deadline. Service install/update/removal work without root. No provider requests are part of automated tests.

### Phase 2: add the remaining providers and clients

- Add Claude, Codex, Cursor and Muse with offline parser fixtures and mocked HTTP.
- Add Claude/Codex renewal with safe conditional file updates and no-UI Claude Keychain handling. Add Cursor CLI credential adapters for its Linux file and Mac Keychain stores. Add Muse file and Mac Keychain adapters without renewal or saved observations.
- Test five-minute success reuse, Claude and Muse cooldowns, `Retry-After` persistence across restart, concurrent request coalescing, caller timeout without cancellation, and unknown state after restart.
- Update the Pi extension, shell `quota` command and both Claude Code hooks. Add `enable` and `disable` commands backed by the daemon control route. Confirm no provider is called while idle, UserPromptSubmit is cache-only, and no disabled provider starts new credential work.

Gate: all provider parser and mocked HTTP tests pass on Linux and Mac. Credential tests use temporary home directories and mocks. Actual owner Keychain items are tested only after the disposable-item proof and with the owner's explicit, present opt-in; otherwise Mac acceptance remains blocked. Existing Swift tests still run through the documented `xcodebuild test` command only. Client tests verify age, severity, balances, counts, resets and actionable errors.

### Phase 3: Mac service, cutover and remove the Swift engine

- Build and install the macOS LaunchAgent. Confirm it remains independent of QuotaBar.app and reads credentials as the intended user.
- Switch clients and both Claude Code hooks to the daemon. Stop the Swift listener and probes before starting the daemon. Do not run both engines at once on a host.
- Observe normal use, errors, restart behavior, provider cooldowns and the five-second limit before deleting the Swift provider engine.
- Remove the old Swift probes, credential writers, monitor and feed server only when no path needs them. Keep the app bundle only if needed for unrelated functions; current agreed scope does not require a new Mac UI.

Gate: accepted provider behavior on both hosts; Keychain prompt-free requirement passes; client and hook checks pass; Linux and Mac release builds pass; install, restart, rollback and uninstall procedures are tested. Rollback stops the daemon before restoring any previous Swift engine. Linux has no previous local producer, so rollback there means stop the service and report the feed unavailable.

## Verification principles

- Keep provider tests offline with fixtures and mocked HTTP. Never run real credentials in tests.
- Use temporary HOME and XDG directories for credential and configuration tests.
- Test status, age and reset formatting at different wall-clock times without making new provider calls.
- Test stale readings after timeout and failure, and unknown state after process restart.
- Test persisted `notBefore`, actual longer `Retry-After`, the one-time five-minute recovery wait for a corrupt provider record, and failure to persist an enforced guard.
- Test that providers run concurrently, duplicate calls coalesce, request timeout does not cancel shared work, and the Pi footer/tool use the same GET path with an eight-second transport timeout.
- Test both Claude hooks: SessionStart starts shared on-demand work but returns within two seconds; UserPromptSubmit never probes and only reports worsening bucket severity. Test bounded session expiry/capacity and malformed IDs without retaining prompts.
- Test configuration reload on each request, invalid manual JSON failing closed without overwriting the file, and disable hiding cached data and discarding in-flight results.
- Prove Keychain UI suppression first with disposable items. Test actual owner items only after that proof and with explicit owner-present opt-in; a mock asserting a no-UI flag is not sufficient.
- Test loopback security and both clients against the versioned feed. Do not overbuild Fetch Metadata rules.
- Run Rust checks on each native target. Run Swift tests only with the documented `xcodebuild test` command.
- Request explicit approval for any live provider test. Automated gates must not call provider APIs.

## Evidence and limits

The only provider latency samples are single checks, not averages or tail-latency measurements. On Linux, successful Claude, Codex, Cursor and Muse calls took about 0.41, 0.63, 0.24 and 0.50 seconds. OpenCode Go returned 403 in the Python check. On Mac, Codex and Cursor took about 0.56 and 0.27 seconds; Claude was skipped because of its recent-call guard, and Muse was skipped to avoid a possible Keychain prompt. A native Mac `URLSession` OpenCode request returned 200 in about 0.30 seconds. No measurements covered token renewal or slow network failures.

Mac Cursor CLI storage was inspected without reading its Keychain items. Its installed runtime selects Keychain by default; the desktop database is not the selected daemon source. Muse's actual Keychain item may require a compatible access identity; prompt-free daemon access is not proven. Provider endpoints remain unofficial or undocumented, and their rate-limit scope across the two hosts is unknown.

The Linux host is Ubuntu 26.10 x86_64 with Rust 1.99 and a systemd user manager. The Mac is arm64 with Rust 1.98.1. No Rust implementation, build, service installation or deployment has been performed. Linux lingering has owner approval in principle, but no machine setting has been changed. Claude/Codex renewal is in scope with documented residual credential-write races; this plan does not claim those races can be eliminated.
