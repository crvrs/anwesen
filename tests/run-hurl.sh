#!/usr/bin/env bash
# End-to-end HTTP contract harness per [[ADR-008 HTTP Testing with hurl]].
#
# Boots `anwesen serve` against `tests/fixtures/vault`, waits for /health to
# return 200, then runs `hurl --test` against every *.hurl under tests/hurl/.
# Propagates hurl's exit code; tears the daemon down on exit.
#
# Override PORT via ANWESEN_TEST_PORT if 18086 clashes locally.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VAULT="$ROOT/tests/fixtures/vault"
HOST="127.0.0.1"
PORT="${ANWESEN_TEST_PORT:-18086}"
BIN="${ANWESEN_BIN:-$ROOT/target/debug/anwesen}"

if ! command -v hurl >/dev/null 2>&1; then
    echo "hurl not found in PATH; see README for install" >&2
    exit 127
fi

if [[ ! -x "$BIN" ]]; then
    echo "building debug anwesen binary"
    (cd "$ROOT" && cargo build --quiet)
fi

LOG="$(mktemp)"
"$BIN" serve --vault "$VAULT" --bind "$HOST:$PORT" --log-level warn >"$LOG" 2>&1 &
PID=$!
cleanup() {
    if kill -0 "$PID" 2>/dev/null; then
        kill "$PID" 2>/dev/null || true
        wait "$PID" 2>/dev/null || true
    fi
    if [[ "${HURL_RC:-0}" -ne 0 && -s "$LOG" ]]; then
        echo "--- anwesen serve log ---" >&2
        cat "$LOG" >&2
    fi
    rm -f "$LOG"
}
trap cleanup EXIT

# Wait up to ~10s for /health to come up.
ready=0
for _ in $(seq 1 50); do
    if curl -sf -o /dev/null "http://$HOST:$PORT/health"; then
        ready=1
        break
    fi
    sleep 0.2
done
if [[ $ready -ne 1 ]]; then
    echo "anwesen did not become ready on http://$HOST:$PORT" >&2
    exit 1
fi

# ANW-43: the offline `query` subcommand must return the document the endpoint
# returns. Both surfaces read the same fixture vault, so the two outputs are
# compared byte for byte before the contract suite runs.
for q in "" "tags=project" "__anw-path=Projects&__anw-limit=1" "status__exists=true"; do
    cli="$("$BIN" query --vault "$VAULT" --query "$q" --log-level error)"
    http="$(curl -sf "http://$HOST:$PORT/query?$q")"
    if [[ "$cli" != "$http" ]]; then
        echo "anwesen query and GET /query disagree for query '$q'" >&2
        echo "  cli:  $cli" >&2
        echo "  http: $http" >&2
        exit 1
    fi
done

# Run every *.hurl. Glob into an array so we can fail loud if there are none.
shopt -s globstar nullglob
files=("$ROOT"/tests/hurl/**/*.hurl)
if [[ ${#files[@]} -eq 0 ]]; then
    echo "no *.hurl files found under tests/hurl/" >&2
    exit 1
fi

set +e
hurl --test --variable "host=http://$HOST:$PORT" "${files[@]}"
HURL_RC=$?
set -e
exit "$HURL_RC"
