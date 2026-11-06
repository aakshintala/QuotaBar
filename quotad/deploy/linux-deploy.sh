#!/bin/bash
# Build quotad for Linux, copy it to the box, and install it as a systemd
# user unit on 127.0.0.1:8787. Stops and disables the Rust quotabard unit, which
# wants the same port (re-enable it with: systemctl --user enable --now quotabard).
# Usage: bash quotad/deploy/linux-deploy.sh [ssh-host]
set -euo pipefail
host=${1:-roundhouse-7010-plus-1.local}
here=$(cd "$(dirname "$0")" && pwd)
ssh=(ssh -o BatchMode=yes -o ConnectTimeout=10 "$host")
bin=$(mktemp)
trap 'rm -f "$bin"' EXIT

(cd "$here/.." && GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' -o "$bin" .)
"${ssh[@]}" 'mkdir -p ~/.local/bin && cat > ~/.local/bin/quotad.new && chmod 755 ~/.local/bin/quotad.new && mv ~/.local/bin/quotad.new ~/.local/bin/quotad' < "$bin"
"${ssh[@]}" 'systemctl --user disable --now quotabard.service 2>/dev/null || true
~/.local/bin/quotad install
sleep 3; systemctl --user --no-pager status quotad.service | head -8
curl -s -m 25 http://127.0.0.1:8787/quotas | head -c 600; echo'
