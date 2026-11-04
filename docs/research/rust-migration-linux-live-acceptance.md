# Linux live acceptance

The unchanged production daemon returned fresh observations without errors for all five providers. Its HTTP-only CLI and a 1,000-request cache burst passed. A separate foreground run measured production-process resources. No new service was installed and no installed consumer was switched.

## Production binary and scope

The binary was `/home/aakshintala/.quotabar-build-2r_8cf63/target/release/quotabard`, SHA-256 `28457897cb4b23f12a44b97473df886833ea88adc4096cbdd426d8ae93161230`. Hash checks matched before and after execution. It ran with real HOME, normal credentials and all five providers enabled by the absent `~/.quotabar/daemon.json` configuration. No source or build changed for live testing.

The owner approved real provider checks, resource measurement and normal Claude/Codex OAuth renewal with conditional credential-file updates if needed. The runs preserved runtime guards and did not force expiration or bypass cooldowns. OAuth renewal was not specifically observed; successful readings do not establish that renewal was exercised.

## Removing the old port forward

Linux port 8787 was occupied by an SSH reverse forward to the Mac feed. The owner approved stopping its original SSH session. A corrected, independently reviewed pidfd helper sent one unprivileged SIGTERM to the exact approved PID 2822, which exited. A new forward immediately appeared; no replacement session was chased or killed.

The source was then traced to the Mac LaunchAgent `com.aakshintala.roundhouse-quota-tunnel`. Its loaded state identified `/usr/bin/ssh`, PID 8586, and its unchanged plist specified `KeepAlive=true` and `-R 127.0.0.1:8787:127.0.0.1:8787` to the Linux host. With separate owner approval, `launchctl bootout` unloaded only that agent. Its process exited and Linux port 8787 became free.

The old agent remains unloaded. Its plist was retained byte-for-byte; it was not permanently disabled or uninstalled and may load again at a later login. Permanent removal of this conflict belongs to cutover. No new `quotabard` service was installed or activated. Diagnostic SSH connections disabled forwarding and connection sharing explicitly.

## Provider and client verification

A short verification run returned schema version 1 and these observations on its first request:

| Provider | Result | Bucket count |
|---|---|---:|
| Claude | Fresh, no error | 2 |
| Codex | Fresh, no error | 1 |
| Cursor | Fresh, no error | 1 |
| OpenCode Go | Fresh, no error | 3 |
| Muse | Fresh, no error | 2 |

The first GET took 1,066.33 ms. A second refresh wave was unnecessary. The 1,000 sequential cache requests completed in 0.161 seconds, with the same capture times and no provider errors. Minimum, median and maximum request latency were 0.12, 0.13 and 1.23 ms. This demonstrates cached feed reuse; it is not a direct packet-level count of vendor requests.

`quotabard quota --json` passed against the loopback feed with exit zero. The public `aiquota` wrapper remains staged rather than installed. The foreground daemon then exited with code zero, and port 8787 was free afterward.

## Production-process resources

A preceding run measured the same binary using Linux process counters. These are daemon-only RSS and CPU numbers, not supervisor memory or macOS memory-footprint values. Resource collection and corrected provider verification were separate runs; provider outcomes from the resource run were not recovered.

| Observation | Result |
|---|---:|
| Initial idle RSS | 6,852 Linux kB, about 6.69 MiB |
| Highest sampled RSS / reported high-water RSS | 8,984 Linux kB, about 8.77 MiB |
| Initial / peak / settled thread count | 1 / 6 / 1 |
| CPU increase during the initial 30-second idle | 0 ticks at 100 Hz |
| CPU increase during the post-activity 120-second idle | 1 tick, about 0.01 seconds |
| Total CPU at final sample | 5 ticks, about 0.05 seconds |
| Post-activity voluntary context-switch increase | 4 |
| Post-activity non-voluntary context-switch increase | 0 |

RSS stayed at 8,984 kB over the post-activity idle window. Tick resolution cannot prove zero sub-tick CPU use. Context switches are not exact wakeup counts, and unmodified consumers could send external demand. These observations are not proof of absolute isolated idleness or a universal resource ceiling.

## Collector correction and execution limits

The resource run's collector incorrectly expected provider IDs as object keys. The real feed contains a `providers` array. Its logged `missing` entries were collector-generated, not provider failures; its `test_pass=true` recorded process completion only. Raw quota bodies had intentionally not been saved, so provider success could not be recovered from that run.

The collector was corrected against the actual feed shape. Syntax and synthetic-feed tests passed before the short verification run, including malformed shape and duplicate/missing provider checks. The valid original resource measurements were retained rather than repeated. An earlier initial-idle attempt hit the tool's 30-second limit before any controlled GET; its child also exited normally.

The supervisor did not manually edit cooldowns or credentials. Normal production-daemon runtime persistence and authorized OAuth behavior were allowed; statements that the test did not edit state must not be read as proof that no runtime files changed.

## Evidence and remaining gates

- `/tmp/quotabar-live-linux-verified-status.log`
- `/tmp/quotabar-live-linux-verified-providers.log`
- `/tmp/quotabar-live-linux-verified-collector-tests.log`
- `/tmp/quotabar-live-linux-status.log` (resource/process completion, not provider acceptance)
- `/tmp/quotabar-live-linux-providers.log` (invalid original collector interpretation)
- `/tmp/quotabar-live-linux-resources.log`
- `/tmp/quotabar-live-linux-acceptance.md`
- `/tmp/quotabar-pidfd-stop-review.md`
- `/tmp/quotabar-linux-stop-corrected-result.md`
- `/tmp/quotabar-legacy-tunnel-stop.md`

No credentials, raw provider bodies, quota amounts or account identifiers are included here. Mac Keychain access, actual OAuth-write acceptance, native lifecycle/logging for the new services, consumer activation and permanent deployment remain separate gates.
