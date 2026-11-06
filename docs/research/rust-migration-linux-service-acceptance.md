# Linux native service acceptance

## Result

The Rust daemon is installed, enabled and running independently on Linux. One bounded live request returned fresh observations without errors for all five providers. The installed HTTP-only CLI reused the same captures. A later native restart and the installed `aiquota` wrapper also passed with all five providers fresh and error-free. Mac acceptance and Pi/hook cutover are separate work.

The installed release comes from checkpoint `6ebdcf7`, with Swift retained. Its SHA-256 is `6b96daabff2378e4411e0186d726988c0ed21d03c9e46cb7ea6f789537d37b8b`. The checkpoint passed 259 Linux tests and 270 Mac tests, formatting, warning-denied Clippy and release builds. Independent review approved the Linux production scope for Standards and Spec.

## Installation

The reviewed installer ran once as the existing non-root user. It installed:

- executable: `~/.local/bin/quotabard`, owner-only mode 0700
- unit: `~/.config/systemd/user/quotabard.service`, mode 0600

The executable hash matched the approved release. The unit matched the reviewed template, SHA-256 `2a44f57b82080634d2780b68dada6fd2960e482f99cbc0b28aac91e524f56bf7`.

Native systemd inspection returned `LoadState=loaded`, `ActiveState=active` and `UnitFileState=enabled`, with a live main PID. The only service listener was `127.0.0.1:8787`; no wildcard or external address was bound.

The pre-existing `Linger=yes` setting was preserved, not enabled or changed. The service may therefore remain running after logout. Three existing user-owned service directories had group-write removed, from 0775 to 0755, before installation. There was no recursive permission change. Cargo output was copied into a private deployment-input directory; its bytes and original permissions were not changed.

An initial acceptance instruction incorrectly expected an installed mode of 0755. The installer and its tests intentionally use 0700. Main accepted that stricter owner-only mode without changing it. The historical stop on that discrepancy is retained in the private install report.

## Bounded live check

One direct loopback `GET /quotas` used a proxy-disabled client, a 25-second deadline, a 2 MiB body cap and no redirect or retry. It returned HTTP 200 in 741 ms, schema v1, five providers and no disabled providers.

| Provider | Freshness | Last error | Bucket count |
|---|---|---|---:|
| Claude | fresh | none | 2 |
| Codex | fresh | none | 1 |
| Cursor | fresh | none | 1 |
| Muse | fresh | none | 2 |
| OpenCode Go | fresh | none | 3 |

The installed executable then ran its `quota --json` client subcommand once. It exited 0 in 9 ms with no stderr. Capture timestamps and allowlisted summaries matched the direct response, establishing shared-cache reuse for that request. Freshness is not a claim that every subscription has available budget. No particular OAuth renewal was observed or separately established.

Response bodies were parsed in memory. No raw feed/provider body, quota amount, credential, prompt or runtime token was printed or retained. Only provider IDs, status/freshness/error enums, capture-presence/cache-match booleans, bucket counts and timing metadata were retained.

A bounded user-journal query succeeded and was reduced in memory to two entries at severity 6. No raw journal message was retained. This establishes journal connectivity, not fatal-error or restart logging behavior.

## Installed wrapper and native restart

The reviewed shell wrapper is installed at `~/.local/bin/aiquota`, owner-owned mode 0755, SHA-256 `db32e7fd73b210b5ed5db621c9f08cfdf6c0e545e79ac5ef7fe7c69f9d38dd94`. Its first native invocation passed with all five providers fresh and error-free. The legacy user `quota` wrapper has not been removed; no OS quota program was invoked or changed.

The first service pass deliberately deferred restart to avoid an immediate cold provider wave. A later pass checked that the preceding wrapper report was 796 seconds old, then ran exactly one `systemctl --user restart quotabard.service`. It returned 0. The old PID/start-time identity exited, a different main PID became active, and TCP-only readiness succeeded in about 101 ms without querying a quota route. The service remained enabled and loopback-only, with unchanged daemon/unit/wrapper hashes and permissions.

One post-restart `aiquota --json` call exited 0 in 730 ms with all five providers fresh and no errors. Raw output remained in memory. A bounded journal query returned six severity-6 entries; no raw messages were retained and no fixed fatal code was recognized. This establishes native restart and the post-restart client path, not every logging or recovery path.

## Remaining lifecycle checks

These passes do not establish:

- native update, rollback or removal
- failure-injection recovery
- installed Pi or Claude Code hook acceptance

The 36 mocked service tests remain distinct from native lifecycle evidence. The service was left enabled and running. These Linux passes made no lingering change, root operation, Mac operation, signing, Keychain probe or Swift removal.

Detailed private evidence is retained in `/tmp/quotabar-linux-service-{preflight,ready,installed,restart}.md`, `/tmp/quotabar-linux-aiquota-installed.md` and the approved home-backed deployment logs. Earlier foreground Linux live/resource runs remain separate evidence; they did not establish this installed service's resource envelope.
