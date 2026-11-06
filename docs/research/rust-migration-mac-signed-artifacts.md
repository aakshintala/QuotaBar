# Mac signed artifacts and isolated proof readiness

## Current boundary

Both owner-account synthetic proofs passed. The clean cross-identity controller exited 0 without manual intervention. The Rust basic proof verified an allowed update and a refused locked update, including exact data checks. Neither synthetic proof accessed subscription credentials.

The owner separately approved real Mac provider checks, normal Claude/Codex renewal and conditional persistence, and signed service installation. That rollout is in progress, not accepted by these synthetic results. Official CLI authentication status is the supported before/after check; it does not prove the CLI's internal Keychain read path.

The original UID-502 launcher returned `controller_failure`; its failed stage remains unknown. The owner then authorized a separately reviewed UID-501 variant. The original artifacts below remain historical evidence, not current launch instructions. See the [follow-up actor ruling](../plans/rust-native-proof-account-isolation.md).

For the original UID-502 artifact set documented here, native proof execution requires the human to log into the standard `test` account, UID 502. Real, effective and audit UIDs must all be 502. Neither `sudo`, user impersonation nor a changed `HOME` establishes that prerequisite. The controller checks identity before signature-verification or proof children; the C helper repeats the identity guard before Security operations. Audit identity alone does not prove foreground GUI membership.

## Source and build evidence

The Rust basic proof's observed-outcome reporting and created-item destination checks passed independent Standards and Spec review. Its focused fake suite passed eight tests; the full Mac suite passed 271 tests, formatting, warning-denied Clippy and release example compilation. No native proof was executed by those gates.

The separate C helper and Python controller passed scoped independent repair review for interruption/reaping safety, metadata-before-secret ordering, shared-flow fake coverage and deterministic fake clocks. Their frozen source hashes are:

- C: `33dacd29d45fcd178c34e997d4f7f12636652e8efb00f066bebf9c17af2b0477`
- Python: `624190a74020ab1c7aaf5f07a7a3f816d41e6fee7ec3f7a8cb9a8f409a89fedc`

The C fake executable imported no Security, CoreFoundation, BSM or audit functions. The Python fake suite passed 21 tests. The real C helper compiled and linked against the Mac SDK but was never executed during preparation. Its unsigned build hash is `2c0ec2ac7c55f4907505e3bc9db3159d30a0f05ff59e868388bf566416577e89`.

The production daemon and examples were built offline from QuotaBar commit `7b4f715`. A fresh 51-path source manifest matched before and after both builds, SHA-256 `1b1301b1c1be1a9566a0c78f0cf437b6eeef985cd4e7bfdf623fc95da56effad`. This manifest does not cover service files or establish a new Linux build. Linux continues running the separately accepted `6ebdcf7` release.

## Approved signing

The owner approved using the existing Developer ID key, Team `936XPH47M9`, for helper B and the daemon. Main checked the frozen source/build hashes, signed fresh private artifact copies and verified their static signatures. No signing key was exported or copied to the disposable account.

| Artifact | Signing | SHA-256 after signing |
|---|---|---|
| Helper A | ad-hoc | `3c19c5fc854c9384551dd2f743330d16160aecacdb96d9ab6e0a3062faa564ca` |
| Helper B | Developer ID | `12403b1ed5ed0d6614c2e8e827d91f2699faafdfaee51bd15567ff1121517acd` |
| Daemon | Developer ID | `4a69ec3d30b72dada99f625d47626598e29b5f9bc5cde123925c2223f00c95e2` |

The daemon identifier is `com.aakshintala.quotabard`. The unsigned daemon build hash was `67bb1af3da99c686efd356c840671f7344ca20e7773098cbe6a801b21dfa9da6`.

Signing proves identity, not permission to access vendor credential items. At signing completion, the daemon had not been launched or installed. Its later live rollout requires separate acceptance evidence. Private signing receipts retain only allowlisted identifiers, team/class, hashes, sizes and verification booleans, not raw certificate output.

## Human proof launch

The reviewed transport bundle contains the frozen sources, A/B helpers, optional basic-proof artifact and a pinned manifest. It contains no daemon, credential or signing key. The manifest SHA-256 is `c1944168e55d872511ae684d6bcfe68f7321297c0c7bf152c21ec57181de948b`.

A final independent artifact/command review approved launching the serialized command with explicit `/bin/bash`. This avoids zsh's special `path` variable. The script checks real/effective UID, hashes, owners and modes, creates a fresh private prepared root as UID 502 and then invokes the audited controller. No existing prepared root may be reused.

The first run is cross-identity only: A and B must both read the initial synthetic value, B must perform its own fresh authorized read and exact comparison before its data-only update, and fresh A/B processes must both read the replacement. Failure stops the sequence. Created-item destination checks detect creation fallback; they do not prevent it. Actor isolation confines any fallback to the disposable account.

The original UID-502 command deliberately omitted the basic proof because its source-validation paths referred to the owner's working tree. No owner-home permission change or frozen-controller edit worked around this. The later owner-account basic run is recorded below.

Synthetic results establish only their observed outcomes. They do not establish vendor-item authorization, actual renewal or installed service acceptance. The owner authorized those later operations separately.

## Owner-account synthetic results

The first UID-501 run created its fixture but failed at `read-initial-A`. A read-only diagnostic emitted `unsafe_root`. The native-created fixture was owned by UID 501 but had mode 0644; the helper required 0600. Main hardened only that exact file through a no-follow descriptor after owner, inode and link-count checks. The unchanged signed A then read successfully. Continuing the same fixture passed B's initial read, B's own fresh comparison and update, and fresh A/B replacement reads. This controlled continuation was not a clean controller pass.

The v3 repair sets mode 0600 after verifying the newly created Keychain destination and before creating its item. It preserves the strict read guard. The shared normalizer's filesystem fake tests and 23 Python tests passed; native compilation did not execute it. Independent Standards and Spec review approved the scoped change.

Main then verified source/build hashes, signed new A/B copies, verified their static signatures, and ran the full controller once against a fresh fixture. It emitted `cross_identity_proof_passed`, exited 0 and produced no stderr. No manual correction, retry or ACL/default change occurred. The private fixture was preserved.

| Clean owner artifact | SHA-256 |
|---|---|
| C source | `472d7247a202a9489c2f38fa830bf8347e3633397e5a108cda54f7e62e9a2d5d` |
| Python source | `cc2f695b6dec3a30fa55cc527b33e15251df286a95a35c42f9ab09a39b276763` |
| Unsigned compiled helper | `0f8cc5c0881ce3f422e7457027fa96c77f7d27c464eeba76191bcad09cd9b794` |
| Signed A | `7cd1f07778d4586fe82af3a1b73fa5c6049fb8a2ea4600623032e613ddc7e594` |
| Signed B | `c0f90bed74fff3b0de68382c819cce31280bce630dffdbe2c316b73ffca1e817` |
| Prepared manifest | `62758f7e725d46cd0724cdda4fedafb79b7a9cb250ccd5b6cded83c28c283e90` |

Main also verified the three approved Rust basic-proof sources and executable hash, then ran it once using the reviewed owner identity guard and bounded child runner. Its validated result was:

```json
{"proof":"disposable-keychain","allowed_modify":"success","locked_modify_attempted":true,"locked_modify":"refused","locked_after":true,"data_verified":true}
```

The actor checks required real/effective/audit UID 501 throughout. The basic result does not establish a separate per-item write-denial ACL. The cross-identity result establishes preserved access for the two synthetic actors, not every vendor CLI or concurrent writer.

Local evidence: `/tmp/quotabar-owner-{fixture-permission-probe,cross-identity-result,fixture-mode-fix,fixture-mode-source-review,clean-cross-identity-result,basic-proof-result}.md`.

## Real-provider authorization and CLI check boundary

After the clean synthetic pass, the owner approved bounded five-provider checks and signed service installation in the same account. Claude/Codex may renew and conditionally persist normally; Cursor/Muse remain read-only. Forced expiry, login/reset, token export, broad grants and programmatic ACL repair remain prohibited.

Before installation, fresh official `claude auth status --json` processes reported logged in, Claude OAuth and first-party API use. Main initially blocked because the internal credential-read path could not be established. After the owner instructed Main to proceed, Main accepted this supported status command as the operational before/after check. It is not proof of plaintext decryption or a particular native read. Record the post-check and any observed renewal separately; positive status alone does not establish either.

## Preserved artifacts

The home-backed pre-migration Mac binary was rechecked at its recorded backup path and still matches `a9ef79a1458cd89bc1c5567a03370e79abcb0f444328689ed0c7c297f4c238d9`. The pre-build checkpoint binary `6df6630f…` was also copied before relinking. Neither backup was executed, signed or overwritten.

Private evidence: `/tmp/quotabar-mac-final-artifacts.md`, `/tmp/quotabar-approved-signing-result.md`, `/tmp/quotabar-cross-identity-fix-round1-source-review.md`, and `/tmp/quotabar-isolated-proof-{launch-ready,launch-final-review,run-script}.md`.
