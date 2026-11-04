# Rust daemon migration plan review

> Historical review of the earlier migration-plan draft. The owner later chose on-demand checks, no saved quota snapshots, no Mac UI, and other scope changes. This verdict does not review or approve the current plan.

Verdict for the earlier draft: READY FOR OWNER

I rechecked only the runtime contract, Phase 4, the verification matrix, and this review's prior findings. No tests or provider requests were run; listed gates remain planned, not verified.

## Prior blocking findings resolved

1. **Restored snapshots are credential-gated.** The runtime contract now keeps persisted observations hidden until a local credential check validates the current source and binding. It distinguishes stable account fingerprints from exact-credential SHA-256 digests, explicitly says the digest is not an account ID, and invalidates old quota and cost data for missing, unreadable, denied, changed, or auth-rejected credentials. It also checks credential generation before publishing results. The verification matrix covers first GET, credential failures, changed credentials, stable bindings, and providers without stable IDs. This resolves the prior account-data exposure gap.

2. **Cooldown timing and restart behavior are explicit.** The contract uses monotonic time for in-process delays and freshness, and wall time only for persisted audit fields and wire dates. On restart it rearms the full persisted guard duration without crediting wall-clock downtime, explicitly accepting over-wait. It documents the limit for a crash before a received `Retry-After` is persisted, blocks on failed backoff persistence, and requires operator repair for corrupt or invalid state rather than silently resetting a guard. The verification matrix covers clock jumps, repeated restarts, persisted retry durations, and invalid-state recovery. This resolves the prior timing gap.

3. **macOS acceptance covers all five providers.** Phase 4 now has a provider-by-provider credential and mocked-probe matrix for Claude, Codex, Cursor, OpenCode Go, and Muse, with no provider API calls. It retains Claude's `/usr/bin/security` boundary and requires the actual Muse Keychain backend to work without UI as the LaunchAgent identity. Cutover requires all five cases to pass. This resolves the prior Mac coverage gap.

## Prior nonblocking concern resolved

- The runtime contract now persists removal of expired quota buckets so a backward wall-clock jump cannot restore them. The verification matrix tests expired quota and cost windows against that case, and Phase 4 acceptance includes cost-row reset expiry. The prior cost-reset concern is resolved.

The prior three blockers and cost-reset concern are resolved in the reviewed sections. No remaining blocker was identified within this confirmation's scope.
