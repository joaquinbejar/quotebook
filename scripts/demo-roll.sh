#!/bin/sh
# The demo maker's writer cycle under launchd (macOS), on devnet only (docs/PLAN-phase2.md, D5).
#
#   scripts/demo-roll.sh run <name>
#   scripts/demo-roll.sh install <name>
#   scripts/demo-roll.sh uninstall <name>
#   scripts/demo-roll.sh status <name>
#
# THE ADMIN KEY IN A SCHEDULED JOB IS DEVNET ONLY. This job holds the path of the quotebook admin
# keypair (GZhm...tVnL, $QUOTEBOOK_ROLL_ADMIN, default ~/.config/solana/quotebook-devnet/admin.json),
# which signs only `create_series`, logged to <dir>/roll.log. This is not allowed on mainnet: a
# scheduled job never holds a mainnet admin key. The binary also refuses any cluster whose
# genesis is not devnet's.
#
# `run` is one cycle (quotebook-demo-roll apply), for the maker installed by
# scripts/demo-agent.sh install <name>, in $QUOTEBOOK_DEMO_HOME/<name>/ (default
# ~/.local/share/quotebook-demo). It is idempotent and acts only on what is missing: a pair for
# each tradable expiry of the 1 oz XAG capped calls (the strike nearest spot), its quotebook
# series, the mints up to the band, the funding and the writer cap, the approvals, and after
# settlement the claims, redeems and revokes. The mints, approvals, claims, redeems and revokes
# run only between a halt and drain and a finalized confirmation: when any is due, the run
# creates <dir>/HALT, waits for the drain, sends, and confirms each at finalized. When the pairs
# change it rewrites <dir>/devnet.json (the previous one kept as devnet.json.prev) and restarts
# the maker and the watcher, which load the pairs at start. Then it removes the halt file if it
# created it. A failed cycle leaves the halt file: the maker stays halted, the watcher reports
# `halted`, and someone looks before removing it. A halt file already present (an incident, or a
# revoke whose approvals must not come back on their own) stops the cycle before it mints,
# approves or claims anything.
#
# One cycle at a time: `run` holds <dir>/roll.lock (a directory with the holder's PID; a lock
# whose PID is gone is taken over). With QUOTEBOOK_ROLL_DRY_RUN=1 it runs `plan` instead: every
# read, the plan printed, no key loaded and nothing sent (QUOTEBOOK_ROLL_MAKER, default the
# devnet demo maker, is the address it plans for).
#
# `install` copies target/release/quotebook-demo-roll (QUOTEBOOK_ROLL_BIN overrides it) and this
# script to <dir>, writes the LaunchAgent ~/Library/LaunchAgents/com.quotebook.demo-roll.<name>.plist
# and loads it: `run` every 6 hours (00:10, 06:10, 12:10 and 18:10 local time, three hours from the taker so a fill cannot land in a restart), not at load. Every run is idempotent,
# so a missed or repeated one is harmless; in a normal week only the first run after a listing
# or a settlement has anything to do, a pause of about 70 s. The plist holds keypair file paths
# only, no key material and no secret: the maker and payer are $QUOTEBOOK_DEMO_KEYS/maker.json
# and payer.json (default ~/.config/solana/quotebook-demo), the admin as above. The RPC URL is
# the public devnet one recorded by the maker's install.
#
# Output: <dir>/roll.log (one line a run, with the cycle's JSON summary) and
# <dir>/roll-log.jsonl (the binary's JSON log, rotated at 10 MB, 5 kept).

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
keys=${QUOTEBOOK_DEMO_KEYS:-$HOME/.config/solana/quotebook-demo}
admin=${QUOTEBOOK_ROLL_ADMIN:-$HOME/.config/solana/quotebook-devnet/admin.json}
dir=$home/$name
label=com.quotebook.demo-roll.$name
maker_label=com.quotebook.demo-maker.$name
watch_label=com.quotebook.demo-watch.$name
plist=$HOME/Library/LaunchAgents/$label.plist
uid=$(id -u)
if launchctl print "gui/$uid" >/dev/null 2>&1; then
	domain=gui/$uid
else
	domain=user/$uid
fi

log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"; }

loaded() {
	for d in "gui/$uid" "user/$uid"; do
		if launchctl print "$d/$1" >/dev/null 2>&1; then
			echo "$d"
		fi
	done
}

unload() {
	for d in $(loaded "$label"); do
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
}

# Takes <dir>/roll.lock, or exits 0 if a live cycle holds it.
lock() {
	if ! mkdir "$dir/roll.lock" 2>/dev/null; then
		holder=$(cat "$dir/roll.lock/pid" 2>/dev/null || true)
		if [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null; then
			log "another cycle runs (PID $holder): nothing done"
			exit 0
		fi
		log "stale lock (PID ${holder:-unknown} gone): taken over"
		rm -rf "$dir/roll.lock"
		mkdir "$dir/roll.lock"
	fi
	echo $$ >"$dir/roll.lock/pid"
	trap 'rm -rf "$dir/roll.lock"' EXIT INT TERM
}

restart() {
	for l in "$maker_label" "$watch_label"; do
		for d in $(loaded "$l"); do
			launchctl kickstart -k "$d/$l"
			log "restarted $d/$l"
		done
	done
}

case $cmd in
run)
	settings
	bin=$dir/quotebook-demo-roll
	[ -x "$bin" ] || { echo "$bin missing: install first" >&2 && exit 1; }
	lock
	if [ "${QUOTEBOOK_ROLL_DRY_RUN:-0}" = 1 ]; then
		maker_address=${QUOTEBOOK_ROLL_MAKER:-142GM9zKDbdncitgiL6zwdKquwuzzNhMPXwEm1RVYaVB}
		case $maker_address in *[!1-9A-HJ-NP-Za-km-z]* | '') echo "QUOTEBOOK_ROLL_MAKER: not a base58 address" >&2 && exit 2 ;; esac
		"$bin" plan --url "$url" --config "$dir/devnet.json" --settings "$dir/devnet-maker.json" \
			--maker-address "$maker_address"
		exit 0
	fi
	[ -f "$keys/maker.json" ] && [ -f "$keys/payer.json" ] || { echo "$keys/maker.json or payer.json missing" >&2 && exit 1; }
	[ -f "$admin" ] || { echo "$admin missing" >&2 && exit 1; }
	status=0
	summary=$("$bin" apply --url "$url" --config "$dir/devnet.json" \
		--settings "$dir/devnet-maker.json" --maker "$keys/maker.json" --payer "$keys/payer.json" \
		--admin "$admin" --endpoint "ws://127.0.0.1:$port" --halt-file "$dir/HALT" \
		--state-file "$dir/roll.state" --log-file "$dir/roll-log.jsonl") || status=$?
	log "cycle exit $status: $summary"
	if [ "$status" -ne 0 ]; then
		if [ -f "$dir/HALT" ]; then log "the halt file stays: $dir/HALT"; fi
		exit "$status"
	fi
	case $summary in *'"restart":true'*) restart ;; esac
	case $summary in
	*'"halt_created":true'*)
		rm -f "$dir/HALT"
		log "halt file removed: the maker quotes again after its cold start"
		;;
	esac
	;;
install)
	settings
	root=$(cd "$(dirname "$0")/.." && pwd)
	bin=${QUOTEBOOK_ROLL_BIN:-$root/target/release/quotebook-demo-roll}
	[ -x "$bin" ] || { echo "$bin missing: cargo build --release -p quotebook-demo" >&2 && exit 1; }
	[ -f "$keys/maker.json" ] && [ -f "$keys/payer.json" ] || { echo "$keys/maker.json or payer.json missing" >&2 && exit 1; }
	[ -f "$admin" ] || { echo "$admin missing" >&2 && exit 1; }
	mkdir -p "$dir" "$HOME/Library/LaunchAgents"
	cp "$bin" "$dir/quotebook-demo-roll"
	cp "$0" "$dir/demo-roll.sh"
	cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>$label</string>
	<!-- Devnet only. The admin key in a scheduled job is devnet only: not allowed on mainnet. -->
	<!-- No key material and no secret: the keys are file paths. -->
	<key>ProgramArguments</key>
	<array>
		<string>/bin/sh</string>
		<string>$(xml "$dir/demo-roll.sh")</string>
		<string>run</string>
		<string>$name</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>QUOTEBOOK_DEMO_HOME</key><string>$(xml "$home")</string>
		<key>QUOTEBOOK_DEMO_KEYS</key><string>$(xml "$keys")</string>
		<key>QUOTEBOOK_ROLL_ADMIN</key><string>$(xml "$admin")</string>
	</dict>
	<key>LimitLoadToSessionType</key>
	<array>
		<string>Aqua</string>
		<string>Background</string>
	</array>
	<key>RunAtLoad</key><false/>
	<key>StartCalendarInterval</key>
	<array>
		<dict><key>Hour</key><integer>0</integer><key>Minute</key><integer>10</integer></dict>
		<dict><key>Hour</key><integer>6</integer><key>Minute</key><integer>10</integer></dict>
		<dict><key>Hour</key><integer>12</integer><key>Minute</key><integer>10</integer></dict>
		<dict><key>Hour</key><integer>18</integer><key>Minute</key><integer>10</integer></dict>
	</array>
	<key>StandardOutPath</key><string>$(xml "$dir/roll.log")</string>
	<key>StandardErrorPath</key><string>$(xml "$dir/roll.log")</string>
	<key>WorkingDirectory</key><string>$(xml "$dir")</string>
</dict>
</plist>
EOF
	plutil -lint "$plist" >/dev/null
	unload
	launchctl bootstrap "$domain" "$plist"
	echo "installed $label: $plist (every 6 h; the first run by hand: $0 run $name)"
	echo "log: $dir/roll.log"
	;;
uninstall)
	unload
	rm -f "$plist"
	echo "uninstalled $label (its files, if any, stay in $dir)"
	;;
status)
	domains=$(loaded "$label")
	if [ -z "$domains" ]; then
		echo "$label not loaded"
	fi
	for d in $domains; do
		echo "$d/$label:"
		launchctl print "$d/$label" | grep -E '^	(state|pid|runs|last exit code) =' || true
	done
	if [ -d "$dir/roll.lock" ]; then echo "lock held: PID $(cat "$dir/roll.lock/pid" 2>/dev/null || echo unknown)"; fi
	if [ -f "$dir/HALT" ]; then echo "halt file present: $dir/HALT"; fi
	if [ -f "$dir/roll.log" ]; then
		echo "last runs ($dir/roll.log):"
		tail -n 3 "$dir/roll.log"
	fi
	;;
*) usage ;;
esac
