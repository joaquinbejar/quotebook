#!/bin/sh
# Runs quotebook-demo-maker under launchd (macOS), on devnet only (docs/PLAN-phase2.md, D3).
#
#   scripts/demo-agent.sh install <name>
#   scripts/demo-agent.sh uninstall <name>
#   scripts/demo-agent.sh status <name>
#   scripts/demo-agent.sh halt <name>
#   scripts/demo-agent.sh revoke <name>
#
# `install` copies target/release/quotebook-demo-maker (run `cargo build --release -p
# quotebook-demo` first; QUOTEBOOK_DEMO_BIN overrides the path) and the two configuration files
# (demo/config/devnet.json and devnet-maker.json) to $QUOTEBOOK_DEMO_HOME/<name>/ (default
# ~/.local/share/quotebook-demo), so a later `git pull` does not change what runs. It writes the
# LaunchAgent ~/Library/LaunchAgents/com.quotebook.demo-maker.<name>.plist and loads it. The agent
# starts now and at every login, and is restarted if it exits with an error or is killed, at most
# once every 120 s (ThrottleInterval); each restart pays the cold start (70 s at a 60 s TTL).
#
# Nothing secret reaches the plist or the environment: the spot source needs no key, the RPC URL
# must be a public devnet URL (QUOTEBOOK_DEMO_URL, default https://api.devnet.solana.com), and the
# maker key is a file path ($QUOTEBOOK_DEMO_KEYS/maker.json, default
# ~/.config/solana/quotebook-demo). The service listens on 127.0.0.1:$QUOTEBOOK_DEMO_PORT (default
# 9000) only. Logs: <dir>/maker.jsonl (JSON lines, rotated at 10 MB, 5 kept), and launchd's
# <dir>/stderr.log for anything printed before logging starts.
#
# `halt` creates <dir>/HALT, which stops new quotes, then waits until a finalized Clock passes the
# newest expiry the service signed (the drain). Remove <dir>/HALT to quote again.
# `revoke` is the emergency path (D1): halt, `cancel_all` up to the service's next nonce, a token
# Revoke on every maker account, both confirmed at finalized, then a restart of the agent, which
# stays halted. It signs with the maker key and pays with $QUOTEBOOK_DEMO_KEYS/payer.json. If the
# service is down, set QUOTEBOOK_DEMO_NEW_MIN to a nonce above every one it signed.

set -eu

usage() {
	sed -n '4,8p' "$0" | sed 's/^# *//' >&2
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
dir=$home/$name
label=com.quotebook.demo-maker.$name
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

# The URL and port recorded at install, for the commands that talk to a running agent.
settings() {
	[ -f "$dir/agent.env" ] || { echo "$dir/agent.env missing: install first" >&2 && exit 1; }
	url=$(sed -n 's/^url=//p' "$dir/agent.env")
	port=$(sed -n 's/^port=//p' "$dir/agent.env")
	endpoint=ws://127.0.0.1:$port
}

case $cmd in
install)
	url=${QUOTEBOOK_DEMO_URL:-https://api.devnet.solana.com}
	port=${QUOTEBOOK_DEMO_PORT:-9000}
	# Devnet only, and no URL that could carry a key: it is written into the plist.
	case $url in
	https://api.devnet.solana.com | https://api.devnet.solana.com/) ;;
	*) echo "QUOTEBOOK_DEMO_URL must be the public devnet RPC, https://api.devnet.solana.com" >&2 && exit 2 ;;
	esac
	case $port in *[!0-9]* | '') echo "QUOTEBOOK_DEMO_PORT: digits only" >&2 && exit 2 ;; esac
	root=$(cd "$(dirname "$0")/.." && pwd)
	bin=${QUOTEBOOK_DEMO_BIN:-$root/target/release/quotebook-demo-maker}
	[ -x "$bin" ] || { echo "$bin missing: cargo build --release -p quotebook-demo" >&2 && exit 1; }
	[ -f "$keys/maker.json" ] || { echo "$keys/maker.json missing" >&2 && exit 1; }
	mkdir -p "$dir" "$HOME/Library/LaunchAgents"
	cp "$bin" "$dir/quotebook-demo-maker"
	cp "$root/demo/config/devnet.json" "$dir/devnet.json"
	cp "$root/demo/config/devnet-maker.json" "$dir/devnet-maker.json"
	printf 'url=%s\nport=%s\n' "$url" "$port" >"$dir/agent.env"
	cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>$label</string>
	<!-- Devnet only. No key material and no secret: the maker key is a file path. -->
	<key>ProgramArguments</key>
	<array>
		<string>$(xml "$dir/quotebook-demo-maker")</string>
		<string>run</string>
		<string>--url</string><string>$(xml "$url")</string>
		<string>--config</string><string>$(xml "$dir/devnet.json")</string>
		<string>--settings</string><string>$(xml "$dir/devnet-maker.json")</string>
		<string>--maker</string><string>$(xml "$keys/maker.json")</string>
		<string>--listen</string><string>127.0.0.1:$port</string>
		<string>--halt-file</string><string>$(xml "$dir/HALT")</string>
		<string>--log-file</string><string>$(xml "$dir/maker.jsonl")</string>
	</array>
	<key>LimitLoadToSessionType</key>
	<array>
		<string>Aqua</string>
		<string>Background</string>
	</array>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key><false/>
	</dict>
	<key>ThrottleInterval</key><integer>120</integer>
	<key>StandardOutPath</key><string>$(xml "$dir/stdout.log")</string>
	<key>StandardErrorPath</key><string>$(xml "$dir/stderr.log")</string>
	<key>WorkingDirectory</key><string>$(xml "$dir")</string>
</dict>
</plist>
EOF
	plutil -lint "$plist" >/dev/null
	unload
	launchctl bootstrap "$domain" "$plist"
	echo "installed $label: $plist"
	echo "log: $dir/maker.jsonl, halt file: $dir/HALT, endpoint: ws://127.0.0.1:$port"
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
	if [ -f "$dir/HALT" ]; then echo "halt file present: $dir/HALT"; fi
	if [ -n "$domains" ]; then
		settings
		"$dir/quotebook-demo-maker" status --endpoint "$endpoint" || echo "no status answer from $endpoint"
	fi
	;;
halt)
	settings
	touch "$dir/HALT"
	echo "halt file created: $dir/HALT; draining"
	"$dir/quotebook-demo-maker" drain --endpoint "$endpoint" --url "$url"
	;;
revoke)
	settings
	touch "$dir/HALT"
	[ -f "$keys/payer.json" ] || { echo "$keys/payer.json missing" >&2 && exit 1; }
	if [ -n "${QUOTEBOOK_DEMO_NEW_MIN:-}" ]; then
		case $QUOTEBOOK_DEMO_NEW_MIN in *[!0-9]*) echo "QUOTEBOOK_DEMO_NEW_MIN: digits only" >&2 && exit 2 ;; esac
		from="--new-min $QUOTEBOOK_DEMO_NEW_MIN"
	else
		from="--endpoint $endpoint"
	fi
	# shellcheck disable=SC2086 # $from is two words, both checked above.
	"$dir/quotebook-demo-maker" revoke --url "$url" --config "$dir/devnet.json" \
		--maker "$keys/maker.json" --payer "$keys/payer.json" $from
	# A service that has cancelled restarts, paying the cold start: a live issuer cannot jump its
	# counter. It comes back halted, since the halt file stays.
	for d in $(loaded); do
		launchctl kickstart -k "$d/$label"
	done
	echo "revoked; $label restarted halted. Approve again and remove $dir/HALT to resume."
	;;
*) usage ;;
esac
