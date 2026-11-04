# Main ruling: prove supported Keychain write behavior

The [disposable-account ruling](rust-native-proof-account-isolation.md) supplies the execution boundary for both native proofs. The owner created the standard `test` account (UID 502). Legacy item creation can internally fall back despite an explicit fixture reference; strict account/audit guards protect the original owner's Keychain. Never run these proofs directly from the owner session. Source review, signing and execution approvals remain separate.

This ruling supersedes the raw write-denial requirements in the [signed Mac plan](rust-signed-mac-keychain.md) and source review finding R1's assumed fixture. It adopts the [primary-source analysis](../research/rust-native-keychain-write-semantics.md).

The repeated premise was that a generic-password item supplies an independent, application-specific data-write permission that can be denied while reads remain allowed. The first plan lacked write evidence; its revision assumed such a fixture existed; implementation then selected DB_MODIFY without establishing enforcement. Source review refuted that assignment. Stop replacing authorization tags and prove the selected API's supported behavior instead.

## What stays required

Independent hosts; checked lifetime suppression before native access; no runtime UI; exact fixed-service and OS-account selection; authorized fresh read and comparison before production modification; existing item only; NULL update attributes; no caller-requested ACL changes (framework metadata regeneration is not byte-identical ACL preservation); whole-record post-write reload; terminal failed/malformed read-back; no upsert, stale retry or rollback. The accepted compare/modify race remains best effort, not CAS. No signing, native proof execution, owner-item operation or permission change is authorized by this ruling.

## Corrected native proof

The selected raw API does not establish independent per-item data-write authorization. Do not fabricate a DB_MODIFY, Encrypt or similar denial ACL. Do not claim that a Decrypt-denied ACL or locked Keychain must reject every raw data replacement.

The mandatory native proof uses only new private synthetic fixtures and checked no-UI policy:

1. Unlocked readable control: invoke data-only modify on its retained existing fixture item reference. Require success and exact replacement read-back.
2. Retain the native read-denied fixture and readable positive controls. Do not perform a blind mutation of the read-denied fixture. No separately authorized verifier, additional ACL repair or helper framework is needed.
3. Confirm the fixture is locked and fresh secret reads are unavailable. Invoke raw data-only modify on a retained readable-control item reference, not through a new lookup. Record that modify was actually attempted, its content-free outcome, and post-call lock state. Accept success or a recognized refusal, never a lookup failure, arbitrary API error or unverified result.
4. Unlock only that private fixture with its known synthetic password. A successful raw modify must have the exact replacement bytes; a refused modify must have the exact original bytes. Failure plus changed data, success plus wrong data, failed verification, unexpected UI or timeout fails the proof. Keep allowed control evidence and cleanup explicit.

The proof must describe this as observed no-UI/raw-update consistency, not write-authorization denial. The raw API is not being advertised as protection from unrelated blind writers.

## Production adapter evidence

The actual production sequencing algorithm must run under injected fake-native operations in ordinary tests. Failed suppression, metadata lookup, denied/locked/unavailable fresh reads, missing item, oversized content or malformed native results must cause zero modify calls. Assert the fake backing record is unchanged and all acquired content/references are released. Known-changed records return actual complete data without modifying. Equal records modify once and reload actual data. Mock evidence and native fixture evidence must be reported separately, not conflated.

The mandatory native proof omits direct blind modification of a Decrypt-denied fixture: inspecting its final data would require extra privileged fixture machinery, while that blind path is forbidden in the production algorithm. This is an intentional scope choice, not a passing test for an omitted case.

## Adjacent scope settlements

- R3: fix shared credential-parent opening once with descriptor no-follow protection and typed unsafe-path errors. This deliberate security hardening applies to both hosts and all shared callers. It is the explicit exception to the earlier unchanged-Linux scope. HOME remains the existing trusted boundary; no new ancestor-walking framework.
- R4: restore the prior Claude file-refresh post-commit behavior. Do not add whole-record file reload in this lane. The mandatory post-modify whole-record reload is for the new Keychain source.

Repaired code requires scoped independent verification against the original five source findings plus this ruling. Native proof execution remains blocked until that review passes and the owner separately approves a bounded run.

## Additional owner-write gate: cross-identity access

The [source analysis](../research/rust-native-keychain-write-semantics.md#cli-access-preservation-boundary) cannot establish continued official-CLI access after a differently signed writer updates an item. This concerns effective access, not merely regenerated metadata.

Before any real Claude Keychain write:

1. Independently review a separate private synthetic cross-identity proof. Prefer one-off native tooling outside production source, not extra daemon modes or a credential framework.
2. Use an ad-hoc helper A and Developer-ID-signed helper B. Signing B needs separate owner-authorized private-key use. Never impersonate a vendor or inspect, invent, copy or restore partition-ID values.
3. Create only a new private fixture Keychain with fixed synthetic service/account/data and an explicit application access list containing both helpers. Each process checks no-UI policy and uses explicit fixture references, never owner/default/search-list access.
4. Require fresh A and B processes both to read the exact initial data. Initial refusal is inconclusive and blocks owner writes; do not guess an ACL repair.
5. Have B perform the same existing-item, NULL-attribute data update. Require fresh A and B processes both to read the exact replacement data. Refusal, mismatch, UI or an unverifiable step blocks owner writes. This is cross-identity evidence, not a universal policy guarantee.
6. Only then request separate bounded owner-write approval. Verify official-CLI access before and after through an appropriate official-CLI check, retaining only allowlisted status metadata. No opaque ACL restoration, stale rollback, token export or security-CLI workaround is authorized.

The basic private no-UI/write-consistency proof can pass separately without closing this gate. Do not run a real Mac provider check that could automatically renew the Keychain record until the owner-write gates are satisfied. Planning and signing alone close none of them.
