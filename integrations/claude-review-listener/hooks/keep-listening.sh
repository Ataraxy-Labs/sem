#!/usr/bin/env bash
# Stop-hook backstop for the sem review listener.
#
# The MCP tools (join_review / wait_for_branch / reply_to_branch) are the
# PRIMARY loop: a well-behaved session never actually stops, it just calls
# wait_for_branch again. This hook exists only as a backstop in case the
# model tries to end its turn anyway (context compaction, a confused model,
# etc.) while a review question is still open.
#
# IMPORTANT (read before touching the polling call below): this hook must
# stay READ-ONLY with respect to sem-cloud's branch-claim state. Hitting
# GET /v1/diffs/{id}/agent/next from here would claim the branch (its state
# flips from "open" to "answering") on the hook's behalf, racing / double
# -claiming against the MCP wait_for_branch tool the model is supposed to be
# calling. So instead we GET /v1/diffs/{id}/comments — a plain read of
# comment state — and only check whether anything looks unanswered. We never
# claim anything here; we just nudge the model back into calling
# wait_for_branch itself.
#
# Contract note: the current Claude Code Stop-hook JSON input
# (https://code.claude.com/docs/en/hooks) does NOT include a
# `stop_hook_active` field, and there is no documented cap on how many times
# a Stop hook may block in a row. Both were assumed in this hook's original
# spec. We compensate with our own bounded counter (persisted under
# CLAUDE_PLUGIN_DATA, keyed by session_id): block at most 8 consecutive times
# for "there's definitely an open question", and block only once (not
# forever) for "not sure, nudge as a courtesy" before giving up and allowing
# the stop. See the plugin README's "Contract doubts" section.

set -u

BLOCK_CAP=8

input="$(cat)"

allow() {
  # No JSON / exit 0 = "allow Claude to stop normally".
  exit 0
}

block() {
  # $1 = reason text
  printf '{"decision":"block","reason":%s}\n' "$(printf '%s' "$1" | jq -Rs .)"
  exit 0
}

command -v jq >/dev/null 2>&1 || allow
command -v curl >/dev/null 2>&1 || allow

session_id="$(printf '%s' "$input" | jq -r '.session_id // "unknown"' 2>/dev/null)"
[ -n "$session_id" ] || session_id="unknown"

: "${SEM_CLOUD_URL:=}"
: "${SEM_API_KEY:=}"
: "${SEM_REVIEW_DIFF_ID:=}"

# Nothing to babysit without config — this backstop only makes sense while
# actively listening on a specific diff.
if [ -z "$SEM_CLOUD_URL" ] || [ -z "$SEM_API_KEY" ] || [ -z "$SEM_REVIEW_DIFF_ID" ]; then
  allow
fi

state_dir="${CLAUDE_PLUGIN_DATA:-${TMPDIR:-/tmp}/sem-review-listener}"
mkdir -p "$state_dir" 2>/dev/null
count_file="$state_dir/stop-count-$session_id"
count="$(cat "$count_file" 2>/dev/null || echo 0)"
case "$count" in ''|*[!0-9]*) count=0 ;; esac

reset_count() {
  rm -f "$count_file" 2>/dev/null
}

# Our own block cap (see contract note above): don't fight the model forever.
if [ "$count" -ge "$BLOCK_CAP" ]; then
  reset_count
  allow
fi

response="$(curl -sS -m 10 \
  -H "Authorization: Bearer $SEM_API_KEY" \
  "${SEM_CLOUD_URL%/}/v1/diffs/$SEM_REVIEW_DIFF_ID/comments" 2>/dev/null)"
curl_status=$?

# Unreachable / not-yet-implemented endpoint (e.g. 404 while sem-cloud is
# still being built): we can't tell what's going on, so don't block blindly.
if [ "$curl_status" -ne 0 ] || ! printf '%s' "$response" | jq -e . >/dev/null 2>&1; then
  reset_count
  allow
fi

open_count="$(printf '%s' "$response" | jq '[(.comments // .) | (if type=="array" then . else [] end)[] | select(.state=="open")] | length' 2>/dev/null)"
case "$open_count" in ''|*[!0-9]*) open_count=0 ;; esac

if [ "$open_count" -gt 0 ]; then
  new_count=$((count + 1))
  printf '%s' "$new_count" > "$count_file"
  block "A review question is waiting. Call wait_for_branch for diff $SEM_REVIEW_DIFF_ID now and continue the listener loop."
fi

# No open comment visible. Nudge once (in case the model just hasn't
# joined/looped yet), but don't fight a genuine stop forever: if we already
# nudged last time and the model tried to stop again, let it go.
if [ "$count" -eq 0 ]; then
  printf '1' > "$count_file"
  block "You are a review listener; call wait_for_branch again."
fi

reset_count
allow
