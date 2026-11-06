# Plan: Go quota daemon (`quotad`) with the Swift panel as its client

Status: done 2026-11-06. Replaced the Rust daemon on `migration/rust-daemon`; quotad runs on Linux (systemd user unit via `quotad install`) and on the Mac (bundled in QuotaBar.app, registered with SMAppService). The Rust binaries and units are deleted on both machines.

## Goals

- One always-on backend, the same Go binary on the Linux box and the Mac, as cheap as possible at idle.
- The Mac keeps its menu bar panel (the Swift app, later a feed client).
- Simple. The Rust attempt grew to 18.7k lines; the budget here is about 3k lines of non-test Go.

## Hard limits (every brief carries these)

- Go standard library only. No third-party modules. `sqlite3` and `/usr/bin/security` are called as processes.
- No goroutine per request beyond `net/http`'s own; one goroutine per provider during a refresh; one background loop.
- No abstraction without two real users. No plugin registry, no config layers, no generic retry framework.
- Package line budgets (non-test code lines; blank lines and comments do not count, so write normally spaced, commented Go): `monitor` 400, `feed` 500, `creds` 250, `settings` 120, each provider 350 (Claude 550 and Codex 450: token refresh, writeback and rate-limit state are required behaviour), `main.go` 150.
- Anything not in this plan is out of scope; record it as a follow-up instead.

## Layout

```
quotad/                 go.mod (module quotad), go 1.27
  main.go               flags, wiring, signals
  monitor/              refresh rules, snapshot cache, pace-aware status, background loop
  feed/                 QuotaFeedDTO JSON, HTTP server, hooks and their text
  providers/{claude,codex,cursor,opencode,muse}/
  creds/                file read, atomic 0600 write, Mac `security` read/write with timeout
  settings/             ~/.quotabar/settings.json, reread when mtime changes
```

Provider interface: `ID() string`, `Available() bool`, `Fetch(ctx) (Snapshot, error)`. Each provider has a pure parse function tested on captured responses (ported from the Swift tests in `Tests/InfrastructureTests/`) and HTTP tested with `httptest`.

## Behaviour (port of the Swift app)

- `monitor`: refresh enabled providers concurrently; reuse a snapshot under 60 s unless forced; join an in-flight refresh; background loop every `app.backgroundSyncInterval` (60 s floor). Status logic from `Sources/Domain/Provider/QuotaStatus.swift` and `UsageQuota.swift`.
- `feed`: 127.0.0.1:`feed.port` (8787), reject `Host` other than 127.0.0.1/localhost. `GET /quotas` (refresh, wait at most 20 s), `POST /hooks/session-start`, `POST /hooks/prompt`; byte-compatible with `Sources/Infrastructure/Server/` (golden test against JSON captured from the live Swift feed).
- Tokens: Claude and Codex refresh and write back as Swift does (file: temp + fsync + rename, 0600; Mac Claude Keychain: `security -i` over stdin). Cursor, OpenCode Go, Muse read only.
- Logs: `log/slog` to stderr; never log tokens.

## Per-provider fixes from CodexBar (`docs/research/codexbar-provider-reference.md`)

| Provider | Fix |
|----------|-----|
| Claude | persisted rate-limit state keyed by token hash; honour `Retry-After`; stop refreshing on `invalid_grant`, else back off 5 min to 6 h |
| Codex | session/weekly by window length (300 / 10080 min); expiry from JWT; `CODEX_HOME` |
| Cursor | cookie from JWT `sub` after last `\|`; `state.vscdb` read-only; Linux `~/.config/cursor/auth.json`; keep Swift's plan-capacity maths |
| OpenCode Go | `auth.json`, fallback `opencode.db` `credential` table |
| Muse | Mac: `/usr/bin/security -w` (spike pending on whether access survives token rewrites); Linux: `~/.config/muse/auth.json`; keep cooldown files in `~/.quotabar/muse` |

## Order of work

1. Skeleton: `go.mod`, `settings`, `creds`, `monitor`, `feed` with one fake provider. Gate: `go vet ./... && go test ./...`.
2. Providers, one job each: Claude, Codex, Cursor, OpenCode Go, Muse.
3. Linux: cross-compile (`GOOS=linux GOARCH=amd64 CGO_ENABLED=0`), systemd user unit, run on a spare port beside the Rust daemon for a day, compare with `/quotas`, then cut over to 8787.
4. Mac: LaunchAgent; Swift app becomes a feed client (delete probes, loaders, server; keep views, theme, alerts, icon).
5. Remove the Rust daemon and its units.

## Out of scope

Signing, Security.framework, proof tooling, UID/audit checks, SHA manifests, cost and log scanning, notifications from the daemon.
