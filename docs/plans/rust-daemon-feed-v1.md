# Rust daemon feed v1 contract

This contract freezes the phase 0 feed and control interfaces for `quotabard`. It follows the approved [migration roadmap](rust-daemon-migration.md). It does not claim exact parity with the current Swift feed.

## Quota feed

`GET /quotas` returns HTTP 200 with `application/json`. It starts eligible checks for enabled providers, waits up to 4.5 seconds, and returns within five seconds. A provider that is still checking returns its latest reading, if any, with its age and safe error code. Without a reading, its status and freshness are `unknown`, and `quotas` is empty. Disabled providers are omitted from `providers` and listed in `disabledProviderIds`.

All object properties below are required. A nullable property is present with JSON `null`, not omitted. Times are UTC RFC 3339 strings. `ageSeconds` is a non-negative JSON integer. Balance values are JSON numbers, never strings. No account metadata, tokens, keys, raw provider errors, or response bodies appear in this feed.

| Field | Type | Nullability and meaning |
|---|---|---|
| `schemaVersion` | integer | Non-null; value `1` |
| `generatedAt` | timestamp | Non-null; response generation time |
| `disabledProviderIds` | string array | Non-null; defaults to `[]` |
| `providers` | provider array | Non-null; enabled providers only |
| `id`, `name` | string | Non-null |
| `tier` | string | Nullable; present only when known |
| `capturedAt` | timestamp | Nullable; time of last successful observation |
| `ageSeconds` | integer | Nullable with no observation; otherwise wall-clock age |
| `freshness` | string | `unknown`, `fresh`, or `stale` |
| `errorCode` | string | Nullable safe error code; no raw error text |
| `status` | string | `unknown`, `healthy`, `warning`, `critical`, or `depleted` |
| `quotas` | bucket array | Non-null; empty when there is no current observation |
| `key`, `label` | string | Non-null; `key` is the stable provider/bucket history key component |
| `percentRemaining` | number | Nullable; can be negative for over-limit percentage meters |
| `balanceRemaining`, `balanceUsed`, `balanceCap` | number | Each nullable; JSON numbers in USD or credits, not strings |
| `balanceUnit` | string | Nullable; `usd` or `credits` |
| `unitsUsed`, `unitsLimit` | integer | Each nullable; count meters |
| `resetsAt` | timestamp | Nullable |
| `resetText` | string | Nullable; response-time reset wording |
| bucket `status` | string | `healthy`, `warning`, `critical`, or `depleted` |

Meters use the applicable representation: percentage, balance, or counts. A balance without a cap has no percentage. Do not invent a cap or a full quota for missing or expired windows. Remove windows whose reset time is at or before response time. `resetText` and severity are computed at response time. Provider status is the most severe bucket status, or `unknown` when there are no buckets. `unknown` is a valid provider status, but it is not a bucket severity and is not compared in bucket history.

Freshness is `unknown` without a successful observation, `fresh` while its age is under five minutes, and `stale` at five minutes or older. A future `capturedAt` is invalid and makes the provider unknown until another successful observation. Keep the prior reading and its age after a slow or failed check; expose the safe error code separately. Set `errorCode` to `busy` when a provider attempt is in flight and no more specific error applies.

Severity follows the domain quota rules: depletion at zero or below; absent meaningful pace, critical below 20%, warning below 50%, otherwise healthy. Where a window has a meaningful duration, use the existing pace-aware `QuotaStatus` rules rather than copying their algorithm here. Balance-only meters are healthy. Cursor billing-cycle meters without a represented duration use absolute thresholds.

Partial example (one provider and one bucket; the feed can include all five providers):

```json
{
  "schemaVersion": 1,
  "generatedAt": "2026-09-26T12:00:00Z",
  "disabledProviderIds": ["muse"],
  "providers": [
    {
      "id": "opencode-go",
      "name": "OpenCode Go",
      "tier": null,
      "capturedAt": "2026-09-26T11:58:00Z",
      "ageSeconds": 120,
      "freshness": "fresh",
      "errorCode": null,
      "status": "healthy",
      "quotas": [
        {
          "key": "session",
          "label": "5-hour limit",
          "percentRemaining": 79.16667,
          "balanceRemaining": null,
          "balanceUsed": null,
          "balanceCap": null,
          "balanceUnit": null,
          "unitsUsed": null,
          "unitsLimit": null,
          "resetsAt": "2026-09-26T14:00:00Z",
          "resetText": "Resets in 2h",
          "status": "healthy"
        }
      ]
    }
  ]
}
```

## Error codes

The provider's `errorCode` is a stable lowercase code or `null`. Use only these safe values:

| Code | Meaning and action |
|---|---|
| `credentials_missing` | No usable login; sign in with the provider CLI |
| `login_expired` | Login rejected or expired; renew the provider CLI login |
| `keychain_blocked` | Keychain read was blocked; check access without prompting |
| `subscription_unavailable` | Provider explicitly reports no active subscription |
| `cooldown` | Provider guard is active; retry after its deadline |
| `rate_limited` | Provider rate-limited the request; honor `Retry-After` |
| `network_error` | Request failed; check connectivity and retry |
| `configuration_error` | Daemon configuration is invalid; correct `~/.quotabar/daemon.json` |
| `busy` | Provider attempt is in flight and no more specific error applies |
| `provider_error` | Other safe provider failure; retry or inspect local daemon logs |
| `persistence_error` | Required state could not be saved; do not retry provider work until fixed |
| `internal_error` | Other safe daemon failure; inspect local daemon logs |

Do not infer `subscription_unavailable` from HTTP 403 alone. These codes never contain raw error text. V1 has no `throttledUntil` field; `cooldown` or `rate_limited` conveys that state. The current Swift feed's `unavailable` text is not carried forward.

## Claude Code hooks

Keep `POST /hooks/session-start` and `POST /hooks/prompt`. Both accept Claude Code's JSON hook body; `session_id` is the session-history identifier. SessionStart starts eligible shared checks, waits at most 1.5 seconds, and responds within two seconds. UserPromptSubmit is cached-only, never probes, and responds within two seconds. Keep prompt text out of logs and retained state.

When context is available, both routes use this envelope shape:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": "Quota headroom (QuotaBar):\nOpenCode Go: 79% remaining"
  }
}
```

Use `hookEventName: "UserPromptSubmit"` for prompt warnings. If there is no context or no newly worsened bucket, return `{}`. The exact human-readable formatter is not frozen here. For a valid session ID, SessionStart records the bucket severities it reports as the session's baseline, so the first prompt does not repeat the same warnings. Prompt history compares severity for each `(provider id, bucket key)` and reports only worsening; session IDs must be valid, idle sessions expire after 24 hours, and the history is capped at 1,024 sessions with least-recently-used eviction. Missing or malformed IDs do not share history or update it.

## Configuration and control

`~/.quotabar/daemon.json` contains `{"disabledProviders":[]}` by default. Missing file means defaults; malformed JSON fails closed. Reject unknown provider IDs. `disabledProviders` is the persisted setting; `disabledProviderIds` is its feed representation. Read configuration at each request boundary. A disabled provider starts no new credential or provider work, its reading is hidden immediately, and an in-flight result is discarded.

`PUT /providers/{id}/enabled` accepts `{"enabled":true}` or `{"enabled":false}`. On success, return HTTP 200 with `{"id":"muse","enabled":false}` (using the requested provider ID and value). Require the configured token in the `X-QuotaBar-Control-Token` header. Store it at `~/.quotabar/daemon/control-token` as exactly 64 lowercase hexadecimal characters followed by one newline. Generate the 32 token bytes from the operating system's random source. Set the daemon state directory to mode `0700` and the token file to mode `0600`. Never put the token in a URL or logs.

Reload and validate configuration before each mutation, then atomically write and apply it. Read-only quota requests remain unauthenticated on IPv4 loopback. Every non-feed HTTP error uses `application/json` with `{"errorCode":"<safe-code>"}`. Use these status and code pairs:

| HTTP status | `errorCode` | When |
|---|---|---|
| 400 | `invalid_request` | Malformed control body or unknown provider ID |
| 401 | `unauthorized` | Missing or incorrect control token |
| 403 | `forbidden` | Host or Origin is disallowed |
| 413 | `request_too_large` | Request body exceeds the configured limit |
| 503 | `configuration_error` | Configuration is malformed or cannot be used |
| 503 | `capacity_unavailable` | Daemon cannot accept work within its configured capacity |

Do not include raw errors or token details in error responses.

## Known evidence limits

Provider APIs are unofficial. The roadmap records OpenCode Go returning 403 to a Python check and 200 to one native Mac `URLSession` request. Do not infer invalid credentials from the Python result. Phase 0 and its fixture gate make no provider API calls.
