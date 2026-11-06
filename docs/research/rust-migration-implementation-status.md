# Rust migration implementation status

The Rust implementation is integrated and validated offline on macOS and Linux. The [Linux native service](rust-migration-linux-service-acceptance.md) is installed, enabled and running, with five fresh providers and installed CLI cache reuse verified. The [signed Mac service](rust-migration-mac-service-acceptance.md) and public `aiquota` wrapper are also installed, but Mac provider acceptance remains partial. Codex and OpenCode Go are fresh; Claude, Cursor and Muse return `keychain_blocked`, including after narrow user-reported grants and a graceful restart. Swift is retained. Linux's `aiquota` wrapper and a native service restart have passed live checks. The Pi decoder passed 574 tests and merged separately as pi-rig PR 184 after green CI. Linux's configured local package checkout was fast-forwarded and its eight approved hashes verified. The configured Linux source's registered quota callback, decoder and formatters passed against one native-feed GET. The exact existing hook shell command was not executed because it exceeded the safe curl matcher. Running-UI reload and actual Claude Code hook acceptance remain distinct pending gates.

## Latest offline build and rollout order

The signed-Mac repair snapshot passed formatting, warning-denied Clippy, release daemon/library builds and all three example builds on both hosts. macOS passed 270 tests; Linux passed 259. The totals differ because Mac-only Keychain tests are excluded on Linux. Ordinary checks used fakes and executed no native proof or live provider request.

The final 51-file source manifest has SHA-256 `49a11b054500c47c14e7e7efcc6ff87f9763e2c5bcd979f7f2894c57bc8fb4b5` and matched both hosts. Independent scoped review approved the Linux production daemon for Standards and Spec. The production repairs are fixed. The two Mac-only proof-tool requirements—emitting the observed locked-update outcome and checking created-item destinations—were subsequently fixed and independently approved, with 271 Mac tests passing. The isolated cross-identity controller subsequently passed repair review. Its actual UID-502 human run returned `controller_failure`, without enough stage information to establish the native result. The owner authorized a separate UID-501 synthetic-only variant with stage diagnostics and preserved fixtures. Its clean cross-identity run passed after a reviewed creation-time file-permission fix. The Rust basic native proof also passed: allowed update succeeded, locked update was attempted and refused, the fixture stayed locked, and exact data checks passed. Neither synthetic run accessed subscription credentials. The owner then separately approved Mac live providers and signed service installation. Installation and a graceful restart passed, but three Keychain-backed providers remain blocked. Native renewal and persistence were not observed. See [Mac signed artifacts and native results](rust-migration-mac-signed-artifacts.md). The proof tools are not reachable from the installed Linux daemon. These offline results and Linux source approval are not deployment or owner-Keychain acceptance.

The owner approved a Linux-first rollout: checkpoint reviewed Rust with Swift retained, then install and accept Linux while isolated Mac tests continue in parallel. Do not wait for Swift retirement or PR completion to make the service usable. Mac tests follow the [actor rulings](../plans/rust-native-proof-account-isolation.md), including the owner's later UID-501 synthetic authorization. Signing, native execution and real credential writes were separately approved. The original test-account failure remains undiagnosed; the later owner-account native proof results do not establish its cause.

The validation table below records the earlier integrated snapshot, not the latest source totals.

## Integrated implementation

- Five fixed provider adapters: Claude, Codex, Cursor, OpenCode Go and Muse.
- One current-thread Tokio runtime, shared HTTP client and on-demand scheduler, with coalesced checks, success caching and persisted cooldowns.
- Feed v1, authenticated provider controls, shared text formatting and both Claude Code hooks.
- HTTP-only `quotabard quota` client, dispatched before daemon initialization.
- Explicit per-user service install, update and removal scripts, templates and mocked lifecycle tests.
- Reviewed, inactive Pi and shell consumer artifacts under `daemon/clients/`.

Read [the daemon README](../../daemon/README.md), [service preparation](../../daemon/SERVICE.md) and [consumer packaging](../../daemon/clients/README.md) before using any executable or applying a patch. No-argument `quotabard` starts the real daemon; it is not a version or validation command.

## Validation completed

The joined implementation passed these checks on both macOS arm64 and Linux x86-64:

| Check | Result per platform |
|---|---|
| Rust formatting | Passed |
| Clippy, all targets, warnings denied | Passed |
| Rust all-target tests | 248 passed: 228 library, 2 CLI, 2 instance-lock, 1 fake credential-reader, 15 HTTP |
| Mocked service tests | 36 passed |
| Shell syntax and ShellCheck | Passed |
| Python compilation | Passed |
| Release executable and library build | Passed |
| Both release examples built, not run | Passed |

macOS staged plist lint also passed. Main independently reran all 248 Rust tests on macOS after integration; none failed or were ignored. Ordinary tests use fake providers and credential readers, not owner Keychain items or live provider endpoints. Manager commands and PID probes in service tests are mocked; real `plutil` receives only staged temporary files.

Consumer validation was separate: 574 full npm tests, including 37 focused checks, and 5 shell-wrapper tests passed. Those consumer results describe the earlier staged snapshot. The Pi patch has since merged through PR 184 and Linux's configured checkout has been updated and functionally checked. Linux's public `aiquota` wrapper is installed and accepted; its internal `quotabard quota` subcommand remains unchanged. Mac's `aiquota` wrapper is installed and returned valid feed data, including the remaining provider errors. Legacy installed wrappers are removed only after each cutover is verified. Pi's `/quota`, `get_quotas` and HTTP routes are unchanged; no operating-system command was changed.

Independent lifecycle and filesystem-safety reviews approved the service source. Recovery uses fresh, owner-only, descriptor-anchored private transaction directories. Root or an uncooperative same-user process modifying their entries between checks and internal rename/unlink is outside the promise. Public-path no-clobber and anchoring safeguards remain in place. Do not clean up recovery files while their management process runs. See `daemon/SERVICE.md` for the complete boundary and post-exit manual recovery rules.

## Live acceptance in progress

After the owner quit the Swift app, they approved live provider and resource checks, including owner-present Mac Keychain attempts and normal Claude/Codex OAuth renewal if needed. The finished Mac daemon returned observations for Codex and OpenCode Go; Claude, Cursor and Muse were blocked on credential access. Its foreground resource run completed and the daemon stopped. See [Mac live acceptance](rust-migration-mac-live-acceptance.md) for the measured memory and CPU, Keychain metadata results and limitations. This is not all-provider acceptance or a cutover.

The owner also approved stopping the old Mac reverse-tunnel LaunchAgent, `com.aakshintala.roundhouse-quota-tunnel`, which recreated Linux port 8787. That existing agent was unloaded; its plist was not changed or permanently disabled. Linux live verification then returned fresh observations without errors for all five providers. The 1,000-request cache burst and real HTTP-only CLI passed. A separate resource run sampled peak RSS of about 8.77 MiB and 0.01 seconds CPU over a 120-second post-activity idle window. See [Linux live acceptance](rust-migration-linux-live-acceptance.md) for evidence, the corrected collector and limitations. All foreground test daemons stopped at that stage. The Linux native service was subsequently installed and accepted; the Mac signed service rollout is now separately authorized.

## Build-storage diagnosis

Linux release failures did not mean roundhouse was out of disk. Its `/tmp` is a separate 16 GB tmpfs with user quotas; home is ext4 and had about 315 GB free when measured. The compiler returned `EDQUOT` under `/tmp`, despite free filesystem blocks and inodes. The exact per-user quota limit was not measured.

Two build-validation mistakes also delayed the gate:

- An external wrapper held Cargo's own `.cargo-build-lock` while starting Cargo. Builds now rely on Cargo's native locking only.
- A cache-copy check compared directory sizes across filesystems. The cache root was 140 bytes on tmpfs and 4,096 bytes on ext4. All 8,359 regular files matched in size and SHA-256; the directory-size difference was harmless.

The passing Linux builds and joined gates used `/home/aakshintala/.quotabar-build-2r_8cf63/target`, with compiler temporary files in its `tmp` subdirectory and logs in `logs`. The owner approved this fresh private build directory. No existing target or cache was deleted. Future Linux gates should reuse this home-backed storage rather than the quota-limited `/tmp` cache.

## Evidence and preservation

The reviewed service import used a 45-entry manifest with SHA-256 `ee17ea85992a4d3915b992cef430fc676ea4ae40fe29e070ed1fe0328a17af12`. All entries matched Root, the private candidate and Linux source before the joined gates. The later `daemon/README.md` update was prose only; it replaced the obsolete claim that the executable, five providers and hooks were absent.

Local evidence includes:

- `/tmp/quotabar-service-root-joined-status.log`
- `/tmp/quotabar-service-root-joined-imported-files.log`
- `/tmp/quotabar-service-root-joined-{mac,linux}-*.log`
- `/tmp/quotabar-service-main-fresh-mac-test.log`
- `/tmp/quotabar-service-doc-fix-review.md`
- `/tmp/quotabar-service-lifecycle-fix-review.md`

Linux joined logs also remain in the home-backed build directory. Binaries, libraries and examples were identified and hashed without execution. The import preserved the other core files, all 96 Swift source files and all six staged consumer-package files by hash and mode. At that earlier integration point, HEAD remained `e12da3525b94561afc3f68ec4eeba59e82762941` and no commit had been made. The owner has since authorized the reviewed Linux-first checkpoint and rollout described above.

Two earlier diagnostic invocations accidentally ran the installed subscription consumer through a shadowed `quota -s` command. They may have read or refreshed its feed and were disclosed. They are not Rust acceptance evidence. Do not use `quota` or `repquota` for further disk diagnostics.

## Not accepted or activated

The following remain separate gates, not implied by passing offline tests or release builds:

- Successful Cursor and Muse owner-Keychain credential access. Authorized Mac live attempts returned `keychain_blocked`; metadata-only diagnosis did not establish effective authorization.
- Actual OAuth credential-write acceptance. Normal renewal is now authorized for live checks, but the Mac run did not observe it being exercised. Conditional writes and interleavings were tested with fixtures.
- Native macOS logging acceptance and complete lifecycle coverage. Both installations and one graceful native restart per host passed. Update, rollback, removal and failure injection remain untested. The separate old-tunnel unload does not establish service acceptance.
- Complete production-binary resource acceptance. Linux production resources and all-five provider verification completed in separate runs. The Mac run measured the finished daemon with two working providers, not all five; its idle window may include installed Pi polling. Neither host's figures establish a universal resource ceiling or absolute isolated idleness. Earlier synthetic resource reports remain distinct.
- Complete installed-consumer acceptance. Both services and public `aiquota` wrappers are installed. The configured Linux Pi source passed a native-feed callback smoke; exact configured hook execution and footer rendering remain distinct pending checks. Linux's pre-existing lingering setting was preserved without change.

No source result authorizes those operations. The Claude `/usr/bin/security` credential fallback remains blocked because no-dialog behaviour is not guaranteed; its file-backed route is implemented.
