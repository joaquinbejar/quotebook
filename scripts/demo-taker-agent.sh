#!/bin/sh
# Runs quotebook-demo-taker under launchd (macOS), on devnet only (docs/PLAN-phase2.md, D5).
#
#   scripts/demo-taker-agent.sh run <name>
#   scripts/demo-taker-agent.sh install <name>
#   scripts/demo-taker-agent.sh uninstall <name>
#   scripts/demo-taker-agent.sh status <name>
#
# `run` is one taker run by hand against the maker installed by scripts/demo-agent.sh install
# <name>, in $QUOTEBOOK_DEMO_HOME/<name>/ (default ~/.local/share/quotebook-demo): 1 lot bought
# from or sold back to the demo maker, one JSON line appended to <dir>/trades.jsonl, which the
# watcher reads (`fill-error`). It skips, writing a `skipped` line, outside market hours, inside
# the settlement window, while <dir>/HALT exists and with no inventory on either side. With
# QUOTEBOOK_TAKER_DRY_RUN=1 it makes every read and prints the decision, requests no quote,
# sends nothing and writes no line.
#
# `install` copies target/release/quotebook-demo-taker (QUOTEBOOK_TAKER_BIN overrides it) to
# <dir>, writes the LaunchAgent ~/Library/LaunchAgents/com.quotebook.demo-taker.<name>.plist and
# loads it: one run every 6 hours (StartInterval 21600), not at load. The taker is the I9 taker
# 3ZBj...sqLs; its keypair is a file path ($QUOTEBOOK_TAKER_KEY, default
# ~/.config/solana/quotebook-devnet/taker.json) and it pays its own fees. The plist holds no key
# material and no secret; the RPC URL is the public devnet one recorded by the maker's install.
# Logs: <dir>/trades.jsonl, <dir>/taker-log.jsonl (JSON log, rotated at 10 MB, 5 kept) and
# launchd's <dir>/taker-stderr.log.

set -eu

usage() {
	sed -n '4,7p' "$0" | sed 's/^# *//' >&2
	exit 2
}

[ $# -eq 2 ] || usage
cmd=$1
name=$2
case $name in
*[!A-Za-z0-9_-]* | '') echo "name must be letters, digits, - or _" >&2 && exit 2 ;;
esac

home=${QUOTEBOOK_DEMO_HOME:-$HOME/.local/share/quotebook-demo}
key=${QUOTEBOOK_TAKER_KEY:-$HOME/.config/solana/quotebook-devnet/taker.json}
maker=${QUOTEBOOK_TAKER_MAKER:-142GM9zKDbdncitgiL6zwdKquwuzzNhMPXwEm1RVYaVB}
dir=$home/$name
label=com.quotebook.demo-taker.$name
plist=$HOME/Library/LaunchAgents/$label.plist
uid=$(id -u)
if launchctl print "gui/$uid" >/dev/null 2>&1; then
	domain=gui/$uid
else
	domain=user/$uid
fi
case $maker in *[!1-9A-HJ-NP-Za-km-z]* | '') echo "QUOTEBOOK_TAKER_MAKER: not a base58 address" >&2 && exit 2 ;; esac

loaded() {
	for d in "gui/$uid" "user/$uid"; do
		if launchctl print "$d/$label" >/dev/null 2>&1; then
			echo "$d"
		fi
	done
}

unload() {
	for d in $(loaded); do
		if ! launchctl bootout "$d/$label" 2>/dev/null; then
			if launchctl print "$d/$label" >/dev/null 2>&1; then
				echo "could not unload $label from $d" >&2
				exit 1
			fi
		fi
	done
}

xml() { printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'; }

settings() {
	for f in devnet.json devnet-maker.json agent.env; do
		[ -f "$dir/$f" ] || { echo "$dir/$f missing: scripts/demo-agent.sh install $name first" >&2 && exit 1; }
	done
	url=$(sed -n 's/^url=//p' "$dir/agent.env")
	port=$(sed -n 's/^port=//p' "$dir/agent.env")
	case $port in *[!0-9]* | '') echo "$dir/agent.env: no port" >&2 && exit 1 ;; esac
	case $url in
	https://api.devnet.solana.com | https://api.devnet.solana.com/) ;;
	*) echo "$dir/agent.env: the URL is not the public devnet RPC" >&2 && exit 1 ;;
	esac
	[ -f "$key" ] || { echo "$key missing" >&2 && exit 1; }
}

case $cmd in
run)
	settings
	bin=$dir/quotebook-demo-taker
	[ -x "$bin" ] || { echo "$bin missing: install first" >&2 && exit 1; }
	dry=
	if [ "${QUOTEBOOK_TAKER_DRY_RUN:-0}" = 1 ]; then dry=--dry-run; fi
	# shellcheck disable=SC2086 # $dry is empty or one word.
	"$bin" --url "$url" --config "$dir/devnet.json" --settings "$dir/devnet-maker.json" \
		--taker "$key" --maker-address "$maker" --endpoint "ws://127.0.0.1:$port" \
		--halt-file "$dir/HALT" --trades "$dir/trades.jsonl" --log-file "$dir/taker-log.jsonl" $dry
	;;
install)
	settings
	root=$(cd "$(dirname "$0")/.." && pwd)
	bin=${QUOTEBOOK_TAKER_BIN:-$root/target/release/quotebook-demo-taker}
	[ -x "$bin" ] || { echo "$bin missing: cargo build --release -p quotebook-demo" >&2 && exit 1; }
	mkdir -p "$dir" "$HOME/Library/LaunchAgents"
	cp "$bin" "$dir/quotebook-demo-taker"
	cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>$label</string>
	<!-- Devnet only. No key material and no secret: the taker key is a file path. -->
	<key>ProgramArguments</key>
	<array>
		<string>$(xml "$dir/quotebook-demo-taker")</string>
		<string>--url</string><string>$(xml "$url")</string>
		<string>--config</string><string>$(xml "$dir/devnet.json")</string>
		<string>--settings</string><string>$(xml "$dir/devnet-maker.json")</string>
		<string>--taker</string><string>$(xml "$key")</string>
		<string>--maker-address</string><string>$maker</string>
		<string>--endpoint</string><string>ws://127.0.0.1:$port</string>
		<string>--halt-file</string><string>$(xml "$dir/HALT")</string>
		<string>--trades</string><string>$(xml "$dir/trades.jsonl")</string>
		<string>--log-file</string><string>$(xml "$dir/taker-log.jsonl")</string>
	</array>
	<key>LimitLoadToSessionType</key>
	<array>
		<string>Aqua</string>
		<string>Background</string>
	</array>
	<key>RunAtLoad</key><false/>
	<key>StartInterval</key><integer>21600</integer>
	<key>StandardOutPath</key><string>$(xml "$dir/taker-stdout.log")</string>
	<key>StandardErrorPath</key><string>$(xml "$dir/taker-stderr.log")</string>
	<key>WorkingDirectory</key><string>$(xml "$dir")</string>
</dict>
</plist>
EOF
	plutil -lint "$plist" >/dev/null
	unload
	launchctl bootstrap "$domain" "$plist"
	echo "installed $label: $plist (every 6 h; one run by hand: $0 run $name)"
	echo "trades: $dir/trades.jsonl"
	;;
uninstall)
	unload
	rm -f "$plist"
	echo "uninstalled $label (its files, if any, stay in $dir)"
	;;
status)
	domains=$(loaded)
	if [ -z "$domains" ]; then
		echo "$label not loaded"
	fi
	for d in $domains; do
		echo "$d/$label:"
		launchctl print "$d/$label" | grep -E '^	(state|pid|runs|last exit code) =' || true
	done
	if [ -f "$dir/trades.jsonl" ]; then
		echo "last runs ($dir/trades.jsonl):"
		tail -n 3 "$dir/trades.jsonl"
	fi
	;;
*) usage ;;
esac
