# macOS Keychain safety for the Rust migration

## Review repair state

Independent source review required changes to unsafe directory fallback, terminal read-back errors, native-boundary tests, file-refresh scope and the write-proof authorization model. Repairs and new joined gates are pending. The initial 255-test snapshot below is historical evidence, not a pass for current edited source.

The [corrected write-proof ruling](../plans/rust-native-write-proof-ruling.md) separates native raw-update observations from fake-backed production read-before-write safeguards. Source analysis also found [uncertain cross-identity access preservation](rust-native-keychain-write-semantics.md#cli-access-preservation-boundary). No real Claude Keychain write, or provider run that could renew it, is allowed before synthetic cross-identity evidence, independent review and separate owner approval. No native write proof, signing or owner-item operation has run in this lane.

## Initial signed-Mac offline snapshot

The Rust implementation now reads Claude credentials from a validated file first, then uses the fixed `Claude Code-credentials` Keychain service and the effective OS login account on macOS. It suppresses Keychain UI before native calls, performs an exact no-secret metadata lookup before reading data, bounds the credential record, and updates only an existing item after comparing its current bytes.

Ordinary tests inject fake stores and account resolvers. They do not call native Keychain APIs or the interaction-suppression setter. Linux remains file-only. Focused tests and full offline Mac and Linux gates passed: 255 tests per host, warning-denied Clippy, formatting, the release daemon and library, and all three examples compiled. These gates do not establish owner-item authorization or runtime no-dialog behavior.

This initial snapshot included an expanded, explicitly opted-in disposable write proof. Source review later rejected its assumed write-denial ACL; see the corrected ruling above. The proof has not been executed and remains blocked pending repaired-source review and separate owner approval. No signing, owner-item access, trust change, provider request, or service operation occurred in this implementation lane.

At this snapshot, implementation was ready for independent source review, not Mac acceptance or deployment. Linux live-provider verification remains prior evidence; this lane made no Linux provider requests.

The accepted untracked baseline manifest remains SHA-256 `7c027cc3695b24e91ed98bde115f33f9d3daaaf95e7ff5870f079cc4e50f478d`. The initial offline source manifest covers 51 daemon and fixture files and has SHA-256 `14d0dc6ffb55bdbd2f67a34468479b76dea5830c66411133fcc5299e713e3dd3`. The macOS release executable hash is `8e104e1076eab84f47aaa1c82ce41fb8f0c932c9f7597f75913f8fea0ed63ce8`; the Linux release executable hash is `c7dcc331bc2c79289f6670af1eb77405aaa90d4c9474644e5e7f1435ff285e9b`. Both executables were built, not run.

## Earlier CLI blocker

Before this implementation, Claude's Mac fallback used `/usr/bin/security`, which has no documented authentication-UI disable mode. Parent-process suppression does not configure the executable after `exec`. The Rust implementation replaces that fallback with native Keychain calls under the process-wide interaction-suppression guard; the claim remains subject to the unrun disposable write proof and owner acceptance.

## Disposable native proof passed

On November 3, 2026, the main session ran the independently reviewed harness once, under the owner's disposable-only authorization. Formatting, Clippy and all 23 offline tests passed before execution.

The standalone executable checked `SecKeychainSetUserInteractionAllowed(false)` before creating its private fixture. Every item operation used that fixture's explicit reference. Reads used `SecKeychainFindGenericPassword`, not a default search or a query against owner items.

The allowed read matched its synthetic data. The restricted-item read returned an access-denied result without data, bracketed by successful allowed reads. After the harness confirmed that the fixture was locked, its allowed-item read also returned an access-denied result without data. This was a read-only proof; it did not exercise modify authorization or writes.

The process completed within its 20-second alarm and printed only this fixed result:

```text
Disposable Keychain proof passed: allowed read matched; ACL and locked reads were denied without data.
```

This establishes the three synthetic cases for this executable on the installed framework. It does not establish access to actual Cursor or Muse items, future installed signing-identity compatibility, or the Claude subprocess's safety. No provider requests or owner credential-item reads ran. The test did not independently compare Keychain preferences before and after execution.

## Source evidence and limits

The earlier investigation inspected SDK headers, cached Rust crate sources, Apple's public source and documentation, and the installed CLI manual. Static binary inspection supported the source findings. That investigation performed no Keychain operations. The executed proof above is separate evidence.

The inspected host reported macOS 26.6.2, build 25G83. Apple source links below use commit `db15acbe6a7f257a859ad9a3bb86097bfe0679d9`. That source does not establish the installed framework's runtime behavior.

## Native suppression is a candidate

Apple's [interaction-control documentation][interaction-doc] says Keychain calls can return errors instead of displaying UI, including automatic unlock dialogs. The legacy framework stores this setting in [process-local globals][globals-header]. It defaults to enabled and selects [non-prompting credentials when disabled][globals-source]. Legacy [item reads use those credentials][item-read].

The candidate is a checked `SecKeychainSetUserInteractionAllowed(false)` call before any fixture access or worker threads. Keep suppression enabled throughout the dedicated helper's lifetime. Every helper must initialize its own policy. Do not extend this claim to other executables or every Security backend.

Use an explicit `kSecMatchSearchList = [fixture]` for each read. Apple's [query classifier][query-classifier] routes that attribute to the legacy backend. Never retry against the default search list.

The local SDK's `SecItem.h:1016–1020` warns that `kSecUseNoAuthenticationUI` only applies to Data Protection items. Legacy items can still activate UI. That query flag alone is not the legacy safety mechanism.

### Rust guard can re-enable UI

The cached [security-framework 3.7.0 implementation][rust-keychain] disables UI at lines 98–102. Its `KeychainUserInteractionLock::drop` unconditionally re-enables UI at lines 245–248. It neither restores the previous state nor counts nested guards.

A nested guard dropping, or panic unwinding, can therefore re-enable UI unexpectedly. For the dedicated helper, use the checked sys setter directly rather than this guard. Audit against other code re-enabling interaction.

## Private creation avoids search-list changes in source

Apple's [private-Keychain predicate][private-predicate] explicitly excludes ordinary private paths from search-list updates. Exceptions include `/Library/Keychains/System.keychain` and any pathname containing `/login.keychain`. The [creation callback][created-source] guards both search-list insertion and automatic default selection with that predicate.

Use a unique absolute path inside a private temporary directory. Reject those special paths, including `/login.keychain` anywhere in the path. Do not snapshot and restore preferences: restoration itself would violate the isolation constraint.

Apple's [creation documentation][create-doc] supports private temporary Keychains. It also says the creation API's `initialAccess` argument is ignored. Specify synthetic item ACLs when creating each item instead.

Avoid `SecKeychainDelete` for strict cleanup. Its [removal implementation][remove-source] enters search-list and default preference machinery and calls `save()`. After all helpers exit and release references, remove only the newly created fixture directory through filesystem cleanup.

These findings support a harness design. They do not prove that creation has no preference side effects on the installed framework.

## Claude subprocess cannot enforce no UI

The installed manual, `/usr/share/man/man1/security.1:43–78`, lists no authentication-UI disable option. Apple's [global option parser][cli-options] agrees. Its hidden recovery option is not such a mode.

The [interactive loop][cli-interactive] only reads and executes commands. A scoped [lock command][cli-lock] locks the named database; it does not suppress subsequent unlock dialogs. Closing stdin, piping commands or killing a timed-out subprocess does not prevent GUI prompts.

A password read reaches [SecKeychainItemCopyContent][cli-read]. An update reaches [SecKeychainItemModifyContent][cli-update]. Supplying ACL changes additionally reaches `SecKeychainItemSetAccess`, which Apple's source comments “will prompt.”

An unlocked disposable item that trusts `/usr/bin/security` could support a separately approved positive read, update and confirming read. Use exact synthetic identifiers and an explicit fixture path. Send synthetic update data through stdin and omit ACL-changing options. Such a test establishes only the observed happy path, not locked or ACL-denied safety. Also, [`-U` falls back to adding after an update failure][cli-upsert]; it is not an existing-item-only primitive.

The blocker combines three requirements: use the trusted CLI, read and renew unattended, and never prompt when locked or denied. No supported suppression mechanism inside that executable was found. A successful positive test or an unlocked-state preflight cannot close this gap.

## Safe next steps

Do not run the expanded proof or access owner items yet. The proof still needs independent source and API review and a separate explicit owner GO. After that gate, owner-present authorization, stable signing identity, and bounded provider acceptance remain separate steps. Do not use `/usr/bin/security`, change ACLs from daemon code, or weaken the interaction policy.

[interaction-doc]: https://developer.apple.com/documentation/security/seckeychainsetuserinteractionallowed(_:)
[globals-header]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Globals.h#L48-L69
[globals-source]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Globals.cpp#L36-L73
[item-read]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Item.cpp#L1818-L1820
[query-classifier]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/SecItem.cpp#L4214-L4233
[rust-keychain]: https://docs.rs/crate/security-framework/3.7.0/source/src/os/macos/keychain.rs
[private-predicate]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/StorageManager.cpp#L119-L135
[created-source]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/StorageManager.cpp#L505-L538
[create-doc]: https://developer.apple.com/documentation/security/seckeychaincreate(_:_:_:_:_:_:)
[remove-source]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/StorageManager.cpp#L999-L1055
[cli-options]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/SecurityTool/macOS/security.c#L1110-L1157
[cli-interactive]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/SecurityTool/macOS/security.c#L1173-L1205
[cli-lock]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/SecurityTool/macOS/keychain_lock.c#L49-L75
[cli-read]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/SecurityTool/macOS/keychain_find.c#L408-L419
[cli-update]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/SecurityTool/macOS/keychain_add.c#L131-L151
[cli-upsert]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/SecurityTool/macOS/keychain_add.c#L183-L198
