# quotad

quotad is the localhost quota daemon behind QuotaBar. It refreshes the
five provider quotas on a background loop and serves them as JSON on
`GET http://127.0.0.1:<feed.port>/quotas` (default port 8787).

With no subcommand it runs the server: `quotad [-settings path]
[-port N] [-log path]`. `-log` appends to a file instead of stderr.

## Install on Linux

Copy the binary to `~/.local/bin/quotad`, then run:

```bash
quotad install
```

This writes the `quotad.service` systemd user unit (with ExecStart
pointing at the installed binary), seeds `~/.quotabar/settings.json`
when missing, ensures `~/.quotabar/muse` is mode 0700, links
`~/.local/bin/aiquota` to the binary, and enables the service.

## aiquota

`aiquota [--json]` is `quotad quota` under another name: it GETs the
feed (25 s timeout) and prints the same per-provider text as the
`/hooks/session-start` endpoint, or the raw body with `--json`.

## Uninstall

`quotad uninstall` (Linux only) stops and removes the user unit and
the managed `aiquota` symlink. Settings and state stay in place.
