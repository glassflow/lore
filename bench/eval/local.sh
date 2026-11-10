#!/usr/bin/env bash
# Reproduces the public-set search numbers against a running lore server.
#
#   bench/eval/local.sh [LABEL]
#
# Creates a tenant and a key, pushes this repo as the source `lore` at the commit that
# bench/eval/questions/lore.json was written against, then runs the search eval on that set. The
# indexed code stays fixed at that commit, so only the server's ranking changes between runs.
# LABEL defaults to <embedder>-<short HEAD of this checkout>, for example fastembed-105500e; the
# result goes to bench/eval/results/LABEL.json.
#
# Environment:
#   LORE_URL           the server (default http://localhost:8080)
#   LORE_ADMIN_TOKEN   its admin token (default dev-admin, as `make run` sets)
#   EVAL_KEY           optional: reuse this tenant key (scopes ingest,read) instead of creating
#                      a tenant; pushing again at the same commit sends nothing. LORE_KEY is not
#                      read, so a key for another server in your shell is never used here
#   LORE_BIN           the lore CLI (default: lore on PATH, else target/release/lore, else
#                      target/debug/lore)
#
# Needs git, curl and python3. The checkout must hold the set's commit (a shallow clone may not:
# git fetch --unshallow).
set -euo pipefail

fail() {
  echo "EVAL FAIL: $*" >&2
  exit 1
}

cd "$(dirname "$0")/../.."
ROOT=$(pwd)
SET=bench/eval/questions/lore.json
export LORE_URL=${LORE_URL:-http://localhost:8080}
export LORE_ADMIN_TOKEN=${LORE_ADMIN_TOKEN:-dev-admin}

for c in git curl python3; do
  command -v "$c" >/dev/null || fail "$c is not on PATH"
done
if [ -z "${LORE_BIN:-}" ]; then
  if command -v lore >/dev/null; then
    LORE_BIN=lore
  elif [ -x target/release/lore ]; then
    LORE_BIN=$ROOT/target/release/lore
  elif [ -x target/debug/lore ]; then
    LORE_BIN=$ROOT/target/debug/lore
  else
    fail "no lore CLI: put it on PATH, set LORE_BIN, or cargo build --release"
  fi
fi

# json <python expression over j>: reads JSON on stdin and prints the expression.
json() {
  python3 -c "import json, sys; j = json.load(sys.stdin); print($1)"
}

INFO=$(curl -fsS "$LORE_URL/v1/info") || fail "no lore server answers at $LORE_URL"
EMBEDDER=$(printf '%s' "$INFO" | json 'j["embedder"].split(":")[0]')
COMMIT=$(json 'j["commit"]' <"$SET")
LABEL=${1:-$EMBEDDER-$(git rev-parse --short HEAD)}
[ "$EMBEDDER" = fastembed ] ||
  echo "note: the server uses the $EMBEDDER embedder; the published numbers use fastembed" >&2

KEY=${EVAL_KEY:-}
if [ -z "$KEY" ]; then
  name="eval-$(date +%s)-$RANDOM"
  tenant=$("$LORE_BIN" admin tenant create --name "$name" | json 'j["id"]') ||
    fail "could not create a tenant (is LORE_ADMIN_TOKEN right?)"
  KEY=$("$LORE_BIN" admin key create --tenant "$tenant" --scopes ingest,read | json 'j["key"]') ||
    fail "could not create a key"
  echo "created tenant $name; EVAL_KEY=$KEY reuses it" >&2
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
git clone -q --no-checkout "$ROOT" "$WORK/lore" || fail "could not clone $ROOT"
git -C "$WORK/lore" checkout -q --detach "$COMMIT" 2>/dev/null ||
  fail "commit $COMMIT is not in this checkout (a shallow clone lacks it: git fetch --unshallow)"

echo "pushing lore at ${COMMIT:0:12} (the first push embeds every file and takes a minute or more)" >&2
LORE_KEY=$KEY "$LORE_BIN" repo push "$WORK/lore" --name lore >&2 || fail "lore repo push failed"

python3 bench/eval/retrieval.py "$LABEL" "$LORE_URL" "$KEY" "$SET"
