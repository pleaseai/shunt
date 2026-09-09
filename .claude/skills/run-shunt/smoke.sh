#!/usr/bin/env bash
# smoke.sh — build, launch, and drive shunt (the Claude Code LLM gateway) end to end.
#
# shunt is an Anthropic-Messages HTTP gateway. It has no GUI: you drive it with
# curl. This script proves the whole request path works WITHOUT any real provider
# credentials by pointing the `anthropic` provider at a local mock upstream, then
# exercising every route the server registers unconditionally:
#
#   shunt check                   -> config validation
#   HEAD /                        -> liveness probe
#   GET  /v1/models               -> model discovery
#   POST /v1/messages             -> proxy forward (mapped model -> mock upstream)
#   POST /v1/messages             -> routing error path (body without a model field)
#   GET  /health                  -> health report
#   GET  /protocol                -> machine-readable gateway contract
#   GET  /routes                  -> resolved route table
#   POST /v1/messages/count_tokens -> proxy forward on the second inference route
#
# Routes registered only by an optional config section (the Codex endpoint, the
# usage surfaces) are out of scope: this config does not enable them.
#
# Usage:  .claude/skills/run-shunt/smoke.sh
# Run from the repo root. Exits non-zero on the first failed assertion.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPO_ROOT"

SHUNT_PORT="${SHUNT_PORT:-31711}"
MOCK_PORT="${MOCK_PORT:-31712}"
WORKDIR="$(mktemp -d)"
CONFIG="$WORKDIR/shunt.smoke.toml"
SHUNT_LOG="$WORKDIR/shunt.log"
MOCK_LOG="$WORKDIR/mock.log"
MOCK_READY="$WORKDIR/mock.ready"
MOCK_PORT_FILE="$WORKDIR/mock.port"
BUILD_LOG="$WORKDIR/build.jsonl"

# Every assertion carries a deadline: a shunt that accepts the connection and
# then stalls must red the driver rather than hang it forever.
CURL_DEADLINE=(--connect-timeout 2 --max-time 10)

SHUNT_PID=""
MOCK_PID=""
cleanup() {
  if [ -n "$SHUNT_PID" ]; then
    kill "$SHUNT_PID" 2>/dev/null || true
    wait "$SHUNT_PID" 2>/dev/null || true
  fi
  if [ -n "$MOCK_PID" ]; then
    kill "$MOCK_PID" 2>/dev/null || true
    wait "$MOCK_PID" 2>/dev/null || true
  fi
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() {
  printf '  \033[31mFAIL\033[0m %s\n' "$1"
  if [ -s "$SHUNT_LOG" ]; then
    printf '%s\n' '--- shunt.log ---'
    cat "$SHUNT_LOG"
  fi
  if [ -s "$MOCK_LOG" ]; then
    printf '%s\n' '--- mock.log ---'
    cat "$MOCK_LOG"
  fi
  exit 1
}

validate_port() {
  local name=$1
  local value=$2
  # Five digits max: a longer literal makes `[ -gt ]` abort with "integer
  # expected" and a status the `if` reads as "not greater", so an unbounded
  # digit run would pass this check.
  if [[ ! $value =~ ^[0-9]{1,5}$ ]] || [ "$value" -gt 65535 ]; then
    fail "$name must be an integer from 0 to 65535, got '$value'"
  fi
}

job_running() {
  local wanted=$1
  local pid
  while IFS= read -r pid; do
    [ "$pid" = "$wanted" ] && return 0
  done < <(jobs -pr)
  return 1
}

listener_port() {
  local pid=$1
  local field
  while IFS= read -r field; do
    case "$field" in
      n127.0.0.1:*)
        printf '%s\n' "${field##*:}"
        return 0
        ;;
    esac
  done < <(lsof -nP -a -p "$pid" -iTCP -sTCP:LISTEN -Fn 2>/dev/null)
  return 1
}

for dependency in cargo curl jq lsof python3; do
  command -v "$dependency" >/dev/null 2>&1 || fail "required command not found: $dependency"
done
validate_port SHUNT_PORT "$SHUNT_PORT"
validate_port MOCK_PORT "$MOCK_PORT"

echo "==> Building shunt (cargo build)"
cargo build --locked --message-format=json-render-diagnostics > "$BUILD_LOG"
BIN="$(jq -sr '
  [ .[]
    | select(.reason == "compiler-artifact")
    | select(.target.name == "shunt")
    | select(.target.kind | index("bin"))
    | .executable
    | select(. != null)
  ] | last // empty
' "$BUILD_LOG")" || fail "failed to read shunt executable from cargo output"
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  fail "cargo build returned no shunt executable"
fi

echo "==> Starting mock upstream on :$MOCK_PORT (stands in for api.anthropic.com)"
python3 - "$MOCK_PORT" "$MOCK_READY" "$MOCK_PORT_FILE" > "$MOCK_LOG" 2>&1 <<'PY' &
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class H(BaseHTTPRequestHandler):
    def _send(self, code, body):
        payload = body.encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        # The path travels back in the body so the proxy assertion pins where
        # shunt forwarded to, not merely that something upstream answered.
        self._send(200, json.dumps({
            "id": "msg_mock_upstream", "type": "message", "role": "assistant",
            "model": "claude-opus-via-codex",
            "content": [{"type": "text", "text": f"hello from the mock upstream {self.path}"}],
        }))

    def log_message(self, *args):
        pass


server = HTTPServer(("127.0.0.1", int(sys.argv[1])), H)
with open(sys.argv[3], "w", encoding="utf-8") as port_file:
    port_file.write(str(server.server_port))
with open(sys.argv[2], "w", encoding="utf-8") as ready:
    ready.write("ready\n")
server.serve_forever()
PY
MOCK_PID=$!

echo "==> Waiting for mock upstream readiness"
for i in $(seq 1 50); do
  [ -s "$MOCK_READY" ] && break
  if ! job_running "$MOCK_PID"; then
    wait "$MOCK_PID" 2>/dev/null || true
    fail "mock upstream exited during startup: $(<"$MOCK_LOG")"
  fi
  sleep 0.1
  [ "$i" = 50 ] && fail "mock upstream did not become ready"
done
MOCK_PORT="$(<"$MOCK_PORT_FILE")"

echo "==> Writing smoke config -> $CONFIG"
cat > "$CONFIG" <<EOF
[server]
bind = "127.0.0.1:$SHUNT_PORT"
default_provider = "anthropic"

[providers.anthropic]
base_url = "http://127.0.0.1:$MOCK_PORT"

[providers.openai]
adapter = "responses"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
auth = "api_key"

[providers.codex]
adapter = "responses"
base_url = "https://chatgpt.com/backend-api"
auth = "chatgpt_oauth"

[[models]]
id = "claude-opus-via-codex"
display_name = "Opus (via Codex)"

[[routes]]
model = "claude-opus-via-codex"
provider = "anthropic"
EOF

echo "==> Test 1: config validation (shunt check)"
CHECK_OUT="$("$BIN" check --config "$CONFIG" 2>&1)" || fail "shunt check: $CHECK_OUT"
if [[ $CHECK_OUT == *"config ok"* ]]; then
  pass "shunt check -> config ok"
else
  fail "shunt check: $CHECK_OUT"
fi

echo "==> Starting shunt on :$SHUNT_PORT"
REQUESTED_SHUNT_PORT=$SHUNT_PORT
"$BIN" run --config "$CONFIG" > "$SHUNT_LOG" 2>&1 &
SHUNT_PID=$!

echo "==> Waiting for shunt to bind"
READY_PORT=""
for i in $(seq 1 50); do
  if ! job_running "$SHUNT_PID"; then
    wait "$SHUNT_PID" 2>/dev/null || true
    fail "shunt exited during startup: $(<"$SHUNT_LOG")"
  fi
  if READY_PORT="$(listener_port "$SHUNT_PID")"; then
    break
  fi
  READY_PORT=""
  sleep 0.1
  [ "$i" = 50 ] && fail "shunt bound no port: $(<"$SHUNT_LOG")"
done
if [ "$REQUESTED_SHUNT_PORT" -ne 0 ] && [ "$READY_PORT" -ne "$REQUESTED_SHUNT_PORT" ]; then
  fail "shunt listened on unexpected port $READY_PORT"
fi
SHUNT_PORT=$READY_PORT

echo "==> Waiting for readiness (HEAD /)"
# The health request carries its own deadline: a shunt holding the port without
# serving it must fail the driver rather than block it.
for i in $(seq 1 10); do
  curl -sf --connect-timeout 1 --max-time 1 -I "http://127.0.0.1:$SHUNT_PORT/" >/dev/null 2>&1 && break
  if ! job_running "$SHUNT_PID"; then
    wait "$SHUNT_PID" 2>/dev/null || true
    fail "shunt exited during startup: $(<"$SHUNT_LOG")"
  fi
  sleep 0.1
  [ "$i" = 10 ] && fail "shunt did not answer HEAD / in time: $(<"$SHUNT_LOG")"
done
pass "HEAD / -> 200 (server live)"

echo "==> Test 2: GET /v1/models (discovery)"
MODELS="$(curl -sf "${CURL_DEADLINE[@]}" "http://127.0.0.1:$SHUNT_PORT/v1/models?limit=1000")" ||
  fail "GET /v1/models failed: connection error, HTTP >= 400, or no answer in time"
if jq -e '.data[0].id == "claude-opus-via-codex"' >/dev/null <<<"$MODELS"; then
  pass "GET /v1/models returns configured model"
else
  fail "unexpected /v1/models: $MODELS"
fi

echo "==> Test 3: POST /v1/messages (proxy forward -> mock upstream)"
MSG="$(curl -sf "${CURL_DEADLINE[@]}" -X POST "http://127.0.0.1:$SHUNT_PORT/v1/messages" \
  -H 'content-type: application/json' -H 'x-api-key: dummy' \
  -d '{"model":"claude-opus-via-codex","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}')" ||
  fail "POST /v1/messages failed: connection error, HTTP >= 400, or no answer in time"
if jq -e '.content[0].text == "hello from the mock upstream /v1/messages"' >/dev/null <<<"$MSG"; then
  pass "POST /v1/messages proxied to upstream and returned its body"
else
  fail "unexpected /v1/messages: $MSG"
fi

echo "==> Test 4: POST /v1/messages with no model field (routing error path)"
CODE="$(curl -s "${CURL_DEADLINE[@]}" -o "$WORKDIR/err.json" -w '%{http_code}' -X POST \
  "http://127.0.0.1:$SHUNT_PORT/v1/messages" -H 'content-type: application/json' -d '{}')" ||
  fail "POST /v1/messages with no model field: no connection or no answer in time"
if [ "$CODE" = "400" ] && jq -e '.error.type == "invalid_request_error"' "$WORKDIR/err.json" >/dev/null; then
  pass "malformed request -> 400 invalid_request_error"
else
  fail "expected 400 invalid_request_error, got $CODE $(<"$WORKDIR/err.json")"
fi

echo "==> Test 5: GET /health"
HEALTH="$(curl -sf "${CURL_DEADLINE[@]}" "http://127.0.0.1:$SHUNT_PORT/health")" ||
  fail "GET /health failed: connection error, HTTP >= 400, or no answer in time"
if jq -e '.status == "ok"' >/dev/null <<<"$HEALTH"; then
  pass "GET /health reports ok"
else
  fail "unexpected /health: $HEALTH"
fi

echo "==> Test 6: GET /protocol (gateway contract)"
PROTOCOL="$(curl -sf "${CURL_DEADLINE[@]}" "http://127.0.0.1:$SHUNT_PORT/protocol")" ||
  fail "GET /protocol failed: connection error, HTTP >= 400, or no answer in time"
if jq -e '.name == "shunt" and .format == "anthropic-messages"
    and (.endpoints | map(.path) | index("/v1/messages")) != null' >/dev/null <<<"$PROTOCOL"; then
  pass "GET /protocol describes the anthropic-messages contract"
else
  fail "unexpected /protocol: $PROTOCOL"
fi

echo "==> Test 7: GET /routes (resolved route table)"
ROUTES="$(curl -sf "${CURL_DEADLINE[@]}" "http://127.0.0.1:$SHUNT_PORT/routes")" ||
  fail "GET /routes failed: connection error, HTTP >= 400, or no answer in time"
if jq -e '.data == [{"model": "claude-opus-via-codex", "provider": "anthropic"}]' >/dev/null <<<"$ROUTES"; then
  pass "GET /routes resolves the configured route"
else
  fail "unexpected /routes: $ROUTES"
fi

echo "==> Test 8: POST /v1/messages/count_tokens (proxy forward)"
COUNT="$(curl -sf "${CURL_DEADLINE[@]}" -X POST "http://127.0.0.1:$SHUNT_PORT/v1/messages/count_tokens" \
  -H 'content-type: application/json' -H 'x-api-key: dummy' \
  -d '{"model":"claude-opus-via-codex","messages":[{"role":"user","content":"hi"}]}')" ||
  fail "POST /v1/messages/count_tokens failed: connection error, HTTP >= 400, or no answer in time"
if jq -e '.content[0].text == "hello from the mock upstream /v1/messages/count_tokens"' >/dev/null <<<"$COUNT"; then
  pass "POST /v1/messages/count_tokens proxied to upstream on its own path"
else
  fail "unexpected /v1/messages/count_tokens: $COUNT"
fi

echo
printf '\033[32mAll smoke checks passed.\033[0m\n'
