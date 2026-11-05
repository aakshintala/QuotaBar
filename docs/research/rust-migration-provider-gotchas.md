# Rust migration provider gotchas

## Findings first

This note records source observations and possible safeguards. The [migration roadmap](../plans/rust-daemon-migration.md) selects the proposed implementation defaults; its polling and persistence choices supersede tuning suggestions here.

The five probes depend on CLI-owned credentials and usage APIs whose public stability this review did not verify. Keep the observed request shapes and parser edge cases, but treat them as compatibility behavior, not provider promises. This review used the repository at `e12da35`; it read no secret files and made no provider requests. No first-party provider documentation was checked, so this document does not claim provider guarantees.

The owner chose independent per-host daemons; that design does not require a shared poller or cross-host lock. Keep request guards local to each daemon, use a separate native CLI login on each host, and do not copy refresh-token files between hosts. Claude and Codex can race with the local CLI when credentials rotate, while the request limit's account/token/IP scope is unknown. A shared poller or lock is an optional fallback only if measurements show sustained throttling.

Use local provider cooldowns even for forced refreshes. A force request may bypass snapshot freshness, but never a local cooldown or an active `Retry-After`. As proposals, retain Claude's five-minute successful-snapshot cache and Muse's five-minute persisted cooldown, persist retry windows locally, and start with a 15-minute per-host minimum for providers without an observed guard. These are tuning defaults, not known safe provider limits; two independent hosts can still make requests close together. Repository history records one observed one-hour Claude `Retry-After` after a single call following a quiet period (`a2ae66c`). That observation means wait for a returned one-hour retry window when received; it does not establish a normal 60-minute polling cadence.

## Shared quota and failure rules

The daemon should retain these semantics unless the SwiftUI client takes responsibility for them:

- `percentRemaining` is the remaining percentage, not the used percentage. `UsageQuota` caps it at 100 but permits negative values to represent over-quota usage. A missing percentage means a balance-only meter, not zero. See [`UsageQuota`](../../Sources/Domain/Provider/UsageQuota.swift) and the regression in commit `41ca4e1`.
- `UsageQuota.status` is pace-aware when `resetsAt` exists. Its elapsed-time estimate uses fixed type durations: session 5 hours, weekly and model-specific 7 days, monthly 30 days, and other `timeLimit` types 7 days. Low remaining quota can still be healthy if the observed burn rate can last to reset; depleted is always depleted. Without a reset, it falls back to absolute thresholds. See [`QuotaStatus`](../../Sources/Domain/Provider/QuotaStatus.swift), [`QuotaType`](../../Sources/Domain/Provider/QuotaType.swift), and `QuotaStatusTests` / `UsageQuotaTests`.
- Balance-only buckets are healthy and do not affect overall status. Codex credits have no invented cap. Count-based meters carry `unitsUsed` and `unitsLimit`. Claude extra spend is a separate `CostUsage`, not a quota percentage. See [`UsageSnapshot`](../../Sources/Domain/Provider/UsageSnapshot.swift) and [`CostUsage`](../../Sources/Domain/Provider/CostUsage.swift).
- `NetworkClient.send` accepts HTTP 200 only. It maps 401/403 to authentication-required unless a provider maps the status first, and maps other statuses and transport errors to execution failures. See [`ProbeHelpers.swift`](../../Sources/Infrastructure/Shared/ProbeHelpers.swift).
- `AIProvider.refresh()` keeps its last snapshot on ordinary errors, replaces it for an explicit stale-snapshot error, and clears it on account-changed or no-current-quota. `QuotaMonitor` skips unavailable providers without updating their state. Thus a removed credential can leave an old snapshot visible, while Muse deliberately surfaces credential errors by always reporting itself available. See [`AIProvider`](../../Sources/Domain/Provider/AIProvider.swift), [`QuotaMonitor`](../../Sources/Domain/Monitor/QuotaMonitor.swift), and [`MuseUsageProbe.isAvailable`](../../Sources/Infrastructure/Muse/MuseUsageProbe.swift).

## Multi-host refresh and credential hazards

The repository does not establish whether any provider limits requests per token, account, IP, or some combination. Do not infer account-wide or per-token guarantees from local caches or response fields.

- Claude's history records a failure mode where Claude Code refreshed a token externally, leaving the app's cached refresh token invalid; the fix reloads credentials and retries when the file changed (`01e710a`). This is evidence of an observed race, not a formal refresh-token guarantee. The current probe has no single-flight refresh lock, and its in-memory cache and rate-limit state are process-local.
- Codex refreshes after `last_refresh` is older than eight days or absent. It recognizes `refresh_token_reused` as session-expired, but has no refresh lock, cache, or failed-refresh reload from the auth file. Two processes with the same stale credential can both attempt refresh. Whether one rotation invalidates another host's token is unknown.
- Muse persists a five-minute cooldown under the user's local `~/.quotabar/muse` directory and locks only that filesystem path. Independent Mac and Linux home directories do not share the lock or timestamp. Its cache key is a hash of the exact OAuth access token, not a stable account ID; token rotation can look like an account change.
- Cursor reads a local Cursor database and caches its token in process memory. The configured path is macOS-specific; the source has no Linux database-path resolution.
- OpenCode Go has no local request cooldown. The key may be present on both hosts, but the key's rate scope and the service's limits are unknown.

Independent local guards are the selected design, not a claim that each host has an account-wide allowance. Use a separate official CLI login on each host where supported; do not copy a refresh-token file. Even separate tokens may share an account-level quota. Never let a UI force action skip its host's provider cooldown. Claude's current code caches successful snapshots for five minutes and handles 429 with `Retry-After`; the history reports one observed one-hour retry window and `Retry-After: 0` while 429 continued (`a2ae66c`, `23b5b7c`, `103a1f0`, `d8e4e98`). Honor an actual future `Retry-After` and persist it locally. The zero-header observation supports the existing five-minute fallback, not a 60-minute normal cadence. Rate scope and safe polling frequency remain unknown. If sustained 429s persist despite honoring local retry windows, a shared poller or lock can be evaluated as an optional mitigation.

## Claude

### Request and credential behavior

- Usage: `GET https://api.anthropic.com/api/oauth/usage`. The observed headers are `Authorization: Bearer …`, `Accept: application/json`, `Content-Type: application/json`, `anthropic-beta: oauth-2025-04-20`, and `User-Agent: QuotaBar`. The bearer value is trimmed. Timeout is 15 seconds.
- Refresh: `POST https://platform.claude.com/v1/oauth/token` with JSON `grant_type`, `refresh_token`, a fixed public client ID, and scopes `user:profile user:inference user:sessions:claude_code`. It updates access token, optional rotated refresh token, and expiry, then writes credentials back to their original source. Do not copy the client ID as a secret.
- Resolution order is `~/.claude/.credentials.json`, macOS Keychain service `Claude Code-credentials`, then `CLAUDE_CODE_OAUTH_TOKEN`. File and Keychain records use `claudeAiOauth`. The environment token has no expiry or refresh token; it is read-only and the probe tries it directly rather than refreshing it. A missing expiry otherwise counts as refresh-needed; the loader uses a five-minute expiry buffer.
- The Keychain path intentionally shells through `/usr/bin/security`, not `SecItem*`, because the source says Claude Code's item trusts that executable. Reads use `security find-generic-password`; writes use `security -i`, `-U`, and hex-encoded JSON over stdin, not the secret in argv. `50061ea` records the earlier argv exposure and delete-then-add gap that this replaced. This route is macOS-only.

Sources: [`ClaudeAPIUsageProbe`](../../Sources/Infrastructure/Claude/ClaudeAPIUsageProbe.swift), [`ClaudeCredentialLoader`](../../Sources/Infrastructure/Claude/ClaudeCredentialLoader.swift), `ClaudeAPIUsageProbeTests`, `ClaudeCredentialLoaderTests`, commits `50061ea`, `01e710a`.

### Cache, data, and failure behavior

- Successful usage snapshots are cached in memory for five minutes, including on forced refresh. HTTP 429 records an in-memory retry time from numeric or HTTP-date `Retry-After`; missing, malformed, past, or zero values use a five-minute fallback. The implementation does not persist either guard across process restarts.
- Legacy windows are `five_hour`, `seven_day`, `seven_day_sonnet`, and `seven_day_opus`; `utilization` is percent used and becomes `100 - utilization`. The newer `limits` array contributes only `weekly_scoped` entries. Model identity uses the first word of `scope.model.display_name`; duplicates already represented by legacy fields are skipped. Do not add unscoped `session` or `weekly_all` entries from `limits`, or you double-count legacy fields.
- Spend prefers `spend` over legacy `extra_usage`. Both require enabled data and nonnegative amounts. `spend` scales `amount_minor` by each amount's `exponent`; legacy values use `decimal_places`, defaulting to 2. An absent/null cap means uncapped. A present but invalid cap invalidates that shape and permits fallback to legacy data.
- Subscription tier comes from the credential record, not the usage response. Missing windows are omitted. Over-100% utilization remains negative remaining, which the domain marks depleted.
- A 401 or 403 triggers one refresh if a refresh token exists; failure clears the in-memory credential cache. A failed proactive refresh reloads credentials to detect a CLI login that raced it. A 429 throws rate-limited. Other failures leave the provider's previous snapshot intact through `AIProvider` but attach an error; there is no explicit stale-snapshot error from this probe.

### Tests to carry and safe simplifications

Carry tests for legacy and scoped model windows, duplicate suppression, malformed scoped entries, negative remaining, fractional resets, spend precedence and fallback, currency exponents, invalid/uncapped caps, `Retry-After` zero/date parsing, forced-refresh cache behavior, token expiry, rotated refresh token persistence, 401 recovery, and externally replaced credentials. These live in `ClaudeAPIUsageProbeTests` and `ClaudeCredentialLoaderTests`.

The daemon can omit Claude-specific display labels and account-tier badges if the client does not need them. Do not omit the distinct spend semantics if cost is exposed. Keep only known response fields; no CLI invocation is needed.

### Do not copy these credential-write risks

The loader preserves unknown top-level JSON fields, but replaces the entire `claudeAiOauth` object with only known fields during refresh. A future CLI field nested there can be lost. The interpolated account name in the `security -i` command is quoted but not escaped; use a safe argument/command encoder if reproducing this route. File writes use atomic replacement but do not explicitly preserve or set file mode. Tests cover values and stdin encoding, not unknown nested fields, concurrent writers, or permissions. A daemon advisory lock can serialize cooperating daemon writers, but the official CLI does not use it unless it explicitly shares that protocol. Re-read and merge the latest JSON, compare it with the version read before refresh, and reload/retry if it changed; this mitigates but cannot eliminate a CLI race. Preserve unknown fields and restrictive mode, and never log credentials.

## Codex

### Request and credential behavior

- Usage: `GET https://chatgpt.com/backend-api/wham/usage`. Headers are `Authorization: Bearer …`, `Accept: application/json`, `User-Agent: OpenUsage`, and optional `ChatGPT-Account-Id` from the credential record. Timeout is 15 seconds.
- Refresh: `POST https://auth.openai.com/oauth/token`, form-urlencoded fields `grant_type`, fixed public `client_id`, and `refresh_token`. It stores returned access token, optional refresh token and ID token, updates `last_refresh`, and atomically writes the auth file.
- Resolution is only `~/.codex/auth.json`, reading `tokens.access_token`, `tokens.refresh_token`, `tokens.account_id`, and root `last_refresh`. `OPENAI_API_KEY` authentication is intentionally unsupported for this usage endpoint.
- The source refreshes proactively when `last_refresh` is absent/unparseable or older than eight days. It does not parse access-token expiry. A non-session proactive-refresh error falls through and tries the existing token; a recognized expired/reused/invalidated refresh token stops with session-expired. A 401 triggers one refresh and retry. There is no usage cache, 429-specific backoff, or process-level refresh serialization.

Sources: [`CodexAPIUsageProbe`](../../Sources/Infrastructure/Codex/CodexAPIUsageProbe.swift), [`CodexCredentialLoader`](../../Sources/Infrastructure/Codex/CodexCredentialLoader.swift), `CodexAPIUsageProbeTests`, `CodexCredentialLoaderTests`, commit `35f1a87` (removes the interactive RPC/TTY stack).

### Data, failure behavior, and tests

The API response headers `x-codex-primary-used-percent` and `x-codex-secondary-used-percent` take precedence over body `rate_limit.primary_window.used_percent` and `secondary_window.used_percent`. The body supplies `reset_at` (epoch seconds) or `reset_after_seconds`. Credits come from `x-codex-credits-balance` or body `credits.balance` (string or number). A positive balance is shown only if `has_credits` is not false; there is no synthetic cap, and zero/no-credit accounts get no bucket. `plan_type` becomes an uppercase custom tier. These are observed parser rules, not provider guarantees.

The domain still estimates elapsed time from fixed five-hour and seven-day durations. If Codex returns a session reset several days away, elapsed time clamps to zero and status falls back to absolute thresholds; `UsageQuotaTests` covers this case. Do not treat that pace estimate as a provider-reported window duration.

Carry tests for header precedence and body fallback, both reset forms, numeric/string credit balances, suppression of zero/no-credit buckets, empty usage, 401, malformed JSON, refresh success/rotation, missing/old `last_refresh`, and recognized refresh-token errors. Add missing tests for request headers, safe form encoding, concurrent refresh, preserving unknown JSON fields, and file modes. Existing tests are in `CodexAPIUsageProbeTests` and `CodexCredentialLoaderTests`.

### Do not copy these refresh/write risks

The refresh body uses `addingPercentEncoding(withAllowedCharacters: .urlQueryAllowed)` to encode form values. That character set is not a form-component encoder and can leave delimiters such as `&` or `+` ambiguous; use a proper `application/x-www-form-urlencoded` encoder. Refresh failures do not reload the auth file, so a concurrent CLI refresh can leave this probe reporting session-expired despite newer local credentials. Unknown fields inside `tokens` are preserved, as are other top-level fields, but the source does not serialize writers or explicitly preserve file permissions. A daemon advisory lock only coordinates daemon processes that honor it; the CLI does not participate unless it shares the same lock protocol. Compare the on-disk file with the version read before refresh, then reload, merge, and retry if it changed. This reduces but does not eliminate CLI races. Test unknown fields, concurrent updates, and mode preservation.

## Cursor

### Request and credential behavior

- Usage: `GET https://cursor.com/api/usage-summary`, authenticated by the `Cookie` header `WorkosCursorSessionToken=<JWT sub>::<JWT>`. The source sets `Content-Type: application/json` and a 15-second timeout. No explicit `Accept` or user-agent is set.
- The token is read with `/usr/bin/sqlite3` from `ItemTable`, key `cursorAuth/accessToken`, at `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`. It decodes the JWT payload for `sub` and optional `exp`; it does not verify the JWT signature locally. The server remains responsible for accepting/rejecting the cookie.
- The token and `sub` are cached in process memory only until JWT `exp`; without `exp`, the DB is read each time. A 401 drops the cache, rereads once, and retries. There is no token refresh, usage snapshot cache, or 429 cooldown. The hard-coded DB path is macOS-specific; Linux support needs an independently verified path and DB schema.

Sources: [`CursorUsageProbe`](../../Sources/Infrastructure/Cursor/CursorUsageProbe.swift), `CursorUsageProbeParsingTests`, `CursorUsageProbeCachingTests`, commits `c686421` and `e1e73d6`.

### Data, failure behavior, and tests

The parser reads `individualUsage.plan` and `individualUsage.onDemand`; it reads `teamUsage.onDemand` only when `limitType == "team"`. It ignores disabled meters and requires positive capacity. Capacity is `max(limit, breakdown.total)`, because the included limit can be maxed while bonus capacity remains. `totalPercentUsed` is authoritative when present; otherwise it derives percentage from `used / limit`. It clamps remaining to zero, reports rounded effective `unitsUsed` plus `unitsLimit`, and assigns the billing-cycle end to each meter. `isUnlimited` adds a 100%-remaining monthly bucket.

A 401 is mapped to session-expired and retried once; 403 and other non-200 statuses use the shared helper's generic error mapping. Missing/invalid JSON or no meters is a parse failure. No last-known snapshot is persisted by this probe.

Carry tests for the real usage-summary shape, bonus-credit capacity, enterprise `breakdown.total`, team on-demand, unlimited plans, disabled meters, both ISO timestamp forms, integer/double counts, JWT `sub`/`exp`, expired token reread, and one 401 retry. Add a regression where a token's `sub` contains invalid header characters and verify safe header construction; never trust a decoded claim as an unvalidated header fragment.

A status pitfall: `On-Demand` and `Team` are `timeLimit` types with the default seven-day duration, but receive the billing-cycle end. `UsageQuota.percentTimeElapsed` therefore estimates their pace using seven days, not the billing cycle. Preserve reset timestamps, but either model the true duration or avoid pace-aware status for these meter types.

The intentional simplification is to report Cursor's server usage summary rather than reconstructing local usage. Keep the `totalPercentUsed`/bonus distinction; commit `e1e73d6` records a real false-depleted regression when the parser used only the base `used / limit` values.

## OpenCode Go

### Request and credential behavior

- Usage: `GET https://opencode.ai/zen/go/v1/usage`, with `Authorization: Bearer …` and `Accept: application/json`; timeout is 15 seconds.
- The key is read, without writing or refreshing, from `opencode-go.key` in `$XDG_DATA_HOME/opencode/auth.json`, or `~/.local/share/opencode/auth.json` when the variable is unset or empty. No token cache, usage cache, or rate cooldown is implemented.
- The source accepts a non-empty key but does not validate `type == "api"`. It also accepts a relative `XDG_DATA_HOME`; on Linux, consider rejecting a non-absolute value and falling back per the XDG path rule.

Sources: [`OpenCodeUsageProbe`](../../Sources/Infrastructure/OpenCode/OpenCodeUsageProbe.swift), [`OpenCodeCredentialLoader`](../../Sources/Infrastructure/OpenCode/OpenCodeCredentialLoader.swift), `OpenCodeUsageProbeTests`, commit `a69ef0e`.

### Data, failure behavior, and tests

The response must contain `usage.rolling`, `usage.weekly`, and `usage.monthly`, each with `percent` and optional `resetsAt`. Percent means used; the parser clamps it to 0–100 and flips it. Windows map to session, weekly, and monthly. It does not report dollar caps or balances, even though source comments describe nominal quota amounts. Reset timestamps may be absent. No account tier is emitted.

The shared helper maps 401/403 to authentication-required and other failures to execution-failed. The decoder requires all three windows and all three percent values, so a partial response fails instead of returning partial quotas. There is no stale cache or special 429 handling.

Carry tests for all three windows, zero usage, over-limit clamping, fractional/non-fractional reset timestamps, missing credentials, 401/500, malformed/partial JSON, and both XDG and fallback paths. Add coverage for relative XDG values and for whether a future API response may omit one window.

The intentional simplification is important: query the server quota API rather than summing local session `cost` estimates. Commit `a69ef0e` documents that the local estimate undercounted server deductions and that OpenCode 2.x removed the earlier `db` CLI path. Do not reintroduce either approximation or interactive CLI driving.

## Muse

### Request and credential behavior

- Quota: `POST https://api.meta.ai/muse-code/key`, with `Authorization: Bearer …`, `x-api-version: 1.0.0`, JSON content/accept headers, and body `{"onboard":false}`. Timeout is 15 seconds. The bearer is the OAuth access token; the CLI's standing `api_key` is required to consider credentials complete but is not sent in this request.
- Credential path is `MUSE_AUTH_PATH` if set, otherwise absolute `$XDG_CONFIG_HOME/muse/auth.json`, otherwise `~/.config/muse/auth.json`. It requires `providers.meta.mechanism == "oauth"` and a `dca:` access token. If metadata says `storage == "keychain"`, it reads service `ai.meta.dev.credentials`, account `meta`, with UI prompts disabled. Otherwise it reads token data from the file. A non-empty standing `api_key` must also exist. It does not refresh or modify credentials and deliberately does not call the undocumented key-issuance endpoint.
- The request client refuses redirects and caps response data at 256 KiB. The parser has the same size cap. This is a useful token-exfiltration and resource-boundary guard to preserve.

Sources: [`MuseUsageProbe`](../../Sources/Infrastructure/Muse/MuseUsageProbe.swift), [`MuseCredentialLoader`](../../Sources/Infrastructure/Muse/MuseCredentialLoader.swift), [`MuseURLSessionNetworkClient`](../../Sources/Infrastructure/Muse/MuseURLSessionNetworkClient.swift), `MuseUsageProbeTests`, `MuseCredentialLoaderTests`, `MuseURLSessionNetworkClientTests`, commit `e12da35`.

### Cooldown, stale behavior, and tests

Muse reserves and persists a five-minute cooldown before each request, under a file lock held across the request. A failure consumes the cooldown. Per-token state is under `~/.quotabar/muse`, with a SHA-256 access-token fingerprint in filenames; cache contents hold quota percentages and resets, not bearer tokens or returned account data. A successful response clears the stale marker. During cooldown after failure, the probe reports an explicit stale snapshot if any unexpired windows remain; expired windows are filtered. Without an unexpired observation it reports no-current-quota. A changed fingerprint throws account-changed once and clears the old account snapshot through `AIProvider`.

The parser reads active subscription state, tier, session/weekly `used_percent`, and epoch-second `resets_at`. It clamps used percentage to 0–100, omits absent windows, and omits windows whose reset is in the past. It does not infer 100% remaining for missing windows and ignores returned secrets, email, and other account fields. The response shape and cooldown remain implementation observations, not Meta service guarantees.

Carry tests for missing/invalid credentials, missing standing key, file and noninteractive Keychain resolution, subscription inactive, absent/expired windows, percentage clamping, omitted account data, redirect refusal, body-size limit, exact request shape, persisted cooldown after success and failure, stale filtering, concurrent probes sharing a lock, interrupted lock recovery, and account changes. Existing tests cover these in the Muse suites.

The intentional simplification is read-only use of the CLI's existing login, with no OAuth exchange, no key issuance, and no use of response account fields. Preserve `isAvailable() == true` so missing/denied credentials appear as actionable errors instead of being silently skipped by `QuotaMonitor`.

## Cross-cutting issues not to reproduce

- Claude and Codex log up to 500 characters of usage response bodies at debug level; both also log refresh error bodies. `CategoryLogger` marks messages public. These bodies may contain account or usage information. Do not log response bodies, headers, tokens, or keys; log status and a safe provider error code instead. See `ClaudeAPIUsageProbe.fetchUsage` / `refreshToken`, `CodexAPIUsageProbe.parseUsageResponse` / `refreshToken`, and [`AppLogger.swift`](../../Sources/Infrastructure/Logging/AppLogger.swift).
- Claude and Codex refresh writes use atomic replacement but do not explicitly preserve file mode. The source tests do not assert permissions. Claude drops unrecognized keys inside `claudeAiOauth`; Codex preserves unknown keys inside `tokens`, but either can overwrite a concurrent CLI update from a stale in-memory copy. A daemon advisory lock serializes only cooperating daemon processes; the official CLI cannot be serialized by that lock unless it uses the same protocol. Compare the current file with the version read before refresh, then conditionally reload and merge if it changed. This mitigates, but does not eliminate, CLI races. Preserve unknown data and restrictive permissions, and test these behaviors on macOS and Linux.
- Claude's `security -i` command includes a quoted but unescaped account string. Do not interpolate variable values into a command language. Use a safe encoder and keep the secret off argv; preserve the tested stdin approach because the Claude Code Keychain ACL is tool-specific.
- `NetworkClient.send` accepts only status 200, not arbitrary 2xx. Confirm whether the Rust client should preserve this observed rule or intentionally broaden it; do not let non-success bodies be parsed as usage.
- `QuotaMonitor.minimumSnapshotAge` is 60 seconds and its in-flight join is per process. These protect one daemon, not two. Claude's five-minute snapshot cache and Muse's five-minute persisted per-token cooldown are local guards; independent hosts maintain separate state by design. See commit `a2ae66c` for the one-refresh path and Claude cache decision.

## Reference tests and history

- Domain rules: `Tests/DomainTests/Provider/QuotaStatusTests.swift`, `UsageQuotaTests.swift`, `UsageSnapshotTests.swift`, and `Tests/DomainTests/Monitor/QuotaMonitorTests.swift`.
- Provider parsers and credential behavior: `Tests/InfrastructureTests/{Claude,Codex,Cursor,OpenCode,Muse}/`.
- Relevant changes: `01e710a` Claude credential-cache recovery after CLI refresh; `50061ea` safe Claude Keychain update; `23b5b7c` Claude 429 backoff; `103a1f0` reject misleading zero `Retry-After`; `d8e4e98` Claude successful-snapshot TTL; `a2ae66c` shared refresh path and five-minute Claude guard; `c686421` Cursor token cache; `e1e73d6` Cursor bonus-credit fix; `35f1a87` Codex API-only simplification; `a69ef0e` OpenCode server-accounting API; `41ca4e1` balance-only and count-meter semantics; `e12da35` Muse cooldown and safe credential loading.
