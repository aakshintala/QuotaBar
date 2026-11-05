# Rust quota daemon credential research

As of November 3, 2026, all five providers have usable-looking Linux credential sources. An owner-authorized, single Cursor quota request returned HTTP 200 and the expected quota objects using the existing CLI access token. This proves that login works with the endpoint now, not that Cursor supports this interface or guarantees future compatibility.

The initial subagent investigation was read-only and made no provider requests. It did not install software, invoke Cursor CLI commands, change credentials, change the remote host, or commit. It fetched only public Cursor documentation pages. Credential checks printed file permissions, JSON field names, and presence or shape booleans, never credential values or account identities. The orchestrator then made the one quota request described below with the owner's explicit approval.

## Cursor CLI on the Linux host

The inspected host is `roundhouse-7010-plus-1.local`.

| Item | Observed evidence |
|---|---|
| Operating system | Ubuntu 26.10, Linux x86_64, kernel `7.3.0-9-generic` |
| Agent executable | `~/.local/bin/agent` is a symlink into `~/.local/share/cursor-agent/versions/2026.10.22-27dcf5f/` |
| Installed runtime | The target `cursor-agent` is a shell wrapper that starts bundled Node and `index.js`; its package manifest names `@anysphere/agent-cli-runtime` |
| Build version | Bundled source stamps `2026.10.22-27dcf5f`; the manifest has no version field. I did not run `agent --version` because even a version invocation would start the CLI |
| `cursor` command | A separate shell shim. It runs `agent` only for the `cursor agent` form when no Cursor IDE executable is found |
| Auth file | `~/.config/cursor/auth.json` exists with mode `0600`; its parent directory has mode `0700` |
| Auth fields | The JSON has `accessToken` and `refreshToken` string fields. Both values have JWT shape; the access-token payload has `sub` and `exp` claims. Values and claims were not printed |

The bundled source contains login, logout, and auth-status code. Login passes access and refresh tokens to a `credentialManager.setAuthentication` method. Other code reads `getAccessToken()` and sends bearer authorization to Cursor backend services. This confirms that the CLI manages refreshable login credentials, but the inspected source did not expose the credential manager's Linux path-selection implementation.

The CLI wrapper recognizes `AGENT_CLI_CREDENTIAL_STORE=file`; that environment variable was unset during inspection. The file and its token fields are strong evidence of file-backed Cursor auth on this host, but the inspected source did not prove that this is the selected backend or document its default. Cursor's public docs say credentials are stored securely locally but do not name the path or storage backend.

The installed source tree contained no literal `usage-summary` or `cursor.com/api/usage-summary` string. The official CLI docs describe browser login, API-key auth, and local credential storage. They do not document quota-summary access.

## Comparison with the existing Cursor probe

`Sources/Infrastructure/Cursor/CursorUsageProbe.swift` currently:

1. reads `cursorAuth/accessToken` from the macOS Cursor app database at `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`;
2. decodes the JWT `sub` claim as the user ID;
3. sends `WorkosCursorSessionToken={userId}::{accessToken}` as a cookie to `https://cursor.com/api/usage-summary`.

The Linux CLI auth file has a JWT-shaped access token and a `sub` claim. The orchestrator verified that the same cookie construction works for the current login in the authorized test below. The CLI source uses bearer auth for its backend calls; the desktop probe uses a WorkOS session cookie for an undocumented usage endpoint. The CLI does not document this use.

The inspected host had no `~/.config/Cursor` directory, so I found no Linux Cursor IDE `state.vscdb` source at the usual configuration location. This does not prove that no other Cursor installation or database exists.

## Credential compatibility for the other providers

Each file below existed on the Linux host with mode `0600`. The path and field checks establish format compatibility only. They do not establish that tokens are current, unexpired, or accepted by provider APIs.

| Provider | Expected source and fields in QuotaBar | Linux host shape check | Compatibility |
|---|---|---|---|
| Claude | `~/.claude/.credentials.json`; `claudeAiOauth.accessToken` and optional `refreshToken` | File exists; both token fields are non-empty strings | Compatible through the file loader. QuotaBar's alternate Claude Keychain path is macOS-specific; the host also had no `CLAUDE_CODE_OAUTH_TOKEN` environment variable |
| Codex | `~/.codex/auth.json`; `tokens.access_token`, `tokens.refresh_token`, `tokens.account_id`, plus `last_refresh` | File exists; access and refresh token fields are non-empty strings | Compatible through the file loader |
| OpenCode Go | `$XDG_DATA_HOME/opencode/auth.json`, or `~/.local/share/opencode/auth.json`; `opencode-go.type` and `opencode-go.key` | XDG override unset; default file exists; `opencode-go.key` is a non-empty string | Compatible through the file loader |
| Muse | `$XDG_CONFIG_HOME/muse/auth.json`, or `~/.config/muse/auth.json`; `providers.meta` with OAuth fields `access_token` and `api_key` | XDG and `MUSE_AUTH_PATH` overrides unset; default file exists; OAuth shape matches; the token has the expected `dca:` prefix; `api_key` is a non-empty string; storage is not marked as Keychain | Compatible through the file loader. The standing API key is required; QuotaBar does not issue one |

QuotaBar's loader definitions are in `Sources/Infrastructure/Claude/ClaudeCredentialLoader.swift`, `Codex/CodexCredentialLoader.swift`, `OpenCode/OpenCodeCredentialLoader.swift`, and `Muse/MuseCredentialLoader.swift`. The Rust implementation can use these formats as observed inputs, but should keep provider-specific refresh and persistence behavior out of a generic file reader until separately specified.

## Cursor options and remaining risks

| Option | Assessment |
|---|---|
| Use a supported Cursor quota API and supported auth flow | Preferred. No quota API or quota-auth contract appeared in the installed CLI source or the official CLI pages reviewed |
| Read the CLI access token and construct the existing session cookie | Verified for the current Linux login by one successful request. It still couples the daemon to an undocumented auth file and quota endpoint; token expiry or CLI rotation can break it |
| Use `CURSOR_API_KEY` | The CLI docs support API keys for CLI use. They do not say that this key works with the usage-summary endpoint, so this is not a demonstrated quota solution |
| Use Cursor IDE's SQLite database | This matches the current macOS probe. No such Linux database was observed on the inspected host |

The initial credential-compatibility blocker is resolved. Use the Linux CLI auth file for Cursor, reread it on authentication failure, and do not invoke the CLI to refresh credentials. Expiry, CLI rotation, a different account and alternative credential backends remain unverified. Surface an actionable login error rather than inventing quota data.

## Authorized Cursor endpoint check

On November 3, 2026, the owner approved exactly one read-only quota request from the Linux box. The orchestrator ran a Python process over SSH that read `~/.config/cursor/auth.json` in memory, decoded `sub`, validated header-safe characters, and made one `GET https://cursor.com/api/usage-summary` with the existing cookie construction. It did not refresh credentials, write files, retry, or invoke a Cursor CLI command. The token stayed on the Linux machine and was never placed in command arguments or output.

The HTTPS request had a 15-second timeout, refused redirects, disabled environment proxies and bounded response parsing to 256 KiB. Only these fixed-label observations were printed:

```text
http_status: 200
json_object: True
individual_usage_object: True
membership_type_present: True
billing_cycle_end_present: True
plan_meter_object: True
plan_quota_numbers_present: True
onDemand_meter_object: True
onDemand_quota_numbers_present: True
```

No quota amounts, account identities, response body or token values were captured in this note. The response was checked for shape, not parsed against every existing Cursor semantic rule. This is live compatibility evidence for one login, not a published auth contract.

## Deployment facts

The local Mac reports Darwin 25.6 arm64, Rust 1.98.1, Cargo 1.98.1, and `launchctl`. It has no `systemctl`.

The Linux host reports Ubuntu 26.10 x86_64, Rust 1.99.0, Cargo 1.99.0, and a running systemd user manager. `loginctl` reports `Linger=no`; a user service will not be kept alive after logout unless the owner enables lingering or chooses a system service. No quota-named user units or unit files were found.

The Linux host has no `~/work/QuotaBar` checkout. It does have `~/work/pi-extensions/extensions/status/quota.ts` and an executable `~/.agents/bin/quota`. QuotaBar documents `/quotas` as its existing feed endpoint and identifies both clients as consumers; keep their feed contract in scope for any future daemon migration.

## Evidence commands

These are the read-only command patterns used. The JSON inspection scripts output only names and booleans, never values.

```sh
# Local Mac toolchain
uname -srm
rustc --version
cargo --version
command -v launchctl

# Linux deployment, over the explicitly approved SSH target
ssh -o BatchMode=yes roundhouse-7010-plus-1.local \
  'uname -srm; awk -F= "/^(NAME|VERSION_ID)=/ {print}" /etc/os-release; rustc --version; cargo --version; systemctl --user is-system-running; loginctl show-user "$(id -u)" -p Linger --value'

# CLI link, target type, and package identity; no CLI command is run
ssh -o BatchMode=yes roundhouse-7010-plus-1.local \
  'readlink -f ~/.local/bin/agent; file ~/.local/share/cursor-agent/versions/2026.10.22-27dcf5f/cursor-agent; python3 -c '\''import json; d=json.load(open("~/.local/share/cursor-agent/versions/2026.10.22-27dcf5f/package.json".replace("~",__import__("os").path.expanduser("~")))); print(d.get("name"), d.get("version"))'\'''

# File permissions only; use the known loader paths
ssh -o BatchMode=yes roundhouse-7010-plus-1.local \
  'for p in ~/.claude/.credentials.json ~/.codex/auth.json ~/.config/cursor/auth.json ~/.local/share/opencode/auth.json ~/.config/muse/auth.json; do test ! -f "$p" || stat -c "%a %n" "$p"; done'

# JSON shape check; print only fixed field labels and booleans
ssh -o BatchMode=yes roundhouse-7010-plus-1.local 'python3 -' <<'PY'
import base64, json, os
home = os.path.expanduser("~")
def load(path):
    try:
        with open(os.path.join(home, path), encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return {}
def has_string(obj, key):
    return isinstance(obj, dict) and isinstance(obj.get(key), str) and bool(obj[key])
claude = load(".claude/.credentials.json").get("claudeAiOauth", {})
codex = load(".codex/auth.json").get("tokens", {})
cursor = load(".config/cursor/auth.json")
open_code = load(".local/share/opencode/auth.json").get("opencode-go", {})
muse = load(".config/muse/auth.json").get("providers", {}).get("meta", {})
print("claude_access_refresh_present", has_string(claude, "accessToken"), has_string(claude, "refreshToken"))
print("codex_access_refresh_present", has_string(codex, "access_token"), has_string(codex, "refresh_token"))
print("cursor_access_refresh_present", has_string(cursor, "accessToken"), has_string(cursor, "refreshToken"))
token = cursor.get("accessToken", "")
if isinstance(token, str) and token.count(".") == 2:
    payload = token.split(".")[1]
    claims = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
    print("cursor_jwt_sub_exp_present", bool(claims.get("sub")), isinstance(claims.get("exp"), (int, float)))
print("opencode_key_present", has_string(open_code, "key"))
muse_token = muse.get("access_token", "")
print("muse_oauth_key_shape", muse.get("mechanism") == "oauth", isinstance(muse_token, str) and muse_token.startswith("dca:"), has_string(muse, "api_key"), muse.get("storage") == "keychain")
PY

# Local probe and loader source references
rg -n 'usageSummaryURL|cursorAuth/accessToken|WorkosCursorSessionToken|credentialsFilePath|authFilePath|credentialURL' Sources/Infrastructure
```

I also inspected the installed JavaScript bundles for `setAuthentication`, `getAccessToken`, `AGENT_CLI_CREDENTIAL_STORE`, and the quota endpoint literal. I read JSON field names and token-shape booleans without printing token values. The subagent ran no `agent`, `cursor`, `security`, `sqlite3`, or provider API command on the remote host. The orchestrator's later authorized request is recorded separately above.

## Official sources

- [Cursor CLI installation](https://cursor.com/docs/cli/installation) documents supported platforms, the installer, version-check command, and automatic updates.
- [Cursor CLI authentication](https://cursor.com/docs/cli/reference/authentication) documents browser login, API-key auth, status/logout commands, and local credential storage at a high level.
- [Cursor headless CLI](https://cursor.com/docs/cli/headless) documents API-key use in scripts and automation.
- [Cursor CLI ACP](https://cursor.com/docs/cli/acp) documents the CLI's `cursor_login` authentication method for ACP clients.
- [systemd `loginctl`](https://www.freedesktop.org/software/systemd/man/latest/loginctl.html) documents linger settings for user managers.
