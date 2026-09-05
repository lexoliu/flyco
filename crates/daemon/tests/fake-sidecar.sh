#!/bin/sh
# A stand-in for the Bun sidecar, used by `tests/driver.rs`.
#
# It speaks the flycod line protocol and nothing else: no Bun, no Agent SDK,
# no `claude` process, so the driver's actor, normalizer, approval routing
# and transcript store can be exercised in CI without a live session. The
# driver invokes it exactly as it invokes bun (`<exe> run sidecar.ts` in the
# materialized sidecar directory), and the arguments are ignored.
#
# The one thing a test can vary is the directory it is run in, which is the
# scratch directory the test named: a scratch called `unmounted` gets a
# sidecar that reports a flyco server missing `machine_status`, and one
# called `dies` gets a sidecar that writes to stderr and exits in the middle
# of a turn. The driver gives a sidecar no other channel, and a second copy
# of this script would be a second copy of the whole protocol.
set -eu

say() {
	printf '%s\n' "$1"
}

# `pwd` rather than `$PWD`: the driver changes the child's directory after
# the fork, and an inherited PWD would still name the test runner's.
dies=0
case "$(pwd)" in
*dies*)
	dies=1
	;;
esac

case "$(pwd)" in
*unmounted*)
	mounted='{"type":"mcp_servers","servers":[{"name":"flyco","status":"connected","state":"connected","tools":["budget_status","machine_resize"]}]}'
	# A CLI that will not mount something says why on stderr and nowhere
	# else, which is the half flyco cannot work out for itself.
	echo 'Warning: MCP server blocked by enterprise policy: flyco' >&2
	;;
*)
	mounted='{"type":"mcp_servers","servers":[{"name":"flyco","status":"connected","state":"connected","tools":["machine_status","budget_status","machine_resize"]}]}'
	;;
esac

say '{"type":"ready","sdk_version":"0.0.0-fake"}'

# Every command received, verbatim, so a test can assert on what flycod
# actually sent rather than on what it meant to send.
received='commands.jsonl'
: >"$received"

answered_load=0

while IFS= read -r line; do
	printf '%s\n' "$line" >>"$received"
	case "$line" in
	*'"type":"start"'*)
		# The session is identified as soon as the query is constructed —
		# before any user message. Capabilities are NOT known yet.
		say '{"type":"started","session_id":"fake-session"}'
		# What the CLI mounted, reported once the `initialize` handshake
		# is done and before any turn. A driver that did not get flyco's
		# own tools here fails the session instead of continuing.
		say "$mounted"
		# Resume asks the store for the transcript before anything else.
		say '{"type":"store_request","id":1,"op":{"load":{"key":{"project_key":"fake-project","session_id":"fake-session"}}}}'
		;;
	*'"type":"store_response"'*)
		if [ "$answered_load" -eq 0 ]; then
			answered_load=1
			say '{"type":"store_request","id":2,"op":{"append":{"key":{"project_key":"fake-project","session_id":"fake-session"},"entries":[{"type":"user","uuid":"11111111-1111-4111-8111-111111111111","text":"mirrored"}]}}}'
		fi
		;;
	*'"type":"user_message"'*)
		if [ "$dies" -eq 1 ]; then
			# What a sidecar that cannot go on actually does: it says why
			# on stderr, which nothing parses, and stops. Nothing arrives
			# on stdout, so the driver learns of it only as an EOF.
			echo 'TypeError: the credential was refused' >&2
			echo '    at start (sidecar.ts:42)' >&2
			exit 3
		fi
		# `system/init` rides the first turn, so this is the earliest the
		# CLI can name what it supports.
		say '{"type":"capabilities","capabilities":["interrupt_receipt_v1"]}'
		say '{"type":"sdk_message","message":{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}}}'
		say '{"type":"approval_request","id":"3f2b7c18-9a4d-4e51-b0c6-7d8e1f2a3b4c","tool":"Bash","input":{"command":"echo hi"},"suggestions":null}'
		;;
	*'"type":"approval_decision"'*)
		say '{"type":"sdk_message","message":{"type":"result","subtype":"success","is_error":false,"result":"done","usage":{"input_tokens":7,"output_tokens":11},"total_cost_usd":0.0025}}'
		;;
	*'"type":"interrupt"'*)
		say '{"type":"sdk_message","message":{"type":"result","subtype":"error_during_execution","is_error":true,"result":"interrupted","usage":{"input_tokens":7,"output_tokens":0}}}'
		;;
	*'"type":"compact"'*)
		say '{"type":"sdk_message","message":{"type":"system","subtype":"status","status":null,"compact_result":"success","uuid":"status-1","session_id":"fake-session"}}'
		;;
	*'"type":"shutdown"'*)
		exit 0
		;;
	*)
		say '{"type":"fatal","error":"the fake sidecar does not know this command"}'
		exit 1
		;;
	esac
done
