# Service preparation

This lane is prepared, not installed. No real service-manager command was run, and no manager state was changed during implementation. Mock tests do not prove native manager acceptance, provider behavior, or quota refresh. The Mac Keychain owner-acceptance gate and the daemon cutover gate remain open.

The scripts install only a prebuilt `quotabard` binary for the current user. They never run the incoming binary to validate it. They do not themselves read, create, or change `~/.quotabar`, credentials, Keychain items, or provider settings. Install and update can start the daemon, which may then read its usual configuration and credentials and create its runtime control token. We did not exercise that runtime behavior.

## Before a user runs a script

Use Python 3.8 or newer and a native, executable `quotabard` binary built for this host. The scripts do not install Python or fetch dependencies. They reject root and accept no target, service-name, or force options.

Review the scripts and generated service definition before use. Run them from an explicit path:

```sh
daemon/scripts/service/install /path/to/quotabard
# after a later build
daemon/scripts/service/update /path/to/quotabard
daemon/scripts/service/remove
```

Installation and update can start the daemon. Removal can stop it. Run these commands only when you intend that service operation. A successful script reports that the manager operation completed. It does not prove that a provider refreshed quota.

The scripts validate the incoming file as a regular, non-symlink executable owned by the current user. It must contain bytes and must not be group- or world-writable. They stage each file beside its destination. On macOS, they validate the staged property list. They then stop and verify the exact service before changing installed files. A home-directory lock serializes QuotaBar's own scripts. Anchored directory descriptors protect against parent-path substitution. The scripts publish with no-clobber links and move existing files into private recovery directories. They verify recovery identities before restore or cleanup. If they cannot prove a safe change or recovery, they fail closed and preserve artifacts.

## Linux user service

Linux installs the binary at `$HOME/.local/bin/quotabard` and the unit at `$HOME/.config/systemd/user/quotabard.service`. The unit starts the executable directly, sends stdout and stderr to the user journal, and attaches to `default.target`.

A fresh install enables and starts the unit. An update keeps the prior active and enabled state. Removal disables and unloads only this unit before removing its owned files. The scripts do not enable lingering. A user service may stop when the user logs out; keeping it alive after logout requires separate explicit owner confirmation and is not done here.

The scripts use structured `systemctl --user show` properties. A nonzero query, missing property, unsupported state, nonzero PID after stop, or inconsistent state is an error. The script does not treat a general command failure as proof that the unit is absent.

## macOS GUI LaunchAgent

macOS installs the binary at `$HOME/Library/Application Support/QuotaBar/bin/quotabard` and the plist at `$HOME/Library/LaunchAgents/com.aakshintala.quotabard.plist`. The plist passes one executable path as one `ProgramArguments` item, runs at GUI-session load, and sends stdout and stderr to `/dev/null`. It does not use a shell, a helper process, or a persistent log file.

The script manages only the current GUI bootstrap session. It requires `launchctl managername` to report `Aqua` and `launchctl manageruid` to match the current effective UID. It does not manage LaunchAgents over SSH or from a background or login-window session.

To identify an absent service, the script reads the complete documented legacy `launchctl list` three-column output. It accepts only the expected header and rows with a PID or `-`, a signed numeric status, and an exact label. A query failure, duplicate label, malformed row, or unrecognized format fails closed; it is never interpreted as absence. For a loaded service, the script records its listed PID. It boots out only `gui/<uid>/com.aakshintala.quotabard`. Before changing files, it requires a successful bootout and a valid full list without that label. It also requires `os.kill(pid, 0)` to return `ESRCH` for the listed PID. The script never sends a signal. `PermissionError`, any other PID-query error, a still-live PID, or possible PID reuse blocks mutation.

Apple documents the legacy list columns and says their output should match earlier releases. Apple does not promise forward compatibility. This strict parser is the approved conservative seam, not a stable Apple API. A future format change will block install, update, or removal until reviewed.

Fatal daemon errors retain stderr diagnostics on every platform. On macOS they also use the already-installed `libc` syslog compatibility call. The log record contains only the fixed `quotabard` prefix and a static daemon error message. Client errors remain on stderr only. The template sends ordinary stdout and stderr to `/dev/null`; this does not provide a persistent log or rotation.

## State and recovery

The scripts never directly read, write, or delete `~/.quotabar`. Removal leaves that directory in place. Install and update can start the daemon, which may read settings and credentials or update its runtime control token and provider cooldown state. That runtime behavior remains untested here.

Install, update, and removal use private files on the destination filesystems. The scripts reject symlinked service directories and files. They do not overwrite an unowned destination that appears during publication. A lock serializes QuotaBar's own scripts, while anchored descriptors detect parent substitution. The lock cannot stop another process running as the same user from changing these directories. If a manager query or stop cannot prove a safe state, the scripts leave installed files alone. If rollback cannot be proved safe, they preserve recovery artifacts and report failure instead of restarting or deleting a possibly changed executable.

Each recovery directory is fresh, unpredictably named, owned by the effective user, mode 0700, and accessed through its retained directory descriptor. While management runs, its recovery files are private transaction storage: no other supported operation writes them. QuotaBar's own management operations are serialized. Do not let operators or cleanup tools edit these directories while the owning process runs.

This assumes that root and uncooperative processes running as the same Unix user do not modify private recovery entries between their final check and internal rename or unlink. Those operations are not atomic inode-conditional mutations. This limitation does not weaken the public-path no-clobber or parent-directory safeguards. Observed identity changes still cause artifact preservation and failed recovery, without an unsafe restart.

Begin manual recovery only after the management process exits. Retained artifacts are not automatically reused as trusted storage for a new transaction. A warning that recovery artifacts could not be removed means the manager operation completed, but hidden files remain beside an affected destination. Check the manager state and verify each artifact before removing it. Do not delete broad matching paths.

## Preparation checks

The repaired private candidate passed 248 Rust tests on each host: 228 library, 2 CLI, 2 instance-lock, 1 fake credential-reader and 15 HTTP tests. Formatting and warning-denied Clippy passed. The Rust tests use fake sinks to check fatal-message formatting and routing; they never call native syslog. Existing CLI integration tests run client mode against a fake loopback feed under a temporary home.

The service suite passed 36 tests on each host. It uses a temporary `HOME`, a fake incoming executable, and mocked `systemctl` or `launchctl` commands that reject unexpected calls. Deterministic interleavings cover public destination appearance, destination and parent substitution, observed recovery-file substitution, and transaction locking. These tests do not promise safety against concurrent private-storage tampering excluded above. The macOS plist test invokes `plutil` only on staged files under that temporary home. PID probes are mocked; these tests deliver no signals and start no daemon. Shell syntax, ShellCheck and Python compilation checks passed.

The repaired macOS arm64 release build produced a Mach-O executable, library and examples. The repaired Linux x86-64 release build also passed for the executable, library and both examples. Linux used a private home-backed ext4 build cache and compiler temporary directory, avoiding the separate `/tmp` tmpfs with user quotas. Both release commands exited zero; artifacts were identified and hashed without execution. No build ran the daemon.

Repair logs use these prefixes on their respective preparation hosts:

- macOS: `/tmp/quotabar-service-repair-mac-20261104T161935Z-`, including `cargo-test.log`, `release-build.log` and `final-service-tests.log`.
- Linux: `/tmp/quotabar-service-repair-linux-20261104T162028Z-`, including `cargo-test.log`, `release-build.log` and `final-service-tests.log`.
- Passing Linux release: `/home/aakshintala/.quotabar-build-2r_8cf63/logs/`. The build used that directory's `target` and `tmp` subdirectories. Both release commands passed after regular-file cache contents were verified. Directory sizes legitimately differ between tmpfs and ext4 and are not cache-content checks.

The earlier `/tmp/quotabar-service-prep-*` logs describe the pre-repair candidate and are historical evidence only. Earlier shared-cache attempts under `/tmp` either waited on an incorrectly pre-held Cargo lock or failed with `EDQUOT`; they are not passing release evidence. No existing cache or target was deleted to obtain the passing home-backed build.

These checks do not establish native service-manager acceptance, LaunchAgent logging behavior, provider behavior, quota refresh, or prompt-free Keychain access. The Mac Keychain owner-acceptance gate and daemon cutover remain blocked until a separate owner-present acceptance.
