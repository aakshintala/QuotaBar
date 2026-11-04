# Rust daemon migration plan: owner review

Review of [rust-daemon-migration.md](rust-daemon-migration.md) against source baseline `e12da3525b94561afc3f68ec4eeba59e82762941`, the four research notes it cites, the current Swift implementation, and the two feed consumers. No code was changed and no tests or provider requests were run.

This review supersedes nothing in [the earlier confirmation review](rust-daemon-migration-review.md). That review checked that three prior findings were answered. This one asks whether the answers are worth their cost, and finds two correctness cases the plan does not cover.

## Verdict

Direction: sound. Plan as written: over-built.

One Rust crate, localhost HTTP with no Swift↔Rust FFI, Linux first, cache-first reads, a single active poller per host, and no distributed coordinator are the right calls. The provider behaviour table carries the hard-won lessons and should stay.

Most of the runtime contract's machinery exists to support one decision: persisting sanitized snapshots for every provider and restoring them safely across restarts. Drop that decision and the hidden restored state, credential fingerprint binding, monotonic age anchoring, full-duration guard rearming, and corrupt-state operator repair all go with it. That is roughly a third of the contract and a large share of the verification matrix.

Two findings are correctness issues regardless of the persistence decision: quota status must be recomputed at render time, and a monotonic freshness TTL is wrong across system sleep.

Recommended outcome: amend the plan before implementation. The amendments are listed as concrete edits at the end.

## Keep unchanged

- Cache-first `GET /quotas`. The current feed can wait 20 seconds in [`QuotaFeedService`](../../Sources/Infrastructure/Server/QuotaFeedService.swift), which exceeds the Pi client's 5-second and 8-second deadlines recorded in [consumer findings](../research/rust-migration-consumers.md). This is the clearest user-visible win in the whole migration.
- The daemon computing quota status, so the menu app, Pi, and the shell client cannot diverge on pacing.
- Persisting a cooldown *before* an attempt, honouring a valid `Retry-After`, and never letting a forced refresh bypass either.
- Keeping observation time (`capturedAt`) immutable and separate from response time.
- Never inventing a full quota for a missing, idle, expired, or reset window.
- A separate `daemon.json` with the daemon as sole writer. Sharing `settings.json` with the whole-file Swift writer in [`JSONSettingsRepository`](../../Sources/Infrastructure/Storage/JSONSettingsRepository.swift) would lose updates.
- Hooks reading published state only, with bounded session retention and no prompt bodies retained or logged. The current session map is never evicted and maps malformed IDs to one empty session in [`QuotaHooks`](../../Sources/Infrastructure/Server/QuotaHooks.swift); fixing both is right.
- One poller at cutover and no shadow-polling of undocumented endpoints.
- Keeping the bundle ID, the status-item repair in [`StatusBarIconDriver`](../../Sources/App/StatusBarIconDriver.swift), and notifications in the Mac UI.
- Treating Muse's Keychain backend as a hard Mac cutover gate rather than silently shipping it unavailable.

## Correctness findings

### 1. Quota status, age, and reset text are functions of now, not of capture

Severity: must fix in the plan text.

[`UsageQuota.status`](../../Sources/Domain/Provider/UsageQuota.swift) is pace-aware. It derives elapsed time from `resetsAt` and the window duration in [`QuotaType`](../../Sources/Domain/Provider/QuotaType.swift), then applies the burn-rate rules in [`QuotaStatus`](../../Sources/Domain/Provider/QuotaStatus.swift). The feed mapper also derives `ageSeconds` and `resetText` at render time in [`QuotaFeedDTO`](../../Sources/Infrastructure/Server/QuotaFeedDTO.swift).

In Swift, status is a computed property. It changes as wall time advances, with no new observation. A daemon that stores a status string in its snapshot freezes it at capture, so a bucket that was `healthy` at capture can stay `healthy` in the feed long after the pace math says `critical`. `resetText` would also be frozen at whatever it said when captured.

The plan's wire contract carries `status`, `ageSeconds`, and `resetText` per quota but never says where they are computed.

Amendment: the stored observation holds `percentRemaining`, quota kind and window duration, `resetsAt`, and the balance and unit fields. Effective bucket status, `ageSeconds`, `resetText`, and expired-bucket omission are computed during feed serialization, at response time. State this once in the runtime contract. It also removes any need to persist a status.

### 2. A monotonic freshness TTL does not survive system sleep

Severity: must fix in the plan text.

The plan sets freshness TTL to 1,200 monotonic seconds and anchors restored age to monotonic time. Monotonic clocks do not measure elapsed wall time across suspend on either target:

- Rust's `std::time::Instant` on Apple platforms uses `CLOCK_UPTIME_RAW`, which "does not increment while the system is asleep" (verified in the Rust 1.98.1 standard library source, `library/std/src/sys/time/unix.rs`).
- On Linux it uses `CLOCK_MONOTONIC`, which likewise excludes suspend time; `CLOCK_BOOTTIME` is the variant that includes it. Inferred from the same source plus the `clock_gettime` man page; not observed on the Ubuntu host.

So after a laptop sleeps overnight, the daemon believes a nine-hour-old observation is seconds old and reports it `fresh`. The scheduler also sits out the remainder of whatever interval it was sleeping through, and a persisted cooldown has effectively not elapsed.

The current app handles this deliberately: [`QuotaMonitor`](../../Sources/Domain/Monitor/QuotaMonitor.swift) pauses while the display is asleep and refreshes on wake, and ages snapshots with wall-clock `Date`.

Amendment: derive freshness and age from wall-clock `capturedAt`. Reserve monotonic time for in-flight timeouts and attempt deadlines, where sleep behaviour does not matter. This also simplifies the cooldown design in finding 4.

Related behavioural note, not a bug: dropping the power-state pause means the Mac daemon polls every interval with the display asleep, and the battery cadence doubling in `QuotaMonitor` goes away. At a 600-second cadence that is a reasonable trade for an always-available feed, but the plan should state it rather than lose it silently.

### 3. Nothing drives Mac notifications or the status icon between popover opens

Severity: must specify before Phase 4.

Phase 4 has the popover fetch state on open and send a refresh control. But quota alerts and the status item must update while the popover is closed. Today `QuotaMonitor` owns both: it tracks previous status per provider and calls the alerter on change.

After the cutover, some component in the Mac app has to poll `/quotas` on a timer and diff observations. That poll is cheap because the endpoint is cache-first, but it has to be designed: cadence, and deduplication keyed on `capturedAt` so that a reconnect after a daemon restart does not re-alert for an old worsening. The consumer note already flags this ("Deduplicate notifications using observations rather than each UI poll. Do not notify simply because a connection recovers").

Amendment: add a Phase 4 deliverable: a UI poll loop (suggested 60 seconds) over `/quotas`, driving the status item and notifications, deduplicating on `(providerId, capturedAt, status)`. Pair it with the manual-refresh flow: `POST /refresh` returns immediately, so the popover must watch for `refreshState` returning to `idle` or `capturedAt` advancing, and show progress meanwhile. Neither is specified at present.

### 4. Full-duration guard rearming on restart can starve a provider forever

Severity: simplify; the current rule has a liveness failure.

"On restart, conservatively rearm the full most recently persisted guard duration from the new process's monotonic start" means a one-hour `Retry-After` restarts from zero on every daemon restart. Restart more often than hourly and that provider is never polled again. The plan's own update scripts stop and restart the service, and launchd and systemd restart on failure, so this is not a stretch case.

The justification is that a wall-clock jump cannot shorten a guard. But the guard protects against the daemon's own polling, not against the machine's owner, who can delete the state directory. It is not a security boundary, and treating it as one buys the operator-repair flow, schema-version handling, and the "fail closed forever on corrupt state" rule.

Amendment: persist one wall-clock `notBefore` per provider. On startup, wait `notBefore - now`, floored at zero. On corrupt or unreadable cooldown state, log it, apply the base cooldown from startup, and rewrite the record. Keep the two rules that actually carry the provider lesson: persist before the attempt, and let `Retry-After` extend the base.

What you give up: a forward clock jump can shorten a cooldown once. NTP corrections on either host are seconds, not hours, and the consequence is one early request that may get a 429 and a new `Retry-After`.

### 5. Money encoding on the wire is unspecified

Severity: settle in Phase 0.

Swift `Decimal` encodes as a JSON number in [`QuotaFeedDTO`](../../Sources/Infrastructure/Server/QuotaFeedDTO.swift), and the Pi client types balances as `number`. The plan requires "exact decimal `totalCost`" and says not to drop or round cost details, but the named stack (Serde, `serde_json`) has no decimal type. Defaulting to `f64` rounds, which contradicts the requirement as written.

Options: serialize money as decimal strings in the v1 contract, or add a decimal crate with `serde-with-arbitrary-precision` equivalent behaviour. Since both consumers are being revised in the same cutover and exact wire parity is explicitly not required, strings are the smaller move. Whichever is chosen, name it in the contract and cover `balanceRemaining`, `balanceUsed`, `balanceCap`, and `costUsage.totalCost` and `budget` uniformly.

### 6. `status: stale` duplicates `freshness` and breaks the status vocabulary

Severity: simplify.

The proposed contract adds `freshness` (`unknown`, `fresh`, `stale`) and also makes provider `status` report `stale` when only stale data remains. That is the same fact in two fields, and every client then has to special-case a status value that is not a severity.

It also collides with existing behaviour: `QuotaStatus(feedKey:)` returns nil for unknown strings, so hook change detection in [`QuotaHooks`](../../Sources/Infrastructure/Server/QuotaHooks.swift) would silently drop a `stale` provider rather than report it.

Amendment: `status` is always a severity (`healthy`, `warning`, `critical`, `depleted`) or `unknown`. `freshness` is the only place staleness is expressed. Stale providers keep their quotas and their severity; clients render severity plus the stale marker.

## Simplification findings

### 7. Do not persist snapshots for all providers

This is the largest available cut.

Current state: only Muse persists an observation, under `~/.quotabar/muse`, together with its persisted cooldown. Claude's five-minute cache and rate-limit state are process-local, as recorded in [provider gotchas](../research/rust-migration-provider-gotchas.md). The runtime note's recommendation was "Start with in-memory snapshots; persist only provider cooldown or stale-observation state where a restart could violate an API limit", and it left snapshot persistence as an open decision. The roadmap resolved that decision in the most expensive direction without recording a reason.

Generalizing persistence is what forces:

- restored observations held in a hidden, unpublished state;
- a background noninteractive credential validation step at startup;
- stable account fingerprint binding, with an exact-credential SHA-256 fallback;
- a credential generation comparison before publishing every probe result;
- monotonic anchoring of a restored age;
- persisted invalidation of expired buckets and cost rows so a clock rollback cannot resurrect them;
- the restored-observation test block covering first GET, absent, unreadable, denied, changed, and auth-rejected credentials, stable bindings, and providers without stable IDs.

Recommendation: persist cooldowns for all providers, snapshots for none. Keep observations in memory. On startup every provider is `unknown` with empty quotas, and the scheduler refreshes immediately subject to the persisted cooldown.

Cost: after a daemon restart during a provider outage, there is no last-known reading until a probe succeeds. Restarts are service updates and logins, so this window is short and rare.

If that cost is not acceptable, the middle option is to persist snapshots but drop the credential-binding machinery: publish them as `stale` with their original `capturedAt` and age, and replace them on the first successful probe. For a single-user machine the harm from a briefly stale reading belonging to a previous login is that the user glances at a wrong number for a few minutes, clearly labelled stale. That is a much cheaper control than fingerprint binding, and it matches what `AIProvider` does today for most providers, where a removed credential can leave an old snapshot visible.

Muse is the exception either way: it already has a deliberate persisted-observation contract with an account-change signal, and its probe reports itself always available so credential errors surface. Keep that behaviour for Muse specifically rather than generalizing it.

### 8. Per-provider cooldowns instead of a universal 300 seconds

Only Claude and Muse have an observed 300-second guard. Applying 300 seconds to every provider, persisted before the attempt and immune to force, means a failed attempt caused by the network being down blocks even a manual refresh for five minutes on providers with no observed limit. It also changes the feel of the refresh button for Codex, Cursor, and OpenCode Go, which today re-probe immediately when forced.

Note the provider note *proposed* the opposite direction for unguarded providers: a 15-minute minimum. Neither number is evidence. What is evidence is that only two providers have shown they need a guard.

Recommendation: one constant per provider. Claude 300, Muse 300, the other three 60 (the existing `QuotaMonitor.minimumSnapshotAge`). `Retry-After` overrides all of them. This is a five-entry table, not a framework, and it keeps the safety property where it was earned.

### 9. The controller surface should be three named operations

Phase 4 says "authenticated control operations" without enumerating them, which leaves the whole Mac settings design open at the point where it is most expensive to discover.

The app needs exactly three: set a provider enabled or disabled, set the refresh interval, and request a refresh. Name them now, with their request and response shapes, in Phase 0's contract freeze. Everything else in [`AppSettings`](../../Sources/App/Settings/AppSettings.swift) (theme, notifications) stays local, and the two provider config cards become read-only guidance driven by daemon error codes.

Two related decisions belong with it:

- Port. If the port stays configurable, the Mac app must read `daemon.json` to locate the daemon, and a port change needs a rebind-or-restart rule. Fixing 8787 as a constant in both binaries and deleting the port control from Settings is simpler and loses nothing the owner uses. The feed-enabled toggle also has to go, since the daemon is now the app's only data source; the consumer note already calls this out.
- Interval semantics. Today the picker offers Off, 1, 5, 10, and 15 minutes via [`RefreshInterval`](../../Sources/Domain/Monitor/RefreshInterval.swift), and "Off" means refresh only on popover open. Under a daemon, "Off" should mean no background polling but still refresh on demand. Say which.

### 10. Trim the HTTP request-filter matrix

Keep: bind to `127.0.0.1` only, exact `Host` authority including port, no CORS headers, JSON content type required on POST, reject any `Origin`, bounded bodies, bounded connection time.

Drop: the `Sec-Fetch-Site` and `Sec-Fetch-Mode` case analysis. It is the part most likely to reject a legitimate native client, as the observed Node `Sec-Fetch-Mode: cors` case already shows, and it defends the two routes with the least at stake. Read-only feed access from a web page is already blocked from being *read* by the absence of CORS headers, and DNS rebinding is blocked by the `Host` check. The mutating routes get the random `0600` token, which is the real control.

That also removes several cases from the Phase 3 security test list. Keep the real-loopback tests for wrong Host, `Origin` present, oversized body, wrong method, and slow or incomplete requests.

### 11. Let the scheduler be the only thing that schedules

The runtime contract says `GET /quotas` returns published state and also that it should "schedule eligible validation and refresh work", and that "requests for a provider already refreshing join that single in-flight attempt". Joining is vocabulary from `QuotaMonitor.refresh`, where callers actually wait. A cache-first GET does not join anything.

Pick one trigger model (see decision D2 below) and state it plainly. If polling is continuous, GET is a pure read and needs no scheduling side effect. If polling is on demand, GET enqueues a refresh when the observation is older than the freshness TTL and none is in flight, then returns. Mixing both produces a handler with side effects that are hard to test and easy to double-fire.

## Phasing findings

### 12. Move the Muse Keychain spike to Phase 0

The single unknown that can invalidate the Mac cutover is whether a standalone Rust executable, running as the installed LaunchAgent identity, can read the `ai.meta.dev.credentials` Keychain item without UI. The plan correctly makes it a hard gate but schedules the answer in Phase 4, after the Linux daemon, all five providers, both clients, and the service deployment are done.

It is a small standalone test: a Rust binary using `security-framework` with `skip_authenticated_items(true)`, installed under a throwaway LaunchAgent, reading the item and logging only success or a status code. Doing it first costs an afternoon and de-risks the entire second half of the roadmap.

Two details to include in the spike, because both change the answer:

- Test after a rebuild. The project signs ad hoc (`CODE_SIGN_IDENTITY = -` in [`Project.swift`](../../Project.swift)). An ad-hoc-signed helper has no stable code identity, so a Keychain ACL grant may not survive the next build. If the daemon needs a stable identity, that is a packaging decision worth knowing about in Phase 0.
- Test with the login Keychain in its normal state and after a fresh login, since a LaunchAgent can start before the Keychain is unlocked.

If the spike fails, the fallback in the runtime note (file-backed Muse login supported, Keychain-backed reported unavailable) becomes a product decision for the owner, made with most of the budget still unspent.

### 13. Phase 1 should not start with Codex credential handling

Phase 1 is meant to prove the executable, the cache-first scheduler, and the feed contract. Codex is the provider with proactive refresh after eight days, refresh-token rotation, an atomic credential rewrite, and a known form-encoding bug to avoid reproducing. That is the largest credential surface of the five.

OpenCode Go is one authenticated GET with a read-only key file and no refresh path. It proves the same skeleton. Alternatively, keep Codex but scope Phase 1 explicitly to "read credentials, parse usage, no token refresh", with 401 recovery deferred to Phase 2. Either way, write the boundary down so the proof stays narrow.

### 14. Deploy the service skeleton before completing all five providers

As phased, nothing runs on the Linux box until Phase 3, after all five providers are ported. That puts every deployment unknown (user unit environment, HOME and XDG resolution under systemd, journal logging, binary replacement, the `Linger=no` behaviour) at the end of the Linux work, where a surprise invalidates assumptions baked into the provider code.

Install the Phase 1 binary as the real user service with one provider enabled, and add providers into a running service. The completion gates do not change; only the order does. It also gets the owner a working feed on the Linux box early, which is the stated goal of the migration.

### 15. Extract shared golden fixtures before porting parsers

The plan says to carry the provider regressions from `Tests/InfrastructureTests/{Claude,Codex,Cursor,OpenCode,Muse}/` and to "compare parser fixtures offline" at Mac cutover instead of shadow-polling. The captured response shapes currently live inline in Swift test files, so a Rust port would retype them and the comparison would be between two hand-copied fixtures.

Add a Phase 0 or early Phase 1 step: extract the captured provider responses into JSON fixture files that both the Swift suites and the Rust tests read. Then parity means the same bytes produce the same quotas in both implementations, which is the strongest offline evidence available and cheap to obtain while the Swift engine still exists. After Phase 5 deletes the Swift probes, that oracle is gone.

## Missing cases and smaller gaps

### Two-host request volume

Independent daemons are the approved design, and no coordinator is the right call. But two daemons polling every 600 seconds is roughly double the account-level request rate, and nothing prevents them landing within seconds of each other. Add per-host jitter to the scheduler (for example a fixed offset derived from the hostname, plus a small random spread). It is a few lines and needs no shared state.

Worth stating in the plan: today's default is background sync *off*, so the current request volume is close to zero unless the popover opens or a client fetches the feed. An always-on daemon on two hosts is a step change, against at least one endpoint that has returned a one-hour `Retry-After` after a single call following a quiet period. That is the strongest argument for on-demand scheduling on Linux (decision D2).

### `daemon.json` has no schema

Phase 0 freezes paths but not contents. List the fields (port or not, per-provider enabled, refresh interval, per-provider cooldown overrides if any), their defaults, and the behaviour on malformed JSON. Recommended: fail startup with a clear log line and a nonzero exit, so the service manager shows the failure, rather than silently running on defaults. The current Swift loader falls back to defaults on any decode failure, which is how a typo becomes "my settings reset themselves".

### Single-instance enforcement is left as a possibility

"A daemon advisory lock can prevent two `quotabard` processes from racing each other" is not a decision. Make it one: `try_lock` a lock file in the state directory at startup and exit with a clear message if it is held. Binding the port is not sufficient on its own, since a second instance on a different port would still race the cooldown files and double the provider requests. This matters during development, when a manually launched binary next to the installed service is the most likely way to burn a rate limit.

### Mac log growth

Linux gets the journal, which rotates. A LaunchAgent writing to `StandardErrorPath` does not. The current app rotates at 5 MB into `QuotaBar.old.log`. Either send Mac daemon logs to unified logging, or state that the file is unbounded and the install script truncates it on update. Do not leave a silently growing file in a support directory.

### Expiring read-only credentials need actionable errors

Cursor on Linux and Muse on both platforms are read-only by design: the daemon rereads the CLI's token but never refreshes it. On a Linux box where the Cursor CLI is rarely run, the access token will eventually expire and the provider will fail until the owner runs the CLI again. That is an acceptable consequence of not writing other tools' credentials, but the safe error code should carry guidance a client can print, such as "run `agent status` to refresh your Cursor login". Add per-provider remediation text to the error-code table in Phase 0. At present the contract requires "useful non-secret guidance" without defining any.

### Resolve the two dangling "check later" items now

- Relative `XDG_DATA_HOME` and `XDG_CONFIG_HOME`: the XDG basedir specification says relative values must be ignored. Decide that in the plan instead of carrying "check relative handling before finalizing" into implementation. Muse's loader already requires an absolute `XDG_CONFIG_HOME`; OpenCode's does not. Align both on "ignore non-absolute".
- macOS Cursor: pin the mechanism. Shelling out to `/usr/bin/sqlite3` against `state.vscdb`, as [`CursorUsageProbe`](../../Sources/Infrastructure/Cursor/CursorUsageProbe.swift) does today, avoids adding an SQLite crate. If a `~/.config/cursor/auth.json` exists on the Mac from the Cursor CLI, preferring it would let one code path serve both platforms; the plan allows a CLI fallback "only if its credential path is independently verified", so add that check to the Phase 0 spike list next to the Keychain test.

### Rollback wording for Linux

"Keep the previous client endpoint available" assumes there is one. On Linux there is no previous producer; the clients currently fetch a localhost port that nothing serves. Linux rollback is simply stopping the service, and the clients return to their unavailable path. Worth correcting so nobody builds endpoint-switching machinery for a case that does not exist. The Mac rollback section is accurate and should stay as is.

### Verification matrix shrinks with the cuts

If findings 4 and 7 are adopted, delete: fake monotonic-clock age anchoring, forward and backward wall-clock jump tests across restart, repeated-restart guard rearming, restored-observation tests for the eight credential states, and invalid-state operator repair. Replace them with three smaller tests: cooldown honoured across restart from a persisted `notBefore`, corrupt cooldown falls back to the base cooldown, and bucket status recomputed at render time as a `resetsAt` approaches. Add one sleep test: an observation whose wall-clock age exceeds the TTL reports `stale` regardless of monotonic elapsed time.

Keep everything else, in particular the offline fixture and mocked-HTTP rule, temporary-HOME credential tests, real-loopback HTTP tests, and running the Swift suite only through `xcodebuild test`.

## Decisions for the owner

The plan ends with two deployment confirmations (Linux lingering, and accepting Claude and Codex credential-write races). Both are the right questions. Four more decide how much of this plan survives, and each is cheaper to answer now than during implementation.

**D1. Must quota readings survive a daemon restart?**
Recommendation: no. Persist cooldowns only; start from `unknown`. This is finding 7 and removes the credential-binding machinery.
Alternative: persist readings, publish them as `stale` with their original age, and skip fingerprint binding.

**D2. Continuous polling, or refresh on demand?**
Recommendation: on demand on Linux, where the only consumers are the Pi extension, the shell client, and hooks, all of which ask when they need an answer. A stale `GET` enqueues a background refresh and returns cache immediately, so the first answer after an idle period may be stale but is labelled with its age. On the Mac, continuous polling is justifiable because the status icon and notifications should be current without anyone asking.
Trade-off: on demand means the Linux feed can be arbitrarily old if nothing has asked recently. Continuous means a permanent doubling of request rate across two hosts against undocumented endpoints.

**D3. One cooldown for all providers, or per-provider values?**
Recommendation: per provider, 300 seconds for Claude and Muse and 60 for the rest, per finding 8.

**D4. Is the feed port fixed at 8787?**
Recommendation: yes, a shared constant, and remove the port and feed-enabled controls from Mac Settings. This deletes the "how does the app find the daemon" and "what happens on a port change" problems entirely.

## Recommended amendments, as edits to the plan

1. Runtime contract: state that bucket status, `ageSeconds`, `resetText`, and expired-bucket omission are computed at serialization time from stored percent, window duration, and `resetsAt`.
2. Runtime contract: derive freshness and age from wall-clock `capturedAt`. Use monotonic time only for attempt deadlines and HTTP timeouts. Delete the monotonic TTL and restored-age anchoring.
3. Runtime contract: replace full-duration guard rearming with a persisted wall-clock `notBefore` per provider. Corrupt cooldown state falls back to the base cooldown and is rewritten. Delete the operator-repair flow and `cooldown_state_invalid`.
4. Runtime contract: persist cooldowns for all providers and snapshots for none (Muse excepted, per its existing contract). Delete the hidden restored state, credential validation at startup, and fingerprint binding. Providers start as `unknown` after a restart. *(Depends on D1.)*
5. Runtime contract: per-provider base cooldowns (Claude 300, Muse 300, others 60), with `Retry-After` overriding. *(Depends on D3.)*
6. Feed contract: remove `stale` from the `status` value set. `freshness` alone expresses staleness.
7. Feed contract: specify money as decimal strings, uniformly across balance fields and `costUsage`. *(Or name the decimal crate.)*
8. Feed contract: add an error-code table with per-provider remediation text for expired read-only logins.
9. Security: delete the `Sec-Fetch-Site` and `Sec-Fetch-Mode` rules and their test cases.
10. Scheduling: pick one trigger model and state it. Add per-host jitter. *(Depends on D2.)*
11. Config: list the `daemon.json` fields and defaults, and fail startup on malformed config. Fix the port at 8787 and remove the corresponding Mac controls. *(Depends on D4.)*
12. Startup: take a `try_lock` on a lock file in the state directory and exit if held.
13. Phase 0: add the Muse Keychain spike as an installed LaunchAgent (including a post-rebuild retest), the Mac Cursor credential-source check, the `daemon.json` schema, the three named control operations, and the money encoding.
14. Phase 1: prove the skeleton with OpenCode Go, or with Codex in read-only mode and refresh deferred to Phase 2.
15. Phase 1: extract captured provider responses into shared JSON fixtures read by both the Swift and Rust test suites.
16. Phase 1 to 2: install the systemd user service as soon as the Phase 1 binary serves a feed, and add providers into the running service.
17. Phase 4: add the Mac UI poll loop that drives the status icon and notifications, deduplicating on `capturedAt`, plus the popover's wait-for-refresh behaviour after `POST /refresh`.
18. Phase 4: pin macOS Cursor to `/usr/bin/sqlite3` against a fixture database, or to the verified CLI auth file.
19. Deployment: bound Mac daemon log growth, or send logs to unified logging.
20. Rollback: correct the Linux paragraph; there is no previous endpoint to restore.
21. Verification matrix: delete the clock-jump, guard-rearming, restored-observation, and corrupt-state-repair cases; add render-time status recomputation, persisted `notBefore` across restart, corrupt-cooldown fallback, and wall-clock staleness after sleep.

## What this review did not check

- No Rust was written or compiled. The dependency and MSRV guidance in the plan is taken on trust from [runtime deployment research](../research/rust-migration-runtime-deployment.md).
- The Linux `CLOCK_MONOTONIC` suspend behaviour is inferred from the Rust standard library source and the man page, not observed on `roundhouse-7010-plus-1.local`. The macOS half of finding 2 was verified in the locally installed Rust 1.98.1 sources.
- Provider endpoint stability, rate-limit scope, and Cursor's Linux credential behaviour after token rotation remain as the research notes describe them: observed compatibility, not a supported contract.
- The Pi and shell clients were read but not modified or run.


