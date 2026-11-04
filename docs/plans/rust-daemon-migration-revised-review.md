# Confirmation review of the revised Rust migration plan

## Verdict

**READY FOR OWNER.** The plan now resolves the earlier findings and the three follow-up clarifications. This confirmation checks those specific edits; it is not implementation evidence.

## Resolved findings

1. **Installed Claude Code hooks:** The plan preserves both installed POST routes, their response envelope, SessionStart startup context and cached-only per-prompt warnings. The prompt hook keeps bounded severity history and never probes providers.
2. **Pi request path:** Footer and tool use the same on-demand `GET /quotas` path and shared cache. The plan removes the cached-only query variant, sets provider wait to 4.5 seconds within the five-second service response ceiling, and raises the footer timeout to eight seconds.
3. **SessionStart deadline:** The existing POST handler uses the shared service directly, waits up to 1.5 seconds and returns within the installed two-second curl timeout. It does not issue a nested GET.
4. **Keychain test safety:** The plan tests disposable items first, including a locked-keychain case. It tests real owner items only after that proof, with the owner present and explicit opt-in. It stops if the disposable test fails or access could prompt. Real-login writes are excluded from the spike.
5. **Corrupt cooldown fallback:** The plan defines a five-minute recovery only for corrupt or unreadable records; it is not a normal failure cooldown. It now atomically saves `notBefore = now + 300 seconds` before waiting, so a restart honors the same deadline. Failed persistence blocks that provider.
6. **Claude guard behavior:** The plan reserves a 300-second guard before every attempt only for Muse. Claude retains its five-minute successful-result cache and persists its actual `Retry-After`, or the existing 300-second fallback after a 429 without a usable header. It does not reserve a new five-minute guard before each Claude request.
7. **Initial eligibility:** `GET /quotas` starts an eligible check when an enabled provider has no successful reading or its successful reading is at least five minutes old. Both cases honor cooldowns and in-flight checks.
8. **Manual configuration:** The daemon is the sole programmatic writer. It reloads and validates manual edits at request boundaries, serializes CLI mutations and documents the narrow race with simultaneous external edits.

## Scope and evidence

The plan remains on-demand, has no general saved quota observations, and keeps the agreed credential and provider boundaries. It is ready for implementation planning; no implementation, provider request, credential access, service change, build or test was performed in this confirmation round.
