# QuotaBar daemon

`quotabard` is the Rust implementation of QuotaBar's on-demand producer and HTTP client. It contains all five provider adapters, the loopback feed, Claude Code hooks and the `quota` daemon subcommand.

The migration is prepared, not activated. The Swift engine and installed consumers have not been replaced. Service installation, owner Keychain acceptance and consumer cutover require separate approval. Do not start the daemon alongside an existing producer or use the prepared service scripts as an acceptance test.

## Runtime

The daemon uses one current-thread Tokio runtime, one scheduler and one shared HTTP client. It constructs five fixed provider adapters without reading credentials or making provider requests at startup:

| Provider ID | Provider |
|---|---|
| `claude` | Claude |
| `codex` | Codex |
| `cursor` | Cursor |
| `opencode-go` | OpenCode Go |
| `muse` | Muse |

Requests start or join at most one check per enabled provider, with at most five checks in flight. Successful readings are reused for five minutes. A caller deadline or disconnect does not cancel shared provider work. The daemon has no periodic provider refresh or maintenance loop. Synchronous filesystem operations cannot be preempted by Tokio deadlines.

Readings remain in memory only. After restart, providers start unknown. Persisted cooldowns prevent requests before their saved deadlines; corrupt or unreadable records fail closed if a replacement recovery guard cannot be saved.

## Build and offline checks

From this directory:

```sh
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets -- -D warnings
cargo test --offline --locked --all-targets
cargo build --offline --locked --release --bin quotabard --lib
cargo build --offline --locked --release --example opencode-live-check --example offline-resource-check --example disposable-keychain-proof
python3 -m unittest discover -s tests/service -p 'test_service_scripts.py'
```

Build commands do not run examples or activate a service. Ordinary tests use fake providers and credential readers. They do not access owner Keychain items or call the native Keychain interaction-suppression setter. Service tests use temporary homes and mocked manager commands and PID probes; `plutil`, when used, receives only staged temporary files.

Native Keychain proof and live-provider examples are opt-in and are not ordinary validation commands. Do not run them without the applicable approval.

## Feed and hooks

Production binds only to `127.0.0.1:8787`. The listener requires an exact loopback Host and port, rejects every Origin header, and serves one request per connection.

| Route | Behaviour |
|---|---|
| `GET /quotas` | Refreshes eligible providers or joins existing checks, waiting at most 4.5 seconds before returning current state |
| `PUT /providers/{id}/enabled` | Changes the provider setting using the private control token; does not start a check |
| `POST /hooks/session-start` | Gets bounded quota context through the shared scheduler |
| `POST /hooks/prompt` | Uses cached state only and reports worsening buckets for the session |

Headers have an absolute two-second deadline. Hook connections have a two-second total deadline, including response writes; other connections have five seconds. The listener caps connections at 32, handlers at 16 and headers at 16 KiB. Hook bodies have a 1 MiB cap; other bodies have a 1 KiB cap. Hook context uses the shared formatter and is capped at 10,000 Unicode scalars. Session history holds bounded severity metadata, not prompts or quota readings.

See the [feed v1 contract](../docs/plans/rust-daemon-feed-v1.md) for JSON shape, authentication and hook semantics. There is no alternate health or cached-feed route.

## HTTP client

After a separately approved daemon deployment, the public shell command is `aiquota`; its wrapper dispatches internally to the unchanged `quotabard quota` subcommand. Pi's `/quota` command and `get_quotas` tool remain unchanged. The staged wrapper is not installed, and installed clients remain unchanged until cutover.

```sh
aiquota
aiquota --json
aiquota enable claude
aiquota disable muse
```

The underlying daemon subcommand remains:

```sh
quotabard quota
quotabard quota --json
quotabard quota enable claude
quotabard quota disable muse
```

Client dispatch happens before daemon initialization. The client does not construct providers, read their credentials or acquire the daemon's instance lock. `QUOTA_FEED` retains its existing meaning: a full loopback HTTP feed URL, defaulting to `http://127.0.0.1:8787/quotas`. The client rejects non-loopback URLs, redirects, user information, query strings and fragments.

GET needs no control token. Enable and disable read the existing private token without creating or changing daemon state; no token is placed in command-line arguments. Client request and connect deadlines are eight and five seconds.

## Configuration and state

The daemon validates `~/.quotabar/daemon.json` at startup and request boundaries. Missing configuration enables all five providers. To disable all checks in an isolated offline configuration, use:

```json
{"disabledProviders":["claude","codex","cursor","opencode-go","muse"]}
```

Invalid configuration is not overwritten. A simultaneous manual edit can still race with a daemon settings mutation; there is no watcher or atomic compare-and-swap with an external editor.

Runtime files live under `~/.quotabar/daemon/`. Directories use mode 0700 and managed files mode 0600. The stable `instance.lock` is never replaced or unlinked. The control token contains 64 lowercase hexadecimal characters and a newline and remains opaque in memory. Unsafe or malformed existing managed files fail closed.

Atomic rename is the commit point for settings and credential writes. Callers apply committed data even if the later directory sync is uncertain. A cooldown whose durability is uncertain blocks requests rather than risking an early retry.

## Prepared deployment and consumers

- [Service management](SERVICE.md) covers explicit per-user install, update and removal, logging, rollback, private-recovery assumptions and remaining native acceptance gates. Python is needed only for management scripts, not the running daemon. The scripts do not enable Linux lingering.
- [Inactive consumers](clients/README.md) contains the reviewed Pi patch and shell wrapper. These are stored artifacts, not installed clients or an automatic cutover path.

The installed binary paths are `$HOME/.local/bin/quotabard` on Linux and `$HOME/Library/Application Support/QuotaBar/bin/quotabard` on macOS.

## Keychain evidence and remaining acceptance

The independently reviewed disposable Keychain proof passed once using synthetic items in an isolated file-backed keychain. It did not access owner Cursor or Muse items, change the default keychain or search list, or establish real-item acceptance. Repeating it requires approval; the executable calls native APIs only with its explicit `--run-disposable-proof` opt-in.

Owner-present Mac live checks found Codex and OpenCode Go working; Cursor and Muse returned `keychain_blocked`. Metadata checks found both items in unlocked Keychains but did not establish application authorization. Claude now has a file-first native Keychain implementation with an exact service/account lookup and checked interaction suppression. Offline tests do not prove owner-item access, signing identity, or installed-path access. See the [Keychain safety evidence](../docs/research/rust-migration-keychain-safety.md) and [Mac live result](../docs/research/rust-migration-mac-live-acceptance.md).

[Linux live verification](../docs/research/rust-migration-linux-live-acceptance.md) passed all five providers, the real HTTP-only CLI and a 1,000-request cache burst. Production-process resources were measured separately on both hosts; the reports distinguish those runs from earlier synthetic baselines and record measurement limits. All temporary daemons stopped afterward.

The expanded disposable proof is being repaired after source review. It tests an allowed raw update and the actual locked-update outcome with exact synthetic data verification, not an assumed write-denial ACL. It has not been run. The [write-proof ruling](../docs/plans/rust-native-write-proof-ruling.md) separately requires cross-identity access evidence before any real Claude Keychain write; same-process read-back cannot prove the official CLI keeps access. Do not run a real Mac provider check that could automatically renew that record before those gates and separate owner approval. Source review, offline gates and owner-authorized native execution remain distinct. Actual OAuth-write acceptance, native service lifecycle/logging, all-provider Mac access and consumer cutover remain open. Keep installed consumers unchanged until deployment is approved.
