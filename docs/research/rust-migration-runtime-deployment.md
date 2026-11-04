# Rust daemon runtime and deployment research

Research notes for a single Rust quota daemon on Linux and macOS. This note records options, not final decisions. The [migration roadmap](../plans/rust-daemon-migration.md) selects proposed defaults and takes precedence where its choices differ, including snapshot persistence and polling intervals. This note installed no packages, made no provider calls, and ran no build or tests. The companion [Linux credential research](rust-migration-linux-credentials.md) records one owner-authorized Cursor quota request.

## Recommendation

Start with one Rust binary, one Cargo crate, and one explicit per-user service install path per operating system. Use Tokio, Axum, Reqwest, Serde, and the Rust standard library. Let the daemon own provider refreshes and daemon configuration; let the SwiftUI app keep its existing appearance and notification settings. Do not make the app start or supervise the daemon.

Make `GET /quotas` return the current cache without waiting for provider APIs. If the cache is stale, schedule one shared refresh in the background. Add an authenticated `POST /refresh` only when the app needs a control surface. Keep Claude’s `/usr/bin/security` access path. Treat Muse’s Keychain-backed login as a separate, unverified integration: its current no-UI Security.framework query does not prove that a new Rust executable can read the item.

Use explicit install, update, and removal scripts that operate only in the current user’s home directory. Use a systemd user service on Linux and a LaunchAgent in the logged-in user’s GUI session on macOS. Do not enable Linux lingering automatically. Do not add an updater, app self-install, Docker requirement, cross-platform service abstraction, or Swift-to-Rust FFI layer. Maintained, target-specific Rust bindings to native macOS facilities are not prohibited.

## Current code and client constraints

The app registers the stable provider IDs `claude`, `codex`, `cursor`, `opencode-go`, and `muse` in [`QuotaBarApp.swift`](../../Sources/App/QuotaBarApp.swift). The IDs are also settings and feed keys; keep them stable even if the new wire format changes ([`CLAUDE.md`](../../CLAUDE.md)).

Credentials are not uniform. Claude and Codex read OAuth JSON and may refresh and rewrite it; OpenCode Go reads its local auth JSON; Muse reads its CLI OAuth metadata and may read a Keychain item. The current macOS Cursor probe reads `state.vscdb` through `sqlite3` ([Claude](../../Sources/Infrastructure/Claude/ClaudeCredentialLoader.swift), [Codex](../../Sources/Infrastructure/Codex/CodexCredentialLoader.swift), [Cursor](../../Sources/Infrastructure/Cursor/CursorUsageProbe.swift), [OpenCode](../../Sources/Infrastructure/OpenCode/OpenCodeCredentialLoader.swift), [Muse](../../Sources/Infrastructure/Muse/MuseCredentialLoader.swift)). Linux credential-source compatibility is now established for all five on the inspected Ubuntu 26.10 host: the four non-Cursor providers had expected file-backed auth shapes, and Cursor’s `~/.config/cursor/auth.json` token produced one owner-approved HTTP 200 response with expected quota objects ([Linux credential research](rust-migration-linux-credentials.md)). This proves current-login compatibility, not a supported Cursor quota API or a stable credential contract. For Linux, use the CLI auth file and current cookie construction, reread it after authentication failure, and do not invoke the CLI to refresh credentials. Do not require Linux `sqlite3` for Cursor.

The existing monitor skips snapshots younger than 60 seconds and joins an in-flight refresh per provider ([`QuotaMonitor.swift`](../../Sources/Domain/Monitor/QuotaMonitor.swift)). Claude has a separate five-minute snapshot cache ([`ClaudeAPIUsageProbe.swift`](../../Sources/Infrastructure/Claude/ClaudeAPIUsageProbe.swift)). Muse persists its per-login cooldown, last observation, and a file lock ([`MuseUsageProbe.swift`](../../Sources/Infrastructure/Muse/MuseUsageProbe.swift)). Preserve provider-specific rate limits, but do not require full UI or error-state parity in the first daemon.

The current `GET /quotas` can wait up to 20 seconds for refresh ([`QuotaFeedService.swift`](../../Sources/Infrastructure/Server/QuotaFeedService.swift)). That exceeds the Pi consumer’s 5-second footer deadline and 8-second tool deadline (`~/work/pi-extensions/extensions/status/quota.ts`). The new cached GET should not inherit the current wait. The current feed DTO contains generated time, providers, and disabled provider IDs; it does not include app settings or `costUsage` ([`QuotaFeedDTO.swift`](../../Sources/Infrastructure/Server/QuotaFeedDTO.swift)). A smaller versioned DTO is reasonable; update consumers alongside it rather than preserving unused fields.

The app owns `~/.quotabar/settings.json`, which mixes appearance, notifications, background refresh, feed, and provider toggles ([`JSONSettingsRepository.swift`](../../Sources/Infrastructure/Storage/JSONSettingsRepository.swift)). Keep that file app-owned. Put daemon settings in a separate per-user config file. The install script can create an initial template and the user can edit it while the daemon is stopped; the Swift app must not write it. If settings later change at runtime, the daemon should be their sole writer and accept updates only through authenticated control.

## Smallest practical runtime

| Component | Use | Source |
|---|---|---|
| Tokio | Async runtime, tasks, timers, synchronization, Unix signals, and child processes | [Tokio crate documentation](https://docs.rs/tokio/latest/tokio/) |
| Axum | HTTP routing, request extraction, body limits, and graceful server shutdown | [Axum crate documentation](https://docs.rs/axum/latest/axum/), [body limit](https://docs.rs/axum/latest/axum/extract/struct.DefaultBodyLimit.html), [graceful shutdown](https://docs.rs/axum/latest/axum/serve/struct.Serve.html) |
| Reqwest | One shared HTTPS client for the five probes | [Reqwest crate documentation](https://docs.rs/reqwest/latest/reqwest/), [TLS backends](https://docs.rs/reqwest/latest/reqwest/tls/index.html), [client timeouts](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html) |
| Serde and serde_json | DTOs, provider responses, and small JSON config files | [Serde](https://docs.rs/serde/latest/serde/), [serde_json](https://docs.rs/serde_json/latest/serde_json/) |

Use Reqwest’s Rustls backend explicitly. Its TLS documentation distinguishes Rustls from native TLS, which uses OpenSSL on Linux. Reuse one `reqwest::Client` so its connection pool is shared. Set total and connect timeouts; Reqwest documents that its total timeout covers connecting through completion of the response body and defaults to none. Confirm the exact feature names against the pinned Reqwest release, since the TLS feature set changes over time.

Use only the Tokio features the binary needs: runtime, macros, networking, synchronization, timers, signals, and process/IO support for `/usr/bin/security` and `sqlite3`. Keep Axum’s default HTTP serving path instead of writing a TCP/HTTP parser or adding Hyper directly. Keep config parsing to Serde JSON. Do not add `clap`, a settings framework, a service-manager crate, `tower-http`, or an SQLite binding until a concrete need justifies one. If enforcing a deadline across request-body reads needs middleware, add only the specific Tower timeout layer required and map its timeout to an HTTP response.

For cross-process locking on the supported Unix targets, Rust’s `std::fs::File` lock APIs are stable since Rust 1.89; the standard library documents their Unix implementation in terms of `flock` ([`File::lock`](https://doc.rust-lang.org/std/fs/struct.File.html#method.lock), [`File::try_lock`](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock)). Require Rust 1.89 or newer. A daemon-wide lock file plus non-blocking `try_lock` can reject a second manually launched instance. If one daemon owns all probes, no separate lock crate or per-provider process lock is needed. A file lock is advisory, so it coordinates only processes that take the same lock.

Add `getrandom` only if/when the daemon creates a random local control token. No FFI, plugin system, Docker, or broad platform abstraction is needed for the base runtime.

## HTTP behavior, cache, and security

### Fast cached reads

Return the latest snapshot from memory immediately. When one or more provider snapshots are stale, enqueue a background refresh and return the current values with their capture times and age. Coalesce concurrent stale reads so each provider has at most one refresh in flight. Run the five provider refreshes concurrently, but serialize refreshes for the same provider.

Warm the cache with one refresh after the listener starts. Preserve the last successful snapshot when a refresh fails and report its age and a redacted error. Use a monotonic clock for cache expiry and wall-clock timestamps for DTOs. Start with in-memory snapshots; persist only provider cooldown or stale-observation state where a restart could violate an API limit. Muse’s five-minute cooldown is a specific case to consider. A process lock plus atomic state-file replacement is simpler than a new lock dependency.

Keep GET bounded by construction: it must not await a provider request. This makes the service comfortably faster than the Pi 5-second and 8-second deadlines. Add a lightweight `/healthz` that reports whether the listener is ready and whether initial refresh is underway. Listener readiness is not quota freshness. Axum’s graceful shutdown support can stop accepting work on SIGINT/SIGTERM; Tokio provides Unix signal handling ([Tokio signals](https://docs.rs/tokio/latest/tokio/signal/)).

Keep `POST /hooks/session-start` and `POST /hooks/prompt` as cached-only handlers if Claude Code hooks remain in scope. Add a distinct `POST /refresh` later for an explicit refresh request. Protect that mutating route with local control authentication and have it return promptly, for example with an accepted/refreshing result; it must not bypass provider cooldowns.

### Loopback and browser requests

Bind only to `127.0.0.1`; do not bind `0.0.0.0` or a LAN interface. The current server pins the listener to IPv4 loopback ([`QuotaHTTPServer.swift`](../../Sources/Infrastructure/Server/QuotaHTTPServer.swift)). Validate the complete `Host` authority, including the configured port, against exact allowed values such as `127.0.0.1:<port>` and `localhost:<port>`. The current implementation strips the port before checking the hostname, so it does not enforce the configured port.

Host validation helps against DNS rebinding: a browser request made to an attacker-controlled hostname still carries that hostname as its HTTP authority ([RFC 9110 Host and authority](https://www.rfc-editor.org/rfc/rfc9110.html#name-host-and-authority)). The Fetch standard lists `Host` as a forbidden request header, so page JavaScript cannot simply replace it with `localhost` ([WHATWG Fetch forbidden headers](https://fetch.spec.whatwg.org/#forbidden-request-header)). Host validation is not client authentication, though. It does not stop another local process, or every cross-site POST.

Send no CORS allow headers. For native-only POST routes, require the expected JSON content type and reject any `Origin` header and cross-site `Sec-Fetch-Site` values. Do not reject every Fetch Metadata header: a local Node fetch POST check on November 3, 2026, sent `Sec-Fetch-Mode: cors` without `Origin` or `Sec-Fetch-Site`. Mode alone does not identify a browser. Test the exact native clients before defining a stricter allow policy. Reject browser preflights rather than enabling CORS. Browser forms can make simple POSTs without a CORS preflight, while custom headers trigger preflight under Fetch’s CORS rules ([WHATWG Fetch CORS](https://fetch.spec.whatwg.org/#cors-protocol-and-credentials), [W3C Fetch Metadata](https://www.w3.org/TR/fetch-metadata/)). If a browser UI is ever added, define and allow its exact origin explicitly rather than broadly allowing localhost variants.

Loopback is not per-user isolation. A different local account or process that can connect to the port can read an unauthenticated feed. The simplest v1 can accept that risk on a single-user personal machine, with Host and browser checks. If the machine is shared or quota data must be private from other local accounts, require a token on GET as well, and teach each agent client to read it from the user-only token file.

### Future local control authentication

For a native SwiftUI or agent control client, the simplest application-level option is a 256-bit random token stored in a user-only file, sent in a custom header such as `X-QuotaBar-Control-Token`. Create the parent directory as user-only and the token file as mode `0600`; use the optional `getrandom` crate rather than custom randomness code. Never put the token in a URL, query string, feed body, or log. The header avoids browser simple-form submission and causes a cross-origin browser fetch to preflight; do not approve that preflight. Reject `Origin` and cross-site Fetch Metadata on these native-only routes; permit the mode-only metadata sent by native Node fetch. This protects against other local UIDs and web pages that cannot read the token, but not against hostile code running as the same UID.

A Unix-domain socket with filesystem permissions avoids an HTTP bearer token, but it adds client and transport differences across Linux/macOS and is less convenient for the existing HTTP consumers. Peer-credential checks are OS-specific and add an unnecessary transport-specific implementation for the current HTTP clients. Do not use a token in the URL or rely on Host validation alone for control actions. Keep control routes disabled until the token file and client access are designed together.

### Timeouts and body bounds

Use a shared Reqwest client with a 10-to-15-second total provider request timeout and a shorter connect timeout. Set finite bounds for all upstream response bodies: reject an oversized declared `Content-Length`, then stream chunks and stop once the cap is exceeded. A 256 KiB to 1 MiB JSON cap is ample for these quota responses; keep the smaller existing Muse cap unless a captured response shows otherwise. Current Muse explicitly caps responses at 256 KiB, while the other provider probes do not share a common response-body cap ([Muse probe](../../Sources/Infrastructure/Muse/MuseUsageProbe.swift), [network client](../../Sources/Infrastructure/Shared/URLSessionNetworkClient.swift)). Never buffer an unbounded server response before checking its size.

Set an inbound hook/control JSON limit, for example 1 MiB, and a short overall request deadline so a slow client cannot hold a connection indefinitely. Axum’s `DefaultBodyLimit` applies to common extractors including JSON and defaults to 2 MiB; it does not cover code that consumes raw bodies directly ([Axum `DefaultBodyLimit`](https://docs.rs/axum/latest/axum/extract/struct.DefaultBodyLimit.html)). Use a bounded extractor or a global request-body limit if raw `Body` is used. Keep connection and handler timeouts distinct from upstream timeouts. Log neither hook prompts nor credentials.

## Claude Code hook compatibility

The first-party Claude Code hook reference says HTTP hooks receive the event’s JSON as the POST body and return the same JSON output format as command hooks. Hook input includes common fields such as `session_id` and `hook_event_name`; `SessionStart` and `UserPromptSubmit` add event-specific fields. The output for context uses `hookSpecificOutput` with the matching `hookEventName` and `additionalContext`. HTTP hooks must return a successful response with an empty body or a JSON object; an invalid non-empty body or non-2xx status is a hook failure ([Claude Code hooks reference](https://code.claude.com/docs/en/hooks)).

The current handler reads `session_id` and returns the event-specific `hookSpecificOutput` envelope ([`QuotaHooks.swift`](../../Sources/Infrastructure/Server/QuotaHooks.swift)). The README currently configures SessionStart through a command that calls `curl`, and UserPromptSubmit as an HTTP hook ([`README.md`](../../README.md)). Keep that known working split for the first migration; confirm event/type support against the exact Claude Code version before changing it. A cached-only hook response is important because UserPromptSubmit runs before model processing and can stall the prompt. Do not use hook POSTs to trigger quota refreshes.

## Credential and Keychain boundaries

### Files and subprocesses

Resolve each CLI login from the service user’s home and explicit XDG environment, not from the menu app’s working directory or interactive shell PATH. A LaunchAgent does not necessarily inherit shell exports, so include stable `HOME`/XDG values where needed and use absolute executable paths. Preserve file permissions, preserve unknown JSON fields when rotating OAuth records, and write replacements atomically.

Claude’s Keychain item is a special case. This repository records that `Claude Code-credentials` trusts `/usr/bin/security`, so it deliberately avoids `SecItem*` and uses that executable ([`CLAUDE.md`](../../CLAUDE.md)). Apple’s maintained [`security(1)` source](https://github.com/apple-oss-distributions/Security/blob/main/SecurityTool/macOS/security.1) documents interactive mode reading commands from stdin, `find-generic-password -w` writing the secret to stdout, and `add-generic-password -X` accepting hex data. Keep the existing rule: invoke `/usr/bin/security` by absolute path; do not put credential bytes in argv; pass write commands over stdin; capture stdout privately; discard stderr; set a timeout; never log either stream.

Codex refreshes also write rotating credentials back to `~/.codex/auth.json`. Atomic replacement and a daemon-owned lock avoid partial writes and duplicate daemon writes. They cannot coordinate with a CLI that ignores that lock, so concurrent CLI re-login versus token refresh remains a race to test. Do not assume that a daemon-wide lock protects writes made by Claude Code, Codex, or Muse.

### Muse Keychain access is not the Claude route

Muse currently reads the generic-password item with service `ai.meta.dev.credentials` and account `meta`, using `SecItemCopyMatching` plus `kSecUseAuthenticationUIFail` ([`MuseCredentialLoader.swift`](../../Sources/Infrastructure/Muse/MuseCredentialLoader.swift)). Apple’s [`SecItem.h`](https://github.com/apple-oss-distributions/Security/blob/main/keychain/headers/SecItem.h) defines `kSecUseAuthenticationUIFail` to return `errSecInteractionNotAllowed` when authentication UI would be needed; `kSecUseAuthenticationUISkip` instead silently skips items that require UI.

A practical Rust no-UI route exists, but it is platform-specific: the [`security-framework` crate’s `ItemSearchOptions`](https://docs.rs/security-framework/latest/security_framework/item/struct.ItemSearchOptions.html) can match a generic-password item by service/account and call `skip_authenticated_items(true)`. Its [source](https://github.com/kornelski/rust-security-framework/blob/main/security-framework/src/item.rs#L357-L368) maps that option to Apple’s `kSecUseAuthenticationUISkip`. That avoids prompting, but it skips a protected item rather than returning the same failure mode as Muse’s current `UIFail` query. Exact `UIFail` semantics require a lower-level Security.framework query. The wrapper uses native platform bindings internally. That does not violate the architectural choice to avoid a Swift-to-Rust FFI bridge. Keep this dependency target-specific rather than pulling it into the Linux base runtime.

Do not assume `/usr/bin/security` is a silent replacement for Muse. The Muse item may have a different access-control list, and a new `quotabard` executable’s signing identity may not inherit the Swift app’s access. Access through both a Rust Security.framework binding and the CLI is unverified on the real machine. Before supporting Keychain-backed Muse, perform a no-UI read test as the installed LaunchAgent identity and record whether the exact item is readable without a prompt. If no safe no-UI route works for the installed identity, keep Muse’s file-backed login supported and report Keychain-backed login as unavailable rather than prompting unexpectedly. Do not retire the current Mac engine until the owner's actual Muse credential backend works.

Use a per-user LaunchAgent, not a root LaunchDaemon, for macOS credentials. It runs as the logged-in user and has the best chance of seeing that user’s login Keychain. Still handle locked, missing, denied, and UI-required items as normal credential errors. Do not assume the Keychain is always unlocked or that a background process can display a consent prompt. If the Swift app later adopts App Sandbox, re-check file access and Keychain entitlements; the current [`entitlements.plist`](../../Sources/App/entitlements.plist) contains network client/server entitlements, not the App Sandbox entitlement. Apple’s [Keychain Services](https://developer.apple.com/documentation/security/keychain_services) and [App Sandbox](https://developer.apple.com/documentation/security/app_sandbox) docs describe those separate access controls.

## Per-user service lifecycle

### Linux

Install a user unit under `~/.config/systemd/user/quotabar-daemon.service`. A small unit can use `Type=exec`, an absolute executable path, `Restart=on-failure`, a short `RestartSec`, and `StartLimitIntervalSec`/`StartLimitBurst` to prevent a tight crash loop. Enable it under the user’s `default.target`. systemd documents the per-user unit search path, service restart behavior, and unit start-rate limits ([`systemd.unit`](https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html), [`systemd.service`](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)).

A user service is the right identity boundary for reading that user’s CLI files. It need not wait for the network to exist; probes should fail and retry without preventing the listener from becoming ready. Use the service manager’s journal for stdout/stderr. The installer should not use `sudo` or change system-wide units.

Leave lingering off by default. `loginctl enable-linger` starts a user manager at boot and keeps it after logout ([`loginctl`](https://www.freedesktop.org/software/systemd/man/latest/loginctl.html)). That can be a deliberate deployment choice if agents need quotas before an interactive login, but it changes the process lifetime and may require authorization. Do not run or approve it as part of this research or an automatic install.

### macOS

Install a plist under `~/Library/LaunchAgents` with an absolute `ProgramArguments` path, a stable label, `RunAtLoad`, `KeepAlive`, and explicit stdout/stderr destinations. Bootstrap it into `gui/$(id -u)`. A per-user agent starts in the user’s GUI login session; Apple’s launchd guide says launchd loads per-user agents at login and sends them `SIGTERM` at logout ([Creating Launch Daemons and Agents](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)). This lets the daemon outlive QuotaBar.app while keeping it in the user context needed for CLI files and the login Keychain.

Do not make QuotaBar.app the daemon’s parent. An app-managed child stops or becomes difficult to supervise when the app quits, and couples service lifetime to UI startup. Do not add `SMAppService` or a privileged helper for the first personal release. The requested explicit LaunchAgent install script is enough and is easier to inspect and remove. Keep install/update/removal out of the app itself.

### Install, update, and remove

Provide explicit user-run scripts, not an auto-updater. The Linux install script should put the binary in `~/.local/bin/quotabar-daemon`; the macOS script can use `~/Library/Application Support/QuotaBar/bin/quotabar-daemon`. Create a config template only when absent, install the service definition, reload the manager, and start the service. The update script should stop the service, replace the binary atomically, then restart it. The removal script should stop and unregister the service and remove its binary; preserve config, cached quotas, and logs unless the user separately asks to delete them.

On Linux, the scripts can use `systemctl --user daemon-reload`, `enable --now`, and `disable --now` ([`systemctl`](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html)). On macOS, they can use `launchctl bootout` before replacing/removing the executable and `launchctl bootstrap` after installation ([Apple `launchctl(1)` source](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchctl.1)). Keep all paths in the current user’s home; do not install to `/Library/LaunchDaemons`, `/usr/local`, or a system package database.

A `Type=exec` or loaded LaunchAgent is not proof that quota data is fresh. The daemon should expose listener readiness separately from provider status. On a crash, rely on systemd restart policy or launchd `KeepAlive`; avoid a second custom watchdog. Surface startup and repeated-crash failures in service-manager logs and a plain health endpoint.

## Build and distribution

Build separate release artifacts for `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu`. Rust lists both as Tier 1 targets; its platform page lists macOS ARM64 as macOS 11+ and x86_64 GNU/Linux as glibc 2.17+ ([Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html)). The project’s current SwiftUI app requires macOS 15 ([`Project.swift`](../../Project.swift)); decide whether the standalone daemon should align with that or support the Rust target’s lower OS minimum.

For Ubuntu 26.10, build the GNU/Linux artifact on an Ubuntu 26.10 host or runner. This needs no Docker. Building on the target release avoids accidentally linking to a newer glibc than the declared deployment baseline. If older Ubuntu releases become supported, build and test against the oldest supported glibc baseline; do not infer compatibility solely from Rust’s target tier. A musl artifact is unnecessary for the stated Ubuntu target and brings a different libc/runtime path.

Commit `Cargo.lock` for this application binary and build release artifacts with `cargo build --locked --release --target <target>`. Cargo’s `--locked` option fails rather than changing the lockfile ([Cargo build options](https://doc.rust-lang.org/cargo/commands/cargo-build.html#manifest-options)); publish one archive per target with checksums. Do not require Homebrew, Docker, FFI libraries, or a cross-platform packaging framework.

Local and inspected-host checks, without installing or building:

- local Mac: macOS 26.6.2, `arm64`, stable Rust/Cargo 1.98.1
- local installed Rust targets: `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`
- local `/usr/bin/clang` is present, but no Linux cross-linker such as `x86_64-linux-gnu-gcc` or `ld.lld` was found
- the inspected Linux host is Ubuntu 26.10 x86_64 with Rust/Cargo 1.99.0 and a running systemd user manager; `Linger=no` ([Linux credential research](rust-migration-linux-credentials.md))
- the macOS arm64 target is native; an installed Linux standard library target does not by itself provide a Linux linker or sysroot

No build was attempted. Linux credentials and one Cursor endpoint response are observed on the separate Ubuntu host, but no Rust build or release artifact has been verified.

For macOS distribution beyond a personal machine, sign and notarize the daemon artifact and document Gatekeeper behavior. The current project sets an ad-hoc signing identity (`CODE_SIGN_IDENTITY = -`) ([`Project.swift`](../../Project.swift)); that does not establish a signed/notarized distribution path. Apple’s [notarization guidance](https://developer.apple.com/documentation/security/notarizing_macos_software_before_distribution) applies to software distributed outside the Mac App Store. If the SwiftUI app is later sandboxed, reassess the LaunchAgent and credential-file model rather than assuming its current permissions transfer to a standalone daemon.

## Key recommendations

- Use one crate with Tokio, Axum, Reqwest/Rustls, Serde, and serde_json.
- Keep one daemon instance per user; use a standard-library lock file only as a duplicate-process guard.
- Make GET return cached state immediately and coalesce stale refreshes in background tasks.
- Keep provider-specific cache and cooldown rules; do not let GET or a future force route bypass them.
- Keep loopback binding, validate exact Host and port, apply browser-origin checks to POSTs, and do not enable CORS.
- Add a random `0600` token and custom header when authenticated control is introduced; decide then whether GET also needs a token.
- Keep Claude on `/usr/bin/security` with secrets passed over stdin for writes. Do not generalize that route to Muse.
- Treat Muse Keychain access from a new Rust service identity as unverified. A maintained target-specific Rust wrapper is acceptable, but its no-UI semantics and installed-identity access must be tested before Mac cutover.
- Install with explicit per-user scripts: systemd user service on Linux, GUI-session LaunchAgent on macOS; keep the daemon independent of the app.
- Build the Linux binary on Ubuntu 26.10 and the macOS binary natively on arm64; publish locked, per-target artifacts.

## Remaining design decisions

1. Should unauthenticated loopback GET be acceptable on single-user systems, or should the install script create a token required by GET and control routes?
2. Can the installed macOS daemon read Muse's current Keychain-backed login without UI? Target-specific Rust bindings are acceptable; exact error semantics and actual access remain unverified.
3. Linux Cursor uses the verified CLI auth file, not a database. The macOS desktop database remains a fallback to preserve when needed.
4. Should ordinary quota snapshots survive daemon restarts, beyond persisting rate-limit/cooldown state that protects providers?
5. Should the standalone macOS daemon support macOS 11+ or align with the app’s current macOS 15 minimum?
6. Should app-initiated refresh be a later authenticated `POST /refresh`, and which agent/app clients need access to it?
