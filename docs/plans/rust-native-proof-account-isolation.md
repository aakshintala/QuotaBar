# Main ruling: isolate native proof in the new standard account

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
