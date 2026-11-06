# Native proof actor rulings

## Follow-up: owner-account synthetic run

After the actual `test` login run returned `controller_failure`, the owner explicitly instructed Main to run the synthetic test from the current owner account instead. This supersedes the UID-502-only execution restriction below for a separately reviewed owner-only variant, not for the original frozen launcher.

The new variant must check real/effective/audit UID 501, retain checked lifetime UI suppression, and operate only on freshly created synthetic fixture items. It must not query, read or update any subscription credential item. Main informed the owner that creation fallback can leave a synthetic item in the default Keychain and deleting the temporary Keychain does not remove a fallback item. Destination mismatch still fails before secret retrieval or modification. No default/search-list setter, ACL repair, partition manipulation, upsert, credential export or identity impersonation is allowed.

The original UID-502 sources, signed artifacts and result remain unchanged as evidence. The owner variant needs its own frozen source hashes, fake gates, compile attestation, signing and scoped review before execution. The existing approval to sign reviewed helper B applies; Main performs signing and the actual owner run. It is not permission for a provider probe or original-owner credential write.

The owner variant preserves its private synthetic fixture root rather than deleting files automatically. It must report the failing stage and a closed exception class without raw messages, native bodies, accounts or credential data. Child deadlines and confirmed reaping remain mandatory. No guessed cleanup or retry loop is authorized.

## Owner-account results and later live authorization

The reviewed owner variant passed its clean cross-identity run. A creation-time file-permission repair was needed: the SDK created a mode-0644 fixture, which the strict read guard rejected. The repaired helper checks the created Keychain destination, hardens only that verified synthetic file to 0600, then creates its item. The read guard remains unchanged. Main verified new sources, compiled bytes, signatures and manifest before one successful complete run. The root remains preserved.

The Rust basic proof also passed in the owner account. Main verified its three approved source hashes and compiled artifact, then used the reviewed identity guard and bounded child runner. The allowed update succeeded; the locked update was attempted and refused. The reported post-call lock state was locked, and exact data verification passed. These outcomes do not establish an independent write ACL or vendor-item authorization.

The owner separately approved bounded real Mac provider checks and signed service installation. This permits normal Claude/Codex OAuth renewal and conditional persistence when needed, not forced expiry or retry loops. Cursor/Muse remain read-only. The owner then instructed Main to proceed after a positive official Claude CLI authentication-status check could not establish its internal read path. Main accepts fresh-process `claude auth status --json` as the supported operational before/after check, without claiming it proves plaintext decryption or a specific Keychain read. Record actual renewal and post-check outcomes separately.

The old UID-502 launcher remains frozen historical evidence, not a request for another account switch. See [the native result and artifact record](../research/rust-migration-mac-signed-artifacts.md).

## Original ruling: isolate in the new standard account

The owner chose a throwaway macOS account and created `test`. Main verified its UID is 502, differs from the owner and root, and has no admin group membership. Do not ask for its password or copy provider credentials, owner credential files or signing keys into it.

## Correct the creation-boundary premise

Both SecKeychainItemCreateFromContent and SecKeychainAddGenericPassword can internally replace an unavailable explicit destination with a default Keychain. A non-null fixture reference is not an unconditional destination boundary. Add-then-SetAccess also does not solve that issue, and Access::setAccess uses prompt credentials directly. Do not try a third creation API or add an ACL setter.

Keep the supported item-level creation sequence using SecKeychainItemCreateFromContent with SecAccess naming A and B. Move the actor into the disposable account instead. This supersedes an unconditional never-default claim for fixture creation: the original owner's Keychain must remain inaccessible; a fallback can affect only fixed synthetic items in the disposable account's realm. Unexpected destination is a test failure, never a passing result or reason to repair preferences. No upsert, real credential lookup, default/search-list setter, ACL restoration or partition manipulation is authorized.

## Enforce the execution boundary

- Run only from an actual logged-in `test` session. Do not use sudo -u, HOME overrides or the owner's GUI/audit session as substitutes.
- The reviewed controller and private C helper must reject real/effective UID other than 502 before any Security API. Verify the audit login UID is also 502 through a source-reviewed public own-process audit API; unavailable/inconsistent audit identity fails closed. Ordinary tests inject fake identity operations. Do not inspect or log another session's audit data.
- The controller is the only approved launch path for both the existing Rust disposable proof and new cross-identity helpers. It verifies the account/audit boundary before starting either. Signing/compilation does not grant runtime permission, and direct owner-session execution is not approved.
- Require explicit execution opt-in, exact reviewed source/artifact manifests and separate owner approval before running. The new account is authority to prepare this design, not permission to run unreviewed code or use a private signing key.
- Use a canonical safe temporary parent, a generated single-component nonce root (0700/UID 502), and a fixed fixture basename. Reject the full Apple preference-special-case predicates including `/login.keychain` anywhere in the path, system locations, symlinks and unexpected destinations before relevant Security calls. Validate destination absence for creation; never reopen a pre-existing fixture for create.
- Where creation returns an item, check its containing Keychain using public typed APIs against the retained intended fixture before any follow-on read/update. A wrong destination stops the test. This check detects fallback; account isolation, not this after-the-fact check, protects the original owner's Keychain.
- UI suppression remains checked for the whole helper lifetime before every Security operation. Do not use SecKeychainItemSetAccess. Create explicit synthetic application access at item creation; no authorization repair after refusal.
- Child deadline: 10 seconds. Overall proof deadline: 90 seconds. On timeout, stop sequencing; terminate the owned child, wait up to 2 seconds, then kill and wait up to 2 seconds if necessary. Cleanup only after confirmed reap. If exit cannot be confirmed, preserve the root and report failure; never continue another update/read or delete files under a live child.
- Cleanup is filesystem-only, restricted to the known private fixture root after all helpers exit. Never SecKeychainDelete or preference setters. Default-realm fallback artifacts in this disposable account are not deleted automatically; report failure and leave account cleanup to the owner.

## Existing gates remain

The existing basic proof still requires source verification and execution approval. The cross-identity proof requires fresh A and B initial reads, B's own bounded compare/read before one existing-item NULL-attribute update, and fresh A and B exact replacement reads. A is ad-hoc; B is signed with the owner's inventoried Developer ID only after separate private-key-use approval. No vendor impersonation or partition-value inspection.

Synthetic success is not proof about the owner's actual CLI item. Any later real Claude write still requires separate approval and official-CLI access checks before/after. No real Mac provider run that can renew credentials is allowed before those gates. No native proof, signing or owner credential access is authorized by this document.
