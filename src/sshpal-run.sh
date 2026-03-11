#!/bin/sh
set -eu

if [ "$#" -lt 1 ]; then
    echo "usage: sshpal-run <task> [args...]" >&2
    exit 2
fi

if ! command -v curl >/dev/null 2>&1; then
    echo "sshpal-run requires curl on the remote host" >&2
    exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "sshpal-run requires jq on the remote host" >&2
    exit 1
fi

task=$1
shift

payload=$(jq -cn --arg task "$task" --args "$@" '{task: $task, args: $ARGS.positional}')

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
    --fail-with-body \
    -H 'Content-Type: application/json' \
    -d "$payload" \
    "http://127.0.0.1:__SSHPAL_RPC_PORT__/run" >"$rpc_fifo" &
curl_pid=$!

jq -r '
    if .type == "stdout" then
        "stdout\t" + (.chunk | @base64)
    elif .type == "stderr" then
        "stderr\t" + (.chunk | @base64)
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
            printf '%s' "$payload" | jq -Rr '@base64d'
            ;;
        stderr)
            printf '%s' "$payload" | jq -Rr '@base64d' >&2
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
