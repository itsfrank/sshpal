#!/bin/sh
set -eu

if [ "$#" -lt 1 ]; then
	echo "usage: sshpal-run <task> [name=value ...] [-- <args...>]" >&2
	exit 2
fi

if ! command -v curl >/dev/null 2>&1; then
	echo "sshpal-run requires curl on the remote host" >&2
	exit 1
fi

discover_project_root() {
	current=$(pwd)
	while :; do
		if [ -f "$current/.sshpal.toml" ]; then
			printf '%s\n' "$current"
			return 0
		fi
		if [ "$current" = "/" ]; then
			return 1
		fi
		current=$(dirname "$current")
	done
}

task=$1
shift

if [ "$task" = "tasks-help" ]; then
	if [ "$#" -ne 0 ]; then
		echo "usage: sshpal-run tasks-help" >&2
		exit 2
	fi
	curl \
		--silent \
		--show-error \
		--fail-with-body \
		"http://127.0.0.1:__SSHPAL_RPC_PORT__/tasks-help"
	exit $?
fi

if [ "$task" = "checkhealth" ]; then
	if [ "$#" -ne 0 ]; then
		echo "usage: sshpal-run checkhealth" >&2
		exit 2
	fi
	curl \
		--silent \
		--show-error \
		--fail-with-body \
		"http://127.0.0.1:__SSHPAL_RPC_PORT__/checkhealth"
	exit $?
fi

if ! command -v jq >/dev/null 2>&1; then
	echo "sshpal-run requires jq on the remote host" >&2
	exit 1
fi

project_root=$(discover_project_root) || {
	echo "sshpal-run could not find .sshpal.toml from $(pwd) upward" >&2
	exit 1
}

sync_dir="$project_root/.sshpal"
sync_file="$sync_dir/sync-token"
mkdir -p "$sync_dir"
sync_token="$(date +%s)-$$"
printf '%s\n' "$sync_token" >"$sync_file"

is_valid_var_name() {
	case "$1" in
	'' | [0-9]* | *[!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_]*)
		return 1
		;;
	*)
		return 0
		;;
	esac
}

vars_json='{}'
while [ "$#" -gt 0 ]; do
	case "$1" in
	--)
		shift
		break
		;;
	*=*)
		key=${1%%=*}
		value=${1#*=}
		if ! is_valid_var_name "$key"; then
			echo "invalid task variable: $key" >&2
			exit 2
		fi
		vars_json=$(jq -cn \
			--argjson vars "$vars_json" \
			--arg key "$key" \
			--arg value "$value" \
			'$vars + {($key): $value}')
		;;
	*)
		echo "invalid task argument: $1 (use name=value before --)" >&2
		exit 2
		;;
	esac
	shift
done

payload=$(jq -cn --arg task "$task" --arg sync_token "$sync_token" --argjson vars "$vars_json" '{task: $task, sync_token: $sync_token, vars: $vars, args: $ARGS.positional}' --args -- "$@")

tmpdir=$(mktemp -d)
rpc_fifo="$tmpdir/rpc-stream"
event_fifo="$tmpdir/event-stream"
mkfifo "$rpc_fifo" "$event_fifo"
cleanup() {
	rm -rf "$tmpdir"
}
trap cleanup EXIT HUP INT TERM

curl \
	--silent \
	--show-error \
	--no-buffer \
	-H 'Content-Type: application/json' \
	-d "$payload" \
	"http://127.0.0.1:__SSHPAL_RPC_PORT__/run" >"$rpc_fifo" &
curl_pid=$!

jq -r '
    if .type == "stdout" then
        "stdout\t" + .chunk_b64
    elif .type == "stderr" then
        "stderr\t" + .chunk_b64
    elif .type == "exit" then
        "exit\t" + (.code | tostring)
    else
        error("unknown RPC event type: \(.type)")
    end
' <"$rpc_fifo" >"$event_fifo" &
jq_pid=$!

exit_code=
tab=$(printf '\t')
while IFS="$tab" read -r kind payload; do
	case "$kind" in
	stdout)
		printf '%s' "$payload" | jq -Rrj '@base64d'
		;;
	stderr)
		printf '%s' "$payload" | jq -Rrj '@base64d' >&2
		;;
	exit)
		exit_code=$payload
		;;
	*)
		echo "unknown RPC event kind: $kind" >&2
		exit 1
		;;
	esac
done <"$event_fifo"

if wait "$jq_pid"; then
	parser_status=0
else
	parser_status=$?
fi

if wait "$curl_pid"; then
	curl_status=0
else
	curl_status=$?
fi

if [ "$curl_status" -ne 0 ]; then
	exit "$curl_status"
fi

if [ "$parser_status" -ne 0 ]; then
	exit "$parser_status"
fi

if [ -z "$exit_code" ]; then
	echo "RPC stream ended without exit event" >&2
	exit 1
fi

exit "$exit_code"
