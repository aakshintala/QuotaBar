#!/bin/bash
# Build quotad for this Mac and run it as a LaunchAgent on the feed port in
# ~/.quotabar/settings.json (8787). The Swift app must already be a feed
# client: the script stops if anything other than quotad holds the port.
# Usage: bash quotad/deploy/mac-deploy.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
label=com.aakshintala.quotad
plist=~/Library/LaunchAgents/$label.plist
port=8787

holder=$(lsof -nP -iTCP:$port -sTCP:LISTEN -Fc 2>/dev/null | sed -n 's/^c//p' | head -1)
if [ -n "$holder" ] && [ "$holder" != quotad ]; then
	echo "port $port is held by $holder; quit it first (the old Swift app serves the feed itself)" >&2
	exit 1
fi

mkdir -p ~/.local/bin ~/Library/Logs/QuotaBar ~/Library/LaunchAgents
(cd "$here/.." && CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' -o ~/.local/bin/quotad.new .)
mv ~/.local/bin/quotad.new ~/.local/bin/quotad
sed "s|HOME_DIR|$HOME|g" "$here/$label.plist" > "$plist"
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$plist"
sleep 3
launchctl print "gui/$(id -u)/$label" | grep -E '^\s*(state|pid) ='
curl -s -m 25 "http://127.0.0.1:$port/quotas" | head -c 400; echo
