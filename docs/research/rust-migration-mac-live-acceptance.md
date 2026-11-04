# Mac live acceptance: partial result

The finished production daemon ran temporarily against the owner's real logins. Codex and OpenCode Go returned observations. Claude, Cursor and Muse did not pass credential access. This is not an all-provider Mac acceptance or a cutover.

## Permission and scope

The owner quit the Swift app and explicitly approved live provider checks and resource measurements. The owner was present for the actual Cursor and Muse Keychain checks and separately approved normal Claude/Codex OAuth renewal and conditional credential-file updates if needed.

The run used the unchanged release binary at `daemon/target/release/quotabard`, SHA-256 `a9ef79a1458cd89bc1c5567a03370e79abcb0f444328689ed0c7c297f4c238d9`, with real HOME, the default configuration and port 8787. No service was installed, client switched, ACL changed or credential-store workaround attempted. The foreground process ran for about 199 seconds, exited normally after its supervisor's SIGTERM, and left port 8787 free. Daemon stderr was empty. Runtime cooldowns were preserved.

## Provider results

| Provider | Observed result |
|---|---|
| Claude | `keychain_blocked`; the deliberately blocked fallback was not attempted |
| Codex | Fresh observation, no error |
| Cursor | `keychain_blocked` during its authorized native read |
| OpenCode Go | Fresh observation, no error |
| Muse | `keychain_blocked`, followed by the preserved attempt guard reporting `cooldown` |

The first controlled GET took 750.7 ms. The second took 14.8 ms. Codex and OpenCode Go retained the same capture times, about 48 seconds apart, which demonstrates reuse of those observations. No OAuth renewal was observed; permission to renew is not evidence that renewal was exercised.

The 1,000-request burst and cached CLI check were skipped. Repeating GETs with failed providers could start new eligible attempts, rather than testing only the success cache.

## Production-process resources

Native `proc_pid_rusage` version-4 counters sampled the actual daemon, not the earlier synthetic example. The external supervisor and counter helper are not included in these daemon-memory numbers.

| Observation | Result |
|---|---:|
| Initial sampled RSS | 7,684,096 bytes, about 7.33 MiB |
| Initial lifetime peak footprint | 1,835,344 bytes, about 1.75 MiB |
| RSS after the first live GET | 15,646,720 bytes, about 14.92 MiB |
| Highest sampled lifetime peak footprint | 4,981,264 bytes, about 4.75 MiB |
| Final sampled RSS | 15,695,872 bytes, about 14.97 MiB |
| Initial / first-request / final thread counts | 1 / 5 / 2 |

Initial samples from elapsed 0.004 through 25.174 seconds had unchanged CPU and wake counters after the first post-start sample. In the later foreground window, elapsed 78.804 through 194.019 seconds, user and system CPU increased by 426,701 and 183,042 raw ticks. Interrupt-wakeup and package-idle-wakeup counters increased by 13 and 0.

Today's Mach timebase was 125/3. Prior same-host calibration supports treating the CPU fields as Mach ticks, giving an estimated 25.4 ms CPU increase over that 115.2-second window. The SDK does not document the CPU-field unit, and no new calibration was performed today. The estimate is not an independent verification of units.

The installed Pi quota client can poll every 60 seconds. It was not disabled. These are foreground measurements with possible consumer traffic, not proof of isolated daemon idleness. Wake counters do not identify exact wake causes. RSS and memory footprint are different measures. The result also excludes three successful provider paths because their credentials were blocked; it is not a memory ceiling for all five providers working.

## Keychain metadata follow-up

An independently reviewed, one-shot metadata helper ran with interaction disabled. Both password output pointers were NULL; the SDK and reviewed Apple implementation show that this skips password retrieval. It found the Cursor and Muse item references and reported that the default Keychain was unlocked. It completed in 0.319 seconds with exit zero and no stderr.

A separately reviewed v2 metadata helper then checked the matched items' own Keychains and access metadata. It completed once in 0.321 seconds with exit zero and no stderr. Both containing Keychains were unlocked. Each item had one observed Decrypt-tagged ACL, no Any-tagged ACL and one PartitionID-tagged ACL. The filtered application arrays contained one entry for Cursor and four for Muse; prompt flags were zero.

This rules out locked containing Keychains at the observation time, but counts and flags do not prove the daemon's effective authorization. No application identities, paths or partition values were inspected. Application access restrictions remain the leading explanation, not a demonstrated complete policy evaluation. No ACL or trust changes were made or authorized by the result. Claude's fallback is a separate missing path and was not queried by either helper.

## Evidence

- `/tmp/quotabar-live-mac-status.log`
- `/tmp/quotabar-live-mac-providers.log`
- `/tmp/quotabar-live-mac-resources.log`
- `/tmp/quotabar-live-mac.1n2csR`
- `/tmp/quotabar-keychain-metadata-review.md`
- `/tmp/quotabar-keychain-metadata-result.md` (reconstructed history from retained v1 execution output, not a rerun)
- `/tmp/quotabar-keychain-access-review.md`
- `/tmp/quotabar-keychain-access-result.md`

No raw provider response or credential data is included in this report. Native service lifecycle/logging, real OAuth renewal, all-provider Mac success and deployment/cutover remain separate gates.
