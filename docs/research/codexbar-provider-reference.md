# CodexBar provider reference for the Go daemon

## Findings first

This note records how CodexBar (steipete/CodexBar v0.83.0, MIT, checked out read-only at `~/work/ref/CodexBar`) talks to the five providers QuotaBar supports: `claude`, `codex`, `cursor`, `opencode-go` and `muse`. Use it as the reference for a small Go daemon. Nothing in CodexBar was built or run, no provider requests were made and no secret values were read or recorded.

The main lessons:

1. CodexBar does not refresh OAuth tokens that a CLI owns. For Claude it asks the `claude` CLI to refresh (it runs `/status` in a PTY), then rereads the credentials. For Codex the usage path never redeems the refresh token; it rereads `auth.json` and falls back to `codex app-server`. Its source says redeeming a shared refresh token without publishing the result strands the owner. QuotaBar does the opposite: it refreshes both tokens itself and writes them back.
2. CodexBar never writes to Claude Code's credentials file or Keychain item. A token it refreshes itself goes only into its own Keychain cache (`com.steipete.codexbar.cache`). Codex `auth.json` writeback exists, but the usage path does not call it.
3. Reading Claude Code's Keychain item is opt-in and off by default, because Claude Code rewrites the item on every refresh, which resets its ACL and causes repeat prompts (CodexBar issues 2380 and 2634). Every Keychain read CodexBar does first runs a no-UI preflight: `LAContext.interactionNotAllowed` plus `kSecUseAuthenticationUIFail`, and an ACL check before it asks for the data. After a denied prompt it waits 6 hours before trying again.
4. For a daemon, the safest macOS route is `/usr/bin/security find-generic-password -s <service> -a <account> -w` with a timeout. This is what QuotaBar already does for Claude. Read only; do not write Claude's item.
5. Cursor, OpenCode Go and Muse are read-only in both apps. No refresh, no writeback.
6. Muse: CodexBar and QuotaBar call the same endpoint, `POST https://api.meta.ai/muse-code/key`, with the `dca:` token. CodexBar sends `{}`; QuotaBar sends `{"onboard":false}`.

All paths below are relative to `~/work/ref/CodexBar/Sources/CodexBarCore/` unless stated. `P/` means `Providers/`.

## Claude

### Endpoints

| Use | Request | Headers | Source |
|---|---|---|---|
| Usage | `GET https://api.anthropic.com/api/oauth/usage?cedar_ember=1` | `Authorization: Bearer <access token>`, `Accept` and `Content-Type: application/json`, `anthropic-beta: oauth-2025-04-20`, `User-Agent: claude-cli/<version> (external, cli)` (version from the installed `claude`, fallback `2.1.0`) | `P/Claude/ClaudeOAuth/ClaudeOAuthUsageFetcher.swift:61-131` |
| Usage retry | On 400, or 403 whose body lacks `user:profile`: retry without the query parameter and with `User-Agent: claude-code/<version>` | as above | same file `:99-106` |
| Profile | `GET https://api.anthropic.com/api/oauth/profile` | Bearer | same file `:143-170` |
| Refresh | `POST https://platform.claude.com/v1/oauth/token`, form-urlencoded `grant_type=refresh_token`, `refresh_token`, `client_id` | `Content-Type: application/x-www-form-urlencoded` | `P/Claude/ClaudeOAuth/ClaudeOAuthCredentials.swift:1361-1447` |

The client ID is Claude Code's public one; `CODEXBAR_CLAUDE_OAUTH_CLIENT_ID` overrides it (`ClaudeOAuthCredentials.swift:23-34`). It is not a secret. Requests use an ephemeral session with no URL cache, 30 s request and 90 s resource timeouts (`ClaudeOAuthUsageFetcher.swift:134-141`).

### Credential sources, in order

`ClaudeOAuthCredentials.swift:336-492` (`loadRecord`):

| Order | Source | Notes |
|---|---|---|
| 1 | Env `CODEXBAR_CLAUDE_OAUTH_TOKEN` (and `_SCOPES`, default `user:profile`) | No refresh. `:2476-2504` |
| 2 | In-memory cache | |
| 3 | CodexBar's own Keychain item, service `com.steipete.codexbar.cache` | `KeychainCacheStore.swift:42` |
| 4 | `$CLAUDE_CONFIG_DIR/.credentials.json`, else `~/.claude/.credentials.json` | `P/Claude/ClaudeConfigPaths.swift:5,87-118` |
| 5 | No-prompt sync from Claude Code's Keychain item `Claude Code-credentials` | only with consent |
| 6 | Prompted read of the same item | only if the prompt mode allows |
| 7 | Expired record, as a last resort | |

File shape: `claudeAiOauth{accessToken, refreshToken, expiresAt (ms), scopes, rateLimitTier, subscriptionType}`. A file holding only `mcpOAuth` is detected and treated as not signed in (`ClaudeOAuthCredentialModels.swift:94-113`). Linux uses the file only.

### macOS Keychain without prompts

| Mechanism | What it does | Source |
|---|---|---|
| Consent gate | Reading Claude Code's item needs explicit opt-in; default off | `ClaudeOAuthDirectKeychainReadConsent.swift:3-17`, `ClaudeOAuthCredentials.swift:2924-2930` |
| Prompt mode | `never`, `onlyOnUserAction`, `always` | `ClaudeOAuthKeychainPromptMode.swift:18-20` |
| Read strategy | `securityFramework` (SecItem) or `securityCLIExperimental` | `ClaudeOAuthKeychainReadStrategy.swift:4-5` |
| `security` CLI | `/usr/bin/security find-generic-password -s <service> [-a <account>] -w`, with a timeout and process-group kill | `ClaudeOAuthCredentials+SecurityCLIReader.swift:13,195-276` |
| No-UI SecItem | `LAContext` with `interactionNotAllowed = true`, plus `kSecUseAuthenticationUI = kSecUseAuthenticationUIFail` (resolved with `dlsym`) | `KeychainNoUIQuery.swift:11-19` |
| Preflight | Query attributes and a reference first, inspect the decrypt ACL, and ask for data only if it would not prompt | `KeychainCacheStore.swift:120-140`, `KeychainAccessPreflight.swift:293-360` |
| Denied cooldown | 6 hours after a denied prompt | `ClaudeOAuthKeychainAccessGate.swift:15` |

Why it matters: Claude Code rewrites `Claude Code-credentials` on each token refresh, which replaces the ACL. An app that was once allowed is then prompted again.

### Refresh and writeback

| Question | CodexBar behaviour | Source |
|---|---|---|
| Who refreshes | Directly only if CodexBar owns the credential. If the file, the Keychain or Claude's config shows the CLI owns it (signed in or indeterminate), it throws `refreshDelegatedToClaudeCLI` | `ClaudeOAuthCredentials.swift:776-818,1605-1627` |
| Delegated refresh | Runs `claude` in a PTY with `/status`, then rereads the Keychain without a prompt. Cooldown 5 min (20 s in some paths), timeout 8 s | `P/Claude/ClaudeStatusProbe.swift:356-381`, `ClaudeOAuthDelegatedRefreshCoordinator.swift:46-56` |
| Response | `access_token`, optional `refresh_token` (keep the old one if absent), `expires_in` | `ClaudeOAuthCredentials.swift:1361-1447` |
| Writeback | Only to CodexBar's own Keychain cache. Never to Claude's file or Keychain item | `ClaudeOAuthCredentials.swift:1657-1690` |
| Failure gate | 400 or 401 `invalid_grant` is terminal until the auth input changes. Other failures back off exponentially from 5 min to 6 h | `ClaudeOAuthRefreshFailureGate.swift:58-59,500-501`, `ClaudeOAuthCredentials.swift:3291-3297` |
| Locking | No file lock. Coordination is in-process (single coordinator plus cooldowns) | |

### Response parsing

`ClaudeOAuthUsageFetcher.swift:283-330`; mapping in `P/Claude/ClaudeUsageFetcher.swift:965-1062`.

| Field | Meaning |
|---|---|
| `five_hour` | session window: `utilization` (percent used), `resets_at` (ISO 8601) |
| `seven_day` | weekly |
| `seven_day_sonnet`, `seven_day_opus` | model-specific weekly; CodexBar shows sonnet, else opus |
| `seven_day_oauth_apps`, `seven_day_routines` and similar | extra windows, shown when present |
| `iguana_necktie` | cloud credits |
| `extra_usage` | `is_enabled`, `monthly_limit`, `used_credits`, `utilization`, `currency`; amounts are minor units (divide by 100) |
| `cedar_ember` | reset status, only with the query parameter |
| `limits[]` | newer shape: `kind`, `group`, `percent`, `resets_at`, `scope.model.{id, display_name}` (`P/Claude/ClaudeWeb/ClaudeWebExtraRateWindowParser.swift:52-62`) |

Primary is `five_hour`, else `seven_day`. A response with neither a primary window nor a spend limit is an error ("missing session data").

### Rate limiting

| Rule | Source |
|---|---|
| 429 reads `Retry-After` as seconds or an HTTP date | `ClaudeOAuthUsageFetcher.swift:187-203` |
| Gate: default 5 min, or the longer of the current gate and `Retry-After`; keyed by SHA-256 of the access token; stored in UserDefaults; bypassed for a user-initiated refresh; cleared on success | `ClaudeOAuthUsageRateLimitGate.swift:6-50` |
| Source order in app auto mode: OAuth, then CLI, then web cookies (`claude.ai/api/organizations/{org}/usage`) | `P/Claude/ClaudeSourcePlanner.swift:176-184`, `P/Claude/ClaudeWeb/ClaudeWebAPIFetcher.swift:111-115` |

### Differences from QuotaBar

| Topic | CodexBar | QuotaBar (`Sources/Infrastructure/Claude/`) |
|---|---|---|
| Refresh | Delegates to the CLI when the CLI owns the token | Refreshes itself, JSON body with scopes `user:profile user:inference user:sessions:claude_code` |
| Writeback | Own Keychain cache only | Writes back to the original source: the file, or the Keychain item through `security -i` over stdin |
| Keychain read | Opt-in, preflight, no-UI | Always tries `security find-generic-password` after the file |
| User-Agent | `claude-cli/<ver> (external, cli)` | `QuotaBar` |
| Query parameter | `?cedar_ember=1` with fallback | none |
| Snapshot cache | Rate gate only | 5 min successful-snapshot cache, even when forced |
| Rate gate scope | Persisted (UserDefaults) | In memory |
| Model windows | `seven_day_sonnet`/`opus` plus `limits` | Same legacy fields plus `limits` entries of kind `weekly_scoped` only |

For the Go daemon: do not write to Claude Code's credentials. If a refresh is unavoidable, follow CodexBar and keep the result private to the daemon, or let the CLI refresh.

## Codex

### Endpoints

| Use | Request | Headers | Source |
|---|---|---|---|
| Usage | `GET <base>/wham/usage`; base defaults to `https://chatgpt.com/backend-api`. If the base lacks `/backend-api`, path is `/api/codex/usage` | `Authorization: Bearer`, `User-Agent: CodexBar`, `Accept: application/json`, `ChatGPT-Account-Id: <account id>` when known | `P/Codex/CodexOAuth/CodexOAuthUsageFetcher.swift:381-424` |
| Base URL | `chatgpt_base_url` in `$CODEX_HOME/config.toml` or `~/.codex/config.toml` | | same file `:629-686` |
| Reset credits | `<base>/wham/rate-limit-reset-credits` | adds `OpenAI-Beta: codex-1`, `originator: Codex Desktop` | same file `:524-545` |
| Spend | `<base>/accounts/{id}/spend-controls/current-user/monthly-usage`, `<base>/accounts/{id}/remaining_balance` | Bearer | same file |
| Refresh | `POST https://auth.openai.com/oauth/token`, JSON `{client_id, grant_type: refresh_token, refresh_token, scope: "openid profile email"}` | | `P/Codex/CodexOAuth/CodexTokenRefresher.swift` |

401 maps to unauthorized (`CodexAuthenticatedHTTPTransport.swift:21-28`).

### Credential sources

| Order | Source | Source ref |
|---|---|---|
| 1 | `$CODEX_HOME/auth.json`, else `~/.codex/auth.json`: `tokens{access_token, refresh_token, id_token, account_id}`, `last_refresh`; `OPENAI_API_KEY` means API-key mode | `CodexOAuthCredentials.swift:97-111,154-190` |
| 2 | Only if `CODEX_HOME` is unset, the native file is missing, and the user allows external sources: `~/.config/codex/auth.json`, then OpenCode's `auth.json` `openai{type: oauth, access, refresh, accountId, expires (ms)}` | `CodexOAuthCredentials.swift:366-446` |

No OS keyring support. Same on macOS and Linux.

### Refresh and writeback

| Question | Behaviour | Source |
|---|---|---|
| When | Expiry from the JWT `exp`; needs refresh within 5 min (or 60 s); without `exp`, when `last_refresh` is over 8 days old | `CodexOAuthCredentials.swift:51-62` |
| Usage path | Never redeems. `prepareCredentialsForUsage` throws `nativeRefreshRequired`, rereads `auth.json` up to twice with 50 ms between, then falls back to the Codex CLI (`codex app-server`) | `P/Codex/CodexProviderDescriptor.swift:344-377,393-414,465-476,498-530` |
| Errors | `refresh_token_expired`, `refresh_token_reused`, `invalid_grant`, `refresh_token_invalidated`; 401 means expired | `CodexTokenRefresher.swift` |
| Writeback (exists, unused by usage) | Merges into the existing JSON and writes with `CredentialFileWriter.writePrivate`: staged file opened `O_EXCL` with mode 0600 in a 0700 directory, `fsync`, atomic rename. No file lock | `CodexOAuthCredentials.swift:324-364`, `CredentialFileWriter.swift:20-74` |

### Response parsing

`CodexOAuthUsageFetcher.swift:6-62,160-330`.

| Field | Meaning |
|---|---|
| `rate_limit.primary_window`, `secondary_window` | `used_percent` (integer), `reset_at` (Unix seconds), `limit_window_seconds` |
| Window role | by length: 300 min is session, 10080 min is weekly; swapped if reversed (`CodexRateWindowNormalizer.swift:3-52`) |
| `credits` | `has_credits`, `unlimited`, `balance` (number or string) |
| `individual_limit`, `spend_control` | `limit`, `used`, `remaining_percent`, `resets_at` |
| `additional_rate_limits[]` | `limit_name`, `metered_feature`, `rate_limit`; decoded leniently |
| `account_id`, `plan_type` | identity and plan |

### Differences from QuotaBar

| Topic | CodexBar | QuotaBar |
|---|---|---|
| Refresh | Never in the usage path | Refreshes itself (form body) when `last_refresh` is missing or over 8 days old, and on 401 |
| Writeback | Private atomic write exists, unused | Atomic write of `auth.json`, mode not set explicitly |
| Expiry | JWT `exp` | `last_refresh` only |
| Window roles | By window length, swapped if reversed | Primary is session, secondary is weekly |
| Headers | Body fields | Also reads `x-codex-*` response headers first |
| `CODEX_HOME`, config base URL | Honoured | Not honoured |
| User-Agent | `CodexBar` | `OpenUsage` |

## Cursor

### Endpoints

All on `https://cursor.com`, cookie-authenticated, 15 s timeout (`P/Cursor/CursorStatusProbe.swift:1224-1370`).

| Use | Request | Notes |
|---|---|---|
| Summary (required) | `GET /api/usage-summary` | |
| Identity | `GET /api/auth/me` | optional; supplies `sub` and email |
| Sand usage | `POST /api/dashboard/get-sand-usage-status`, body `{}` | optional, 5 s |
| Unified billing | `GET /api/dashboard/v2/usage-status?scope=user` | only when the summary uses unified personal billing (`CursorUnifiedUsageStatus.swift:5`) |
| Legacy requests | `GET /api/usage?user=<id>` | only for non-unified, non-team plans; failure ignored |
| Team spend | `POST /api/dashboard/<endpoint>` with `Origin` and `Referer` | team plans (`CursorTeamSpend.swift:112-127`) |

Headers: `Accept: application/json`, `Cookie`. POSTs add `Content-Type: application/json` and `Origin: https://cursor.com`. 401 and 403 mean not logged in; any other non-200 is a network error (`CursorStatusProbe.swift` `fetchJSON`).

Cookie: `WorkosCursorSessionToken=<user id>%3A%3A<access token>`. The user ID is the part of the JWT `sub` after the last `|`, and must match `[A-Za-z0-9._-]+` (`CursorAppAuth.swift:140-158`).

### Credential sources

| Platform | Source | Source ref |
|---|---|---|
| macOS | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` | `CursorAppAuth.swift:202-246` |
| Linux | `$XDG_CONFIG_HOME` or `~/.config` + `/Cursor/User/globalStorage/state.vscdb`, then cursor-agent `~/.config/cursor/auth.json` `{accessToken}` | `CursorAppAuth.swift:202-246,373-399` |
| Both | Persisted session, browser cookies, manual cookie | `CursorStatusProbe+SessionResolution.swift` |

The database query is `SELECT value FROM ItemTable WHERE key='cursorAuth/accessToken'`, opened read-only with a 250 ms busy timeout. It falls back to `?immutable=1` only when the WAL sidecar files are missing (`CursorAppAuth.swift:276-325`). A session read from Cursor itself wins; an expired one falls through to browser cookies (`CursorStatusProbe+SessionResolution.swift:242-243`).

Newer Cursor builds may store the value as `cursor-safe-storage:v1:` plus base64 of `v10` and AES-CBC ciphertext. On macOS CodexBar decrypts it with Keychain item service `Cursor Safe Storage`, account `Cursor`, read with preflight and the no-UI query. The key is PBKDF2-SHA1 with salt `saltysalt`, 1003 iterations, 16 bytes; the IV is 16 spaces (`CursorAppAuth.swift:256-273`, `ChromiumSafeStorage.swift:11-82`). Linux has no decryption path.

No refresh and no writes.

### Response parsing

`CursorStatusProbe+UsageSummary.swift:15-121`; model at `CursorStatusProbe.swift:200-262`.

| Field | Meaning |
|---|---|
| `billingCycleStart`, `billingCycleEnd` | ISO 8601; the end is the reset |
| `membershipType`, `limitType`, `isUnlimited` | plan |
| `individualUsage.plan` | `used`, `limit`, `remaining` (cents), `breakdown`, `autoPercentUsed`, `apiPercentUsed`, `totalPercentUsed` (already percent, even below 1) |
| `individualUsage.onDemand` | `enabled`, `used`, `limit` (cents) |
| `individualUsage.overall` | enterprise personal cap (cents) |
| `teamUsage.onDemand`, `teamUsage.pooled` | team (cents) |

Headline percent order: verified team budget, `totalPercentUsed`, mean of auto and API percents, either one, `used/limit`, `overall`, `pooled`, else 0. CodexBar notes that `breakdown.total` is total used, not the limit.

### Differences from QuotaBar

| Topic | CodexBar | QuotaBar |
|---|---|---|
| DB access | SQLite library, read-only, busy timeout, immutable fallback | `/usr/bin/sqlite3` |
| Linux | `~/.config/Cursor/...` and cursor-agent `auth.json` | macOS path only |
| Encrypted token | Decrypts on macOS | Not handled |
| Cookie user ID | Part after last `|`, validated, `%3A%3A` | Whole `sub`, literal `::`, not validated |
| Capacity | `plan.limit` | `max(limit, breakdown.total)` to cover bonus credit |
| Accept header | Set | Not set |

The capacity difference needs care: QuotaBar's `max` rule fixed a real false-depleted case (commit `e1e73d6`). Check both readings against a captured response before choosing.

## OpenCode Go

### Endpoints

| Use | Request | Headers | Source |
|---|---|---|---|
| API usage | `GET https://opencode.ai/zen/go/v1/usage` | `Authorization: Bearer <key>`, `Accept: application/json`, `User-Agent: CodexBar` | `P/OpenCodeGo/OpenCodeGoUsageFetcher.swift:183-216` |
| Console workspaces | `GET https://opencode.ai/console/api/orgs` | `Cookie` | `:38,431-446` |
| Console Go status | `GET https://opencode.ai/console/api/go/status` | `Cookie`, `x-org-id: <workspace>` (400 without it) | `:40-44,448-473` |
| Console balance | `GET https://opencode.ai/console/api/billing/status` | as above | `:42` |
| Legacy | `https://opencode.ai/_server` server functions (`X-Server-Id`, `X-Server-Instance`) and the page `https://opencode.ai/workspace/<id>/go`, parsed by regex | `Cookie`, browser User-Agent | `:403-429,752-790` |

Redirects are refused unless they stay on allowed hosts, and the session stores no cookies (`:49-92,275`). 401 or 403 means invalid credentials.

### Credential sources

| Order | Source | Source ref |
|---|---|---|
| 1 | Env `OPENCODE_API_KEY` | `OpenCodeGoSettingsReader.swift:4-8` |
| 2 | OpenCode CLI store: `$XDG_DATA_HOME` (absolute only) or `~/.local/share` + `/opencode/`. In `opencode.db` (or `$OPENCODE_DB`), table `credential`, `active = 1`, `integration_id` `opencode-go` before `opencode`. Without that table, `auth.json` `opencode-go.key` | `OpenCodeGoLocalUsageReader.swift:319-327,492-530` |
| 3 | Browser or manual cookie for the console and legacy paths | `OpenCodeGoProviderDescriptor.swift:140-158` |

Database errors are reported as invalid credentials so no contents leak. No refresh and no writes.

In auto mode CodexBar starts with a local estimate built from `opencode.db` message costs against fixed limits of $12 (5 h), $30 (weekly) and $60 (monthly), then overlays remote data (`OpenCodeGoLocalUsageReader.swift:27-29`, `OpenCodeGoProviderDescriptor.swift:154-158`).

### Response parsing

| Response | Fields | Source |
|---|---|---|
| API | `usage.rolling`, `usage.weekly`, `usage.monthly`; each has a percent key (already 0-100) or used/limit, and `resetInSec` or a reset-at time | `OpenCodeGoUsageFetcher.swift:578-603,704-750` |
| Console | `access.meters.fiveHour`, `week`, `month`, `resetsAt`; `access.endsAt` fills a missing monthly reset; `access: null` means no subscription | `:536-576` |

Percents are clamped to 0-100. The weekly window is optional; the 5-hour window is required.

### Differences from QuotaBar

QuotaBar uses only the API path with `auth.json` `opencode-go.key`. It does not read the `credential` table in `opencode.db`, does not reject a relative `XDG_DATA_HOME`, and requires all three windows. Commit `a69ef0e` dropped the local cost estimate because it undercounted server deductions; do not bring it back. Check whether current OpenCode stores the key in `opencode.db` rather than `auth.json`, since CodexBar prefers the database.

## Muse

### Endpoints

| Use | Request | Headers | Source |
|---|---|---|---|
| Quota | `POST https://api.meta.ai/muse-code/key`, body `{}`, 15 s | `Authorization: Bearer <dca: token>`, `x-api-version: 1.0.0`, `User-Agent: CodexBar` | `Resources/Plugins/muse.ts:17-27` |
| Optional team quota | `GET https://dev.meta.ai/api/auth/me` and team quota paths, browser cookie, at most 5 requests of 8 s | `Cookie` | `muse.ts:90-130` |

Status handling: 401 or 403 means login rejected; 429 rate limited; 500 and above unavailable; other non-200 an API failure (`muse.ts:22-27`). The device token is never sent to `dev.meta.ai`.

The provider runs as a bundled TypeScript plugin with a 60 s overall deadline (`P/Muse/MuseProviderDescriptor.swift:92-111`).

### Credential sources

`P/Muse/MuseCredentials.swift`.

| Order | Source | Notes |
|---|---|---|
| 1 | `$MUSE_AUTH_PATH`, else `~/.config/muse/auth.json`: `providers.meta.access_token`, or `mechanism: oauth` | `:50-83`. Does not check `$XDG_CONFIG_HOME` |
| 2 | macOS Keychain service `ai.meta.dev.credentials`, account `meta`, JSON `{access_token}` | `:9-10,102-178` |

The token must start with `dca:` (`:182-189`). The Keychain path runs the ACL preflight first. If the read would prompt, it reads only when the app can show its own explanation first; background refreshes never prompt (`:108-129`). An availability check detects the item without reading the secret (`:85-100`). No refresh and no writes.

### Response parsing

`muse.ts:57-75,238-259`.

| Field | Meaning |
|---|---|
| `require_payment: true` | billing incomplete, error |
| `is_subs_active` | must be `true` |
| `subs_tier_name`, `user_email` | plan and identity |
| `subs_usage.window` | `window_duration_mins`, `used_percent`, `resets_at` (Unix seconds) |
| `subs_usage.weekly` | `used_percent`, `resets_at` |

Percents are clamped to 0-100. Reset values at or below 0, or past year 4000, are dropped.

### Differences from QuotaBar

| Topic | CodexBar | QuotaBar |
|---|---|---|
| Body | `{}` | `{"onboard":false}` |
| Keychain | Fallback when the file has no token | Only when `providers.meta.storage == "keychain"` |
| `XDG_CONFIG_HOME` | Ignored | Honoured |
| Standing `api_key` | Not needed | Required |
| Cooldown | None in the plugin | 5 min, persisted and locked under `~/.quotabar/muse` |
| Size cap, redirects | Not set in the plugin | 256 KiB cap, redirects refused |

## Patterns worth copying

| Pattern | Where |
|---|---|
| Do not redeem a refresh token another program owns; reread its file or let the owner refresh | Claude `ClaudeOAuthCredentials.swift:1605-1627`, Codex `CodexProviderDescriptor.swift:465-476` |
| Reread credentials a few times with a short sleep before failing, to catch a CLI mid-write | `CodexProviderDescriptor.swift:393-414` |
| Rate gate keyed by a token hash, taking the longer of the current gate and `Retry-After`, cleared on success | `ClaudeOAuthUsageRateLimitGate.swift:6-50` |
| Refresh-failure gate: terminal on `invalid_grant` until auth changes, else 5 min to 6 h backoff | `ClaudeOAuthRefreshFailureGate.swift` |
| Private atomic writes: 0600 staged file opened `O_EXCL`, `fsync`, rename | `CredentialFileWriter.swift:20-74` |
| SQLite read-only with busy timeout; `immutable=1` only when WAL files are absent | `CursorAppAuth.swift:276-325` |
| Validate a JWT claim before putting it in a header | `CursorAppAuth.swift:144-157` |
| Hide database and decoding errors behind a generic credential error | `OpenCodeGoLocalUsageReader.swift:510-518` |
| Refuse cross-host redirects for credentialed requests | `OpenCodeGoUsageFetcher.swift:49-92,275` |

## Why the CodexBar app may use a lot of CPU and memory

These are likely causes from a short read, not measured:

- Local cost scanning. CodexBar parses Claude, Codex, Muse and OpenCode session logs (JSONL and SQLite) to estimate spend (`Vendored/CostUsage/`, `P/Muse/MuseLocalUsageReader.swift`). Its own comment says the Muse session tree is "routinely gigabytes across thousands of files" (`MuseLocalUsageReader.swift:101`).
- Animated status icon driven by a display link (`CodexBar/DisplayLink.swift`, `StatusItemController+Animation.swift:1329`), which runs every frame while active.
- WebKit dashboards and browser-cookie probes (WKWebView in about 10 files), PTY sessions of the `claude` and `codex` CLIs, and a JavaScript plugin runtime.
- The app ships a memory-pressure monitor that trims caches and calls `malloc_zone_pressure_relief` (`CodexBar/MemoryPressureMonitor.swift`, `MemoryPressureRelief.swift`) and a main-thread hang watchdog, which suggests the authors have seen both problems.

The daemon avoids all of these by calling only the quota HTTP APIs.
