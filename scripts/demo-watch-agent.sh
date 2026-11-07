#!/bin/sh
# Runs quotebook-demo-watch under launchd (macOS), next to the demo maker it watches and independent
# of it, so a dead maker is noticed (docs/PLAN-phase2.md, D4). Devnet only.
#
#   scripts/demo-watch-agent.sh install <name>
#   scripts/demo-watch-agent.sh uninstall <name>
#   scripts/demo-watch-agent.sh status <name>
#
# `install` needs the maker installed first (scripts/demo-agent.sh install <name>): it watches
# the configuration that agent runs, in $QUOTEBOOK_DEMO_HOME/<name>/ (default
# ~/.local/share/quotebook-demo), its halt file and its port. It copies
# target/release/quotebook-demo-watch (QUOTEBOOK_WATCH_BIN overrides it) there, writes the
# LaunchAgent ~/Library/LaunchAgents/com.quotebook.demo-watch.<name>.plist and loads it with
# `--interval 60`. It runs always: restarted after any exit, at most once a minute
# (ThrottleInterval 60), and at every login.
#
# It holds no key: the maker, payer and taker are addresses (QUOTEBOOK_WATCH_MAKER,
# QUOTEBOOK_WATCH_PAYER and QUOTEBOOK_WATCH_TAKER, defaulting to the devnet demo's), and it signs
# nothing. The RPC URL must be the public devnet one (QUOTEBOOK_DEMO_URL). Each alert is one line
# in <dir>/alerts.log, written and rotated by the watcher (10 MB, 5 kept); if QUOTEBOOK_WATCH_ALERT_CMD
# is set at install, each alert also runs it with the alert in QUOTEBOOK_ALERT_KIND,
# QUOTEBOOK_ALERT_SUBJECT and QUOTEBOOK_ALERT_MESSAGE. Nothing goes on any bus. Output:
# <dir>/watch.jsonl (one JSON line a pass and one per alert, rotated at 10 MB, 5 kept),
# <dir>/watch-log.jsonl (the watcher's own log, rotated the same way), and launchd's
# <dir>/watch-stderr.log for anything printed before logging starts.

set -eu

usage() {
	sed -n '5,7p' "$0" | sed 's/^# *//' >&2
	exit 2
}

[ $# -eq 2 ] || usage
cmd=$1
name=$2
case $name in
*[!A-Za-z0-9_-]* | '') echo "name must be letters, digits, - or _" >&2 && exit 2 ;;
esac

home=${QUOTEBOOK_DEMO_HOME:-$HOME/.local/share/quotebook-demo}
dir=$home/$name
label=com.quotebook.demo-watch.$name
plist=$HOME/Library/LaunchAgents/$label.plist
uid=$(id -u)
if launchctl print "gui/$uid" >/dev/null 2>&1; then
	domain=gui/$uid
else
	domain=user/$uid
fi

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

address() {
	case $2 in
	*[!1-9A-HJ-NP-Za-km-z]* | '') echo "$1: not a base58 address" >&2 && exit 2 ;;
	esac
}

case $cmd in
install)
	url=${QUOTEBOOK_DEMO_URL:-https://api.devnet.solana.com}
	# Devnet only, and no URL that could carry a key: it is written into the plist.
	case $url in
	https://api.devnet.solana.com | https://api.devnet.solana.com/) ;;
	*) echo "QUOTEBOOK_DEMO_URL must be the public devnet RPC, https://api.devnet.solana.com" >&2 && exit 2 ;;
	esac
	maker=${QUOTEBOOK_WATCH_MAKER:-142GM9zKDbdncitgiL6zwdKquwuzzNhMPXwEm1RVYaVB}
	payer=${QUOTEBOOK_WATCH_PAYER:-EEGQJgeP96RSHZd97cX3p3HVivAGGKU4LSsQzR7TFVCY}
	taker=${QUOTEBOOK_WATCH_TAKER:-3ZBj1qBpdoH5NrMTwAHq2fY6Hbo5M1GURxaP9y8bsqLs}
	address QUOTEBOOK_WATCH_MAKER "$maker"
	address QUOTEBOOK_WATCH_PAYER "$payer"
	address QUOTEBOOK_WATCH_TAKER "$taker"
	for f in devnet.json devnet-maker.json agent.env; do
		[ -f "$dir/$f" ] || { echo "$dir/$f missing: scripts/demo-agent.sh install $name first" >&2 && exit 1; }
	done
	port=$(sed -n 's/^port=//p' "$dir/agent.env")
	case $port in *[!0-9]* | '') echo "$dir/agent.env: no port" >&2 && exit 1 ;; esac
	case $dir in *\'*) echo "$dir: a quote in the path" >&2 && exit 2 ;; esac
	root=$(cd "$(dirname "$0")/.." && pwd)
	bin=${QUOTEBOOK_WATCH_BIN:-$root/target/release/quotebook-demo-watch}
	[ -x "$bin" ] || { echo "$bin missing: cargo build --release -p quotebook-demo" >&2 && exit 1; }
	alert=${QUOTEBOOK_WATCH_ALERT_CMD:-}
	env_block=''
	if [ -n "$alert" ]; then
		env_block="	<key>EnvironmentVariables</key>
	<dict>
		<key>QUOTEBOOK_WATCH_ALERT_CMD</key><string>$(xml "$alert")</string>
	</dict>"
	fi
	mkdir -p "$dir" "$HOME/Library/LaunchAgents"
	cp "$bin" "$dir/quotebook-demo-watch"
	cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>$label</string>
	<!-- Devnet only. No key and no secret: every account is an address. -->
	<key>ProgramArguments</key>
	<array>
		<string>$(xml "$dir/quotebook-demo-watch")</string>
		<string>--interval</string><string>60</string>
		<string>--url</string><string>$(xml "$url")</string>
		<string>--config</string><string>$(xml "$dir/devnet.json")</string>
		<string>--settings</string><string>$(xml "$dir/devnet-maker.json")</string>
		<string>--maker-address</string><string>$maker</string>
		<string>--payer</string><string>$payer</string>
		<string>--taker</string><string>$taker</string>
		<string>--endpoint</string><string>ws://127.0.0.1:$port</string>
		<string>--halt-file</string><string>$(xml "$dir/HALT")</string>
		<string>--taker-log</string><string>$(xml "$dir/trades.jsonl")</string>
		<string>--out</string><string>$(xml "$dir/watch.jsonl")</string>
		<string>--log-file</string><string>$(xml "$dir/watch-log.jsonl")</string>
		<string>--alerts-log</string><string>$(xml "$dir/alerts.log")</string>
	</array>
$env_block
	<key>LimitLoadToSessionType</key>
	<array>
		<string>Aqua</string>
		<string>Background</string>
	</array>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key><true/>
	<key>ThrottleInterval</key><integer>60</integer>
	<key>StandardOutPath</key><string>$(xml "$dir/watch-stdout.log")</string>
	<key>StandardErrorPath</key><string>$(xml "$dir/watch-stderr.log")</string>
	<key>WorkingDirectory</key><string>$(xml "$dir")</string>
</dict>
</plist>
EOF
	plutil -lint "$plist" >/dev/null
	unload
	launchctl bootstrap "$domain" "$plist"
	echo "installed $label: $plist"
	echo "status lines: $dir/watch.jsonl, alerts: $dir/alerts.log"
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
	if [ -f "$dir/alerts.log" ]; then
		echo "last alerts ($dir/alerts.log):"
		tail -n 5 "$dir/alerts.log"
	fi
	;;
*) usage ;;
esac
