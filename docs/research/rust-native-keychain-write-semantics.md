# Native Keychain write semantics

Main adopted the revised contract in the [write-proof ruling](../plans/rust-native-write-proof-ruling.md). This source-analysis memo records the reasoning and evidence limits; it does not approve code or native execution.

## Answer for Main

The reviewed API does not document or expose the assumed independent, application-specific password-item write permission that can be denied while Decrypt remains allowed. The cached Apple implementation does not establish such enforcement either. Do not replace DB_MODIFY with Encrypt or another guessed tag.

A direct blind data replacement may succeed despite a Decrypt-denied item ACL. That is consistent with the inspected implementation, not evidence of a no-UI failure. Decrypt authorization protects retrieving the old plaintext; it is not a general promise against overwriting that plaintext.

Main needs to revise the overstrong raw write-denial proof requirement. Keep runtime no-UI, authorized fresh read before compare/modify, existing-item-only updates, whole-record read-back, and the accepted best-effort race limits. The original requirement does not require inventing a macOS per-app integrity boundary.

Current proof execution remains BLOCKED. This is an R1 semantic clarification and a proposed contract, not approval of revised code or any execution. S1/R2/R3/R4 repairs are outside this memo.

## Evidence boundary

Read only cached public Apple source at commit `db15acbe6a7f257a859ad9a3bb86097bfe0679d9` and local macOS 26.5 SDK headers. The cached source files used below match their Git blob hashes in the cached Apple indexes. Links identify the public files and exact lines; no HTTP retrieval occurred.

Source explains one legacy file-backed Keychain implementation. It does not prove the behavior of every installed framework, backend, partition policy, or owner item. No native calls, credentials, experiments, tests, builds, signing, services, code edits, or Git writes occurred. Only this memo was written.

## Authorization path

| Step | Primary-source evidence | Consequence |
|---|---|---|
| API entry | [SecKeychainItem.cpp:173–183][entry] calls `ItemImpl::modifyAttributesAndData`. Local SDK `SecKeychainItem.h:156–167` describes an existing-item update, NULL attributes for no attribute changes, and a result code. | No compare-and-swap, no per-call UI flag, and no documented independent item-write authorization guarantee. |
| Install replacement bytes | [Item.cpp:1434–1480][modify] assigns new `mData` when the caller supplies data, then calls `update`. | It need not read the old secret first. The daemon's read-before-write is an application safeguard, not this raw API's prerequisite. |
| Choose storage path | [Item.cpp:821–869][update] sends password data to `updateSSGroup`; [Item.cpp:1828–1845][storage-kind] identifies password classes using secure storage. | Do not infer password data-write policy from a tag's name. |
| Replace encrypted storage | [Item.cpp:888–990][new-group] obtains the old SSGroup, copies its access policy, removes internally regenerated partition/integrity entries, and chooses a new SSGroup when new data exists. It deletes the old record, creates the group under temporary creation credentials, inserts new data, applies access policy, and commits a transaction. | This path does not demonstrate checking an old per-item DB_MODIFY right or decrypting the old plaintext. Internal integrity regeneration is not a caller-selectable per-app write ACL. |
| Encrypt new data | [securestorage.cpp:230–268][group-key] creates a permanent symmetric key under the supplied creation context. [securestorage.cpp:186–200][insert] encrypts the replacement using that group before inserting it. [Access.cpp:357–398][maker] constructs temporary creation credentials; [Access.cpp:185–205][apply] uses them to finalize the new group's access policy. | The replacement need not possess the old item's Decrypt authorization. This is not a recommendation to deny Encrypt: modern data replacement uses a new group. |
| Read old data | [Item.cpp:1789–1825][read] obtains `globals().itemCredentials()` and calls secure storage `get`. [securestorage.cpp:315–345][decrypt] decrypts the old ciphertext with the group's key and supplied credentials. | Reads have a different authorization path from blind replacement. |

[Access.cpp:83–116][standard] creates standard Change-ACL ownership, unrestricted Encrypt, and restricted Decrypt/Sign/MAC/Derive/Export rights. It creates no item DB_MODIFY entry. [SecAccess.cpp:77–131][mapping] maps `kSecACLAuthorizationKeychainItemModify` to `CSSM_ACL_AUTHORIZATION_DB_MODIFY`; the name alone does not establish item-secret enforcement.

Thus the old fixture's zero matching ACLs are not a reason to fabricate one. Nor should a successful blind write to a read-denied fixture be labeled an authorization bypass by the daemon: production must never take that blind path after a failed fresh read.

## UI policy and locking

[SecKeychain.cpp:866–875][setter] implements `SecKeychainSetUserInteractionAllowed` by setting the process-local flag in [Globals.h:43–65][globals-h]. [Globals.cpp:57–74][globals] selects cancel credentials instead of unlock credentials, and null credentials instead of item-prompt credentials, when UI is disabled. Ordinary item reads use those item credentials.

Database activation installs the default-credential hook at [Keychains.cpp:399–408][activation]. [Keychains.cpp:1847–1872][defaults] normally chooses the global Keychain credentials, with separate paths for custom referral credentials and smart cards. The new-data update path also uses explicit non-prompting creation credentials described above. There is no independent `noUI` argument on the modify API.

The local SDK `SecKeychain.h:634–641` describes disabling optional user interaction. Its `:346–367` describes explicit unlock/lock and warns that operations requiring an unlocked Keychain may automatically call unlock. [Keychains.cpp:573–594][lock] delegates locking and explicit unlock to the database. These are not a documented promise that every raw item update on every locked backend must return one particular denial status.

Creating a new permanent group key can require database/key access, so a locked write can refuse under disabled interaction. But this source review cannot promote that expectation into a universal raw-write-denial guarantee. Neither a retained item reference nor confirmed lock status proves which later operation failed. Record the actual modify result and lock state, then verify data after controlled synthetic-password unlock.

Accepting either raw locked success or refusal is defensible for a revised no-UI/data-consistency proof, provided Main explicitly adopts that narrower claim. It would prove the observed outcome for that fixture, not that locked writes are forbidden. A successful result must have exact replacement data; a refused result must leave exact original data. Error plus changed bytes, success plus different bytes, unverifiable state, unexpected dialog, or timeout fails the proof.

A failed metadata lookup is never evidence of modify denial. Invoke the raw update on a retained fixture item reference and record that call's result separately.

## Proposed replacement contract

This proposal needs Main's ruling and subsequent source review before execution.

### A. Raw API proof on private synthetic fixtures

Keep checked lifetime UI suppression before every real native operation, explicit private fixture references, NULL update attributes, known synthetic bytes, and no owner/default/search-list operations. Retain item references at creation; do not reacquire them through a potentially failing lookup to test a mutation.

| Fixture state | Required observation | Claim allowed |
|---|---|---|
| Unlocked and readable | Direct modify is invoked, succeeds, and exact read-back matches replacement. | Positive control: this API actually updated the existing fixture item without UI. |
| Read denied by the fixture ACL | Confirm read denial without data, bracketed by an unlocked readable control. Invoke direct modify on the retained reference, separately from that read. Success or refusal is possible; verify replacement on success or original data on refusal using a reviewed fixture-only verification capability. | Read denial and the actual independent raw-update outcome, not an invented write authorization. |
| Locked | Confirm lock status and expected synthetic read unavailability. Invoke direct modify on the retained reference. Record success/refusal and post-call lock state. Unlock only with the known synthetic fixture password, then verify exact replacement/original data according to the result. | No-UI raw-update behavior and consistent data on this locked fixture, not guaranteed write denial. |

Read-denied data verification has a real limit: the denied identity cannot simply read back data and call that a positive control. Any separately authorized verifier must be synthetic-fixture-only and independently reviewed; do not change owner ACLs, export credentials, or silently enable UI. If that extra fixture mechanism is unwanted, Main can omit the direct blind read-denied mutation from the mandatory native proof. Retain native read-denial evidence and section B's no-modify proof instead. Do not claim an omitted or unreadable data-state check passed.

Do not accept arbitrary errors as authorization evidence. Record fixed, content-free outcome categories and distinguish lookup, read, mutation, and verification failures. A non-authorization refusal may demonstrate no-UI refusal with intact data, but not ACL denial. Any unexpected result stays inconclusive/failing rather than being relabeled success.

### B. Production conditional-store proof

Test the actual shared adapter sequence through fake native operations, without even the real suppression setter in ordinary tests:

1. Require suppression, exact no-secret metadata lookup, and an authorized bounded fresh secret read before comparison.
2. On read denial, locked/read-unavailable state, missing item, oversized data, or read error, return a safe error. Assert zero modify calls and unchanged backing synthetic bytes. Never count this as a raw modify-denial test.
3. On changed fresh bytes, return the complete current record without modification. On equal bytes, invoke existing-item data-only modify once, then reload the complete actual record. Preserve unknown JSON and make failed/malformed post-write reload terminal.
4. Verify synthetic locked behavior natively where a readable post-unlock check is available. For a read-denied item without a separate authorized reader, zero mutation calls prove the adapter's no-write path by construction; fake backing-state assertions supply the exact-byte check. State that evidence split explicitly.

This is the relevant production protection: the daemon cannot replace a secret it was unable to read and compare. It does not make the raw Keychain API resistant to an unrelated blind writer. The narrow compare/modify race remains best effort, not CAS; source transactions do not establish an unconditional no-data-loss guarantee for all framework failures. Preserve exact checks and stop on inconsistency rather than retrying stale writes or rolling back blindly.

## CLI access preservation boundary

The cached source does not establish that the official CLI can still read after a differently signed writer updates the item.

- [Access.cpp:123–132](https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Access.cpp#L123-L132) copies the old group's owner and ACL entries.
- [Item.cpp:924–933, 959–990](https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Item.cpp#L924-L990) removes PARTITION_ID and INTEGRITY. New data creates a new SSGroup, adds recomputed integrity and applies the remaining access policy. This branch does not restore old partition values.
- [Access.cpp:187–211](https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Access.cpp#L187-L211) replaces temporary creation access with the remaining entries and owner.

Ordinary application ACL entries are copied, but downstream partition generation and its inputs are not established by this trace. Do not claim either preserved effective access or definite writer-Team retargeting. Same-process read-back cannot prove another signer's access.

The [ruling](../plans/rust-native-write-proof-ruling.md) therefore requires synthetic cross-identity evidence before any owner write, followed by a separately approved owner-write test that verifies official-CLI access before and after. No opaque ACL restoration, invented partition values, relaxed trust or security-CLI workaround is authorized. Failure or inconclusive evidence blocks owner writes.

## Ruling requested

Replace “prove a readable item has independently ACL-denied raw data writes, and every raw locked write is denied” with “exercise raw modify without UI and verify its actual data-consistent outcome; separately prove the production conditional store never modifies after a failed authorized fresh read.”

Keep all later owner approval, signed-app-only authorization, installed-path reacceptance, and natural-renewal evidence gates. No permission or execution gate closes through this memo.

[entry]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/SecKeychainItem.cpp#L173-L183
[modify]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Item.cpp#L1434-L1480
[update]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Item.cpp#L821-L869
[storage-kind]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Item.cpp#L1828-L1845
[new-group]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Item.cpp#L888-L990
[group-key]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_cdsa_client/lib/securestorage.cpp#L230-L268
[insert]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_cdsa_client/lib/securestorage.cpp#L186-L200
[maker]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Access.cpp#L357-L398
[apply]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Access.cpp#L185-L205
[read]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Item.cpp#L1789-L1825
[decrypt]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_cdsa_client/lib/securestorage.cpp#L315-L345
[standard]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Access.cpp#L83-L116
[mapping]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/SecAccess.cpp#L77-L131
[setter]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/SecKeychain.cpp#L866-L875
[globals-h]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Globals.h#L43-L65
[globals]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Globals.cpp#L57-L74
[activation]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Keychains.cpp#L399-L408
[defaults]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Keychains.cpp#L1847-L1872
[lock]: https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/OSX/libsecurity_keychain/lib/Keychains.cpp#L573-L594
