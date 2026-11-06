#!/usr/bin/env bash
# Smoke test for the MCP endpoint of a running lore server.
#
#   scripts/mcp_smoke.sh
#
# Needs LORE_URL and LORE_ADMIN_TOKEN in the environment, and lore, jq, git and curl on PATH.
# It creates a fresh tenant and key on each run, pushes tests/fixtures/sample-repo, then talks to
# POST $LORE_URL/mcp the way an MCP client does: initialize, tools/list, search and (when the
# server offers it) a deep ask.
set -euo pipefail
shopt -s inherit_errexit 2>/dev/null || true

cd "$(dirname "$0")/.."

PLANS=src/billing/plans.rs

fail() {
  echo "MCP SMOKE FAIL: $*" >&2
  exit 1
}

# assert_jq <json> <jq-filter> <msg>: fails unless `jq -e <filter>` is true for the json.
assert_jq() {
  local json=$1 filter=$2 msg=$3
  if ! printf '%s' "$json" | jq -e "$filter" >/dev/null 2>&1; then
    fail "$msg (jq filter: $filter; got: ${json:0:400})"
  fi
}

# git_smoke <dir> <git args...>: runs git with a throwaway identity, never touching global config.
git_smoke() {
  local dir=$1
  shift
  git -C "$dir" -c user.name=smoke -c user.email=smoke@example.invalid \
    -c commit.gpgsign=false "$@" >/dev/null \
    || fail "git $* failed in $dir"
}

# mcp <json-rpc body>: POSTs it to /mcp and prints the JSON-RPC reply (nothing for a
# notification). The server may answer with plain JSON or with an SSE stream, in which case the
# JSON is on the `data:` lines. A session id returned by the server (the Mcp-Session-Id header)
# is kept in $WORK/session and sent on every later call.
mcp() {
  local body=$1 hdr out code sid
  local -a args
  hdr=$(mktemp "$WORK/hdr.XXXXXX") || fail "could not create a temp file"
  out=$(mktemp "$WORK/out.XXXXXX") || fail "could not create a temp file"
  args=(-sS -X POST "$LORE_URL/mcp" -D "$hdr" -o "$out" -w '%{http_code}'
    -H "Authorization: Bearer $KEY"
    -H 'Content-Type: application/json'
    -H 'Accept: application/json, text/event-stream'
    -H 'MCP-Protocol-Version: 2025-06-18')
  if [ -s "$WORK/session" ]; then
    args+=(-H "Mcp-Session-Id: $(cat "$WORK/session")")
  fi
  code=$(curl "${args[@]}" -d "$body") || fail "curl could not reach $LORE_URL/mcp"
  case "$code" in
    2??) ;;
    *) fail "POST /mcp answered $code: $(head -c 400 "$out")" ;;
  esac
  sid=$(awk 'tolower($1) == "mcp-session-id:" { print $2 }' "$hdr" | tr -d '\r')
  if [ -n "$sid" ]; then
    printf '%s' "$sid" >"$WORK/session"
  fi
  if grep -qi '^content-type: *text/event-stream' "$hdr"; then
    sed -n 's/^data: \{0,1\}//p' "$out" | tr -d '\r'
  else
    cat "$out"
  fi
}

# tool_call <id> <tool> <arguments json>: calls a tool and prints the JSON-RPC reply.
tool_call() {
  local id=$1 tool=$2 args=$3 body
  body=$(jq -nc --argjson id "$id" --arg tool "$tool" --argjson args "$args" \
    '{jsonrpc: "2.0", id: $id, method: "tools/call", params: {name: $tool, arguments: $args}}') \
    || fail "could not build the $tool request"
  mcp "$body"
}

main() {
  local missing="" cmd D R tenant name init tools names search line info ask

  [ -n "${LORE_URL:-}" ] || missing="$missing LORE_URL"
  [ -n "${LORE_ADMIN_TOKEN:-}" ] || missing="$missing LORE_ADMIN_TOKEN"
  for cmd in lore jq git curl; do
    command -v "$cmd" >/dev/null 2>&1 || missing="$missing $cmd(on PATH)"
  done
  [ -z "$missing" ] || fail "missing:$missing"
  export LORE_URL LORE_ADMIN_TOKEN

  WORK=$(mktemp -d) || fail "could not create a temp dir"
  trap 'rm -rf "$WORK"' EXIT

  # 1. A fresh tenant and key, and the fixture pushed, as in smoke.sh stage_m1.
  name="mcp-smoke-$(date +%s)-$RANDOM"
  tenant=$(lore admin tenant create --name "$name" | jq -er '.id') \
    || fail "could not create tenant $name"
  KEY=$(lore admin key create --tenant "$tenant" --scopes ingest,read | jq -er '.key') \
    || fail "could not create a key for tenant $tenant"
  D=$(mktemp -d "$WORK/fixture.XXXXXX") || fail "could not create a temp dir"
  cp -R tests/fixtures/sample-repo/. "$D"/ || fail "could not copy the fixture"
  git_smoke "$D" init -q -b main
  git_smoke "$D" add -A
  git_smoke "$D" commit -q -m "sample repo"
  R=$(LORE_KEY=$KEY lore repo push "$D" --name sample --json) || fail "push failed"
  assert_jq "$R" '.files_indexed >= 5' "the push should index at least 5 files"

  # 2. initialize, then the initialized notification.
  init=$(mcp '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"mcp_smoke","version":"0"}}}') \
    || exit 1
  assert_jq "$init" '.result.serverInfo.name == "lore"' "initialize should report the server name lore"
  mcp '{"jsonrpc":"2.0","method":"notifications/initialized"}' >/dev/null || exit 1

  # 3. tools/list.
  tools=$(mcp '{"jsonrpc":"2.0","id":2,"method":"tools/list"}') || exit 1
  names=$(printf '%s' "$tools" | jq -er '[.result.tools[].name] | sort | join(" ")') \
    || fail "tools/list gave no tools: ${tools:0:400}"
  echo "$names"
  [ "$names" = "ask callers definition get_document list_sources search" ] \
    || fail "expected the tools 'ask callers definition get_document list_sources search', got '$names'"

  # 4. search.
  search=$(tool_call 3 search '{"query":"plan limits"}') || exit 1
  assert_jq "$search" '.result.isError != true' "search should not be a tool error"
  # The result is text: a header per hit, `[n] <source> <path>:<start>-<end> (<kind>...) id=<uuid>`.
  line=$(printf '%s' "$search" | jq -er '.result.content[0].text' \
    | grep -m 1 -E "^\[[0-9]+\] [^ ]+ $PLANS:[0-9]+-[0-9]+ ")
  [ -n "$line" ] || fail "search for 'plan limits' should return $PLANS: ${search:0:400}"
  echo "$line"

  # 5. A deep ask, when the server offers deep mode.
  info=$(curl -sS "$LORE_URL/v1/info") || fail "could not read /v1/info"
  if printf '%s' "$info" | jq -e '.modes | index("deep")' >/dev/null 2>&1; then
    ask=$(tool_call 4 ask '{"question":"list every plan and its seat limit","mode":"deep"}') || exit 1
    assert_jq "$ask" '.result.isError != true' "the deep ask should not be a tool error"
    assert_jq "$ask" '.result.content[0].text | contains("src/billing/plans.rs")' \
      "the deep ask should cite $PLANS"
    echo "deep ask cites $PLANS"
  else
    echo "deep mode is not offered by this server, skipping the deep ask" >&2
  fi

  echo "MCP SMOKE OK"
}

main "$@"
