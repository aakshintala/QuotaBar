# QuotaBar staged consumers (not activated)

This private bundle contains the staged Pi quota consumer source/test changes and a proposed minimal shell wrapper with isolated tests. Nothing here is installed or activated.

## Pi consumer

`source/extensions/status/quota.ts` keeps the existing direct `GET /quotas` client, 8-second tool timeout, 5-second footer timeout, shared in-flight request, local freshness cache, and existing Pi tool/command registration. It validates feed schema v1 and requires nullable properties to be present, then reports ages from `capturedAt`, status, balance/count/percentage values, reset text, and actionable safe error-code guidance. The tool output remains capped at 400 characters. Tests use synthetic loopback feeds only.

Run the package's documented suite in the private source copy with `npm test`. No dependency or package-lock changes are needed.

## Shell wrapper proposal

`aiquota` forwards to the known daemon binary with `quotabard quota "$@"`, preserving `QUOTA_FEED` through normal environment inheritance. It does not fetch, format, inspect credentials, install, or start a service. The shell tests use temporary HOME directories, fake `uname`, and a fake executable; they never run the production binary or an actual quota client.

Run with `node --test bundle/aiquota.test.mjs` from the stage root, or from this directory with `node --test aiquota.test.mjs`.

Supported binary locations in the proposed wrapper:

- Linux: `$HOME/.local/bin/quotabard`
- macOS: `$HOME/Library/Application Support/QuotaBar/bin/quotabard`

## Boundaries

The staged wrapper is named `aiquota`; it is not copied to `~/.agents/bin/quota`, which remains unchanged until a separately approved cutover. No daemon binary was copied or run, no provider/API request was made, and no settings, service, hook wiring, Pi installation, or root project source was changed. The existing Pi footer renderer lives in `extensions/status/footer.ts`; it is outside this lane's allowed file scope, so this staged update does not add stale/error annotations to the visual footer itself. Its shared fetch contract and v1 feed pass-through remain unchanged; detailed freshness/status is shown by `get_quotas` and `/quota`.
