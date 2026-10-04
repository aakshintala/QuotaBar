# QuotaBar

A macOS menu bar app that shows how much of your AI coding subscriptions is left: Claude, Codex, Cursor and OpenCode Go. It can also serve the same numbers to Claude Code sessions and other local tools over a localhost feed.

QuotaBar began as a fork of [tddworks/ClaudeBar](https://github.com/tddworks/ClaudeBar) and was cut down to the providers above. Thanks to the ClaudeBar authors and contributors.

## Features

- One row per quota window (session, weekly, monthly, per model), with its reset time.
- Status colours that account for pace: 30% left is fine early in a window and a warning late in it.
- Balance and request-count meters where a provider reports them (Codex credits, Cursor requests).
- System notifications when a provider's status gets worse.
- Optional background refresh (at most once a minute, less often on battery).
- Dark and light themes.
- A localhost quota feed and Claude Code hooks (below).

## Providers

QuotaBar reads the credentials each provider's own tool already stores, so there is nothing to sign in to in the app.

| Provider | Needs |
|----------|-------|
| Claude | Claude Code signed in (`~/.claude/.credentials.json` or its Keychain item) |
| Codex | Codex CLI signed in (`~/.codex/auth.json`) |
| Cursor | Cursor signed in on this Mac |
| OpenCode Go | opencode signed in to OpenCode Go (`~/.local/share/opencode/auth.json`) |

Turn providers on or off in Settings. Settings are stored in `~/.quotabar/settings.json` and logs in `~/Library/Logs/QuotaBar/`.

## Quota for Claude Code agents

QuotaBar can tell Claude Code sessions how much quota is left, so agents can
route work ("Claude weekly is at 12%, delegate to Codex") and hold back
expensive runs. The app answers Claude Code hooks itself: no extra process
stays running per session.

1. Settings → "Quota Feed" → enable. It listens on `127.0.0.1:8787` only.
2. Add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "timeout": 3,
      "command": "curl -s --max-time 2 -X POST -H 'Content-Type: application/json' --data-binary @- http://127.0.0.1:8787/hooks/session-start || true" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "http", "url": "http://127.0.0.1:8787/hooks/prompt", "timeout": 2 }] }]
  }
}
```

SessionStart uses `curl` because Claude Code does not run HTTP hooks for that
event. `curl` passes the hook input through, so the app knows the session.

- **Session start** adds the full feed to context: one line per healthy
  provider, with reset times only on buckets that need attention.
- **Each prompt** adds nothing unless a bucket got worse since the session was
  last told. A low quota is announced once, not on every prompt.
- Hooks read the app's cached data and never trigger a probe, so they cannot
  stall a session. The text states how old the data is.
- `GET /quotas` serves the raw JSON feed for other clients.

## Build from source

Requires macOS 15+, Xcode with Swift 6, and [Tuist](https://tuist.io) (`brew install tuist`). From a checkout of `github.com/aakshintala/QuotaBar`:

```bash
tuist install
tuist build QuotaBar -C Release
```

For development, run `tuist generate` and open `QuotaBar.xcworkspace`. Run tests with `xcodebuild`, not `tuist test`: [`CLAUDE.md`](CLAUDE.md) has the exact command, and [`docs/architecture/ARCHITECTURE.md`](docs/architecture/ARCHITECTURE.md) explains how the code fits together.

## License

MIT — see [LICENSE](LICENSE)
