# Mac native service acceptance

## Result

The signed Mac daemon is installed and running as a native LaunchAgent, independently of Linux. Loopback readiness, the feed, the installed HTTP client and public `aiquota` wrapper passed. Provider acceptance remains partial: Codex and OpenCode Go are fresh; Claude, Cursor and Muse return `keychain_blocked`.

Do not retire Swift or claim five-provider Mac acceptance from this result. Native credential renewal and persistence were not observed. Passing synthetic Keychain proofs does not establish access to vendor credential items.

## Installed artifacts

The daemon was built from commit `7b4f715` and signed with the separately approved Developer ID. The installer copied the signed bytes unchanged.

| Artifact | Evidence |
|---|---|
| Daemon | `~/Library/Application Support/QuotaBar/bin/quotabard`, UID 501, mode 0700 |
| Daemon SHA-256 | `4a69ec3d30b72dada99f625d47626598e29b5f9bc5cde123925c2223f00c95e2` |
| Signature | strict verification passed, identifier `com.aakshintala.quotabard`, Team `936XPH47M9` |
| LaunchAgent | `~/Library/LaunchAgents/com.aakshintala.quotabard.plist`, UID 501, mode 0600 |
| Plist contents | matched the reviewed template and installed executable path |
| Public client | `~/.local/bin/aiquota`, UID 501, mode 0755 |
| Client SHA-256 | `db32e7fd73b210b5ed5db621c9f08cfdf6c0e545e79ac5ef7fe7c69f9d38dd94` |

The installer ran once without sudo. The management process had real/effective/audit UID 501 and an Aqua manager for UID 501. Existing parent directories passed ownership and permission checks; no recursive chmod occurred. A missing daemon configuration enabled the default five providers. No provider preferences were rewritten.

The exact LaunchAgent label was loaded and not marked disabled. Its process executable and hash matched the installed binary. Its listener was only `127.0.0.1:8787`.

## Feed and client observations

Bounded direct requests used no proxy, redirects or retries. Responses stayed in memory; only fixed provider IDs, freshness, errors, capture presence, bucket counts, statuses and timing were retained.

The first direct feed returned HTTP 200 in 12 ms. Its collector hit a formatting error after parsing and before reporting provider metadata. That request was not repeated. A subsequent installed CLI response established two fresh providers, two blocked providers and Muse in cooldown.

After the owner reported narrow app-access grants, one direct feed returned HTTP 200 in 9 ms. The installed `aiquota --json` returned valid feed JSON. Their provider projections matched after excluding age. This comparison does not prove no new credential attempt occurred: failed providers have no successful observation to reuse.

## Controlled restart

The owner reported that adding the installed executable to the Claude and Cursor access lists had not helped. Main stopped permission changes and tested process lifetime once.

The controller verified the old process, sent one graceful SIGTERM through launchctl, and confirmed its exit. One kickstart then started a distinct process from the unchanged signed binary. TCP readiness passed within five seconds. No forced kill, repeated signal or cooldown reset occurred.

One post-restart feed returned HTTP 200 in 732 ms:

| Provider | Freshness | Error | Captured | Buckets |
|---|---|---|---|---:|
| Claude | unknown | `keychain_blocked` | no | 0 |
| Codex | fresh | none | yes | 1 |
| Cursor | unknown | `keychain_blocked` | no | 0 |
| OpenCode Go | fresh | none | yes | 3 |
| Muse | unknown | `keychain_blocked` | no | 0 |

Depleted quota status is not an observation failure. Muse's changed error is consistent with natural cooldown expiry; no cooldown was cleared. A fresh process did not resolve the three Keychain errors.

## CLI checks and diagnostic limit

Fresh official `claude auth status --json` processes reported logged-in Claude OAuth before and after the live phases. Main initially required proof of the CLI's internal Keychain read path, then accepted supported authentication status after the owner instructed Main to proceed. This operational check does not establish plaintext decryption, token renewal or persistence.

No raw credentials, provider bodies, quota amounts, accounts, ACL contents or partition IDs were retained. A narrowly filtered securityd log query produced no useful records; that is not evidence that any particular policy was absent.

The current error maps multiple native failure stages to `keychain_blocked`. The trusted-app change and fresh-process test did not establish the cause. A private diagnostic build is being prepared to trace only existing native call stages and closed status classes. It must keep UI suppression and all credential operations unchanged, add no credential reads, and never log sensitive values. It is not accepted or installed by this page.

Update, rollback, removal and failure injection remain untested. The obsolete tunnel plist remains unloaded and preserved. Private evidence: `/tmp/quotabar-mac-service-live-acceptance.md` and `/tmp/quotabar-mac-post-grant-security-classes.md`.
