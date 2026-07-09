#!/usr/bin/env bash
# smoke.sh — build, launch, and drive shunt (the Claude Code LLM gateway) end to end.
#
# shunt is an Anthropic-Messages HTTP gateway. It has no GUI: you drive it with
# curl. This script proves the whole request path works WITHOUT any real provider
# credentials by pointing the `anthropic` provider at a local mock upstream, then
# exercising every route the server exposes:
#
#   shunt check            -> config validation
#   HEAD /                 -> liveness probe
#   GET  /v1/models        -> model discovery
#   POST /v1/messages      -> proxy forward (mapped model -> mock upstream)
#   POST /v1/messages      -> routing error path (body without a model field)
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
BIN="$REPO_ROOT/target/debug/shunt"

SHUNT_PID=""
MOCK_PID=""
cleanup() {
  [ -n "$SHUNT_PID" ] && kill "$SHUNT_PID" 2>/dev/null || true
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; echo "--- shunt.log ---"; cat "$SHUNT_LOG" 2>/dev/null; exit 1; }

echo "==> Building shunt (cargo build)"
cargo build 2>&1 | tail -1

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
CHECK_OUT="$("$BIN" check --config "$CONFIG" 2>&1)"
echo "$CHECK_OUT" | grep -q "config ok" && pass "shunt check -> config ok" || fail "shunt check: $CHECK_OUT"

echo "==> Starting mock upstream on :$MOCK_PORT (stands in for api.anthropic.com)"
python3 - "$MOCK_PORT" > "$MOCK_LOG" 2>&1 <<'PY' &
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def _send(self, code, body):
        payload = body.encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
    def do_HEAD(self):  # readiness probe target
        self.send_response(200)
        self.send_header("content-length", "0")
        self.end_headers()
    def do_POST(self):
        # Canned Anthropic Messages response — proves shunt forwarded the request.
        self._send(200, json.dumps({
            "id": "msg_mock_upstream", "type": "message", "role": "assistant",
            "model": "claude-opus-via-codex",
            "content": [{"type": "text", "text": "hello from the mock upstream"}],
        }))
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
MOCK_PID=$!

echo "==> Waiting for mock upstream readiness"
for i in $(seq 1 50); do
  curl -s -I "http://127.0.0.1:$MOCK_PORT/" >/dev/null 2>&1 && break
  kill -0 "$MOCK_PID" 2>/dev/null || fail "mock upstream exited during startup: $(cat "$MOCK_LOG")"
  sleep 0.1
  [ "$i" = 50 ] && fail "mock upstream did not become ready"
done

echo "==> Starting shunt on :$SHUNT_PORT"
"$BIN" run --config "$CONFIG" > "$SHUNT_LOG" 2>&1 &
SHUNT_PID=$!

echo "==> Waiting for readiness (HEAD /)"
for i in $(seq 1 50); do
  if curl -sf -I "http://127.0.0.1:$SHUNT_PORT/" >/dev/null 2>&1; then break; fi
  kill -0 "$SHUNT_PID" 2>/dev/null || fail "shunt exited during startup"
  sleep 0.1
  [ "$i" = 50 ] && fail "shunt did not become ready"
done
pass "HEAD / -> 200 (server live)"

echo "==> Test 2: GET /v1/models (discovery)"
MODELS="$(curl -sf "http://127.0.0.1:$SHUNT_PORT/v1/models?limit=1000")"
echo "$MODELS" | jq -e '.data[0].id == "claude-opus-via-codex"' >/dev/null \
  && pass "GET /v1/models returns configured model" || fail "unexpected /v1/models: $MODELS"

echo "==> Test 3: POST /v1/messages (proxy forward -> mock upstream)"
MSG="$(curl -sf -X POST "http://127.0.0.1:$SHUNT_PORT/v1/messages" \
  -H 'content-type: application/json' -H 'x-api-key: dummy' \
  -d '{"model":"claude-opus-via-codex","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}')"
echo "$MSG" | jq -e '.content[0].text == "hello from the mock upstream"' >/dev/null \
  && pass "POST /v1/messages proxied to upstream and returned its body" || fail "unexpected /v1/messages: $MSG"

echo "==> Test 4: POST /v1/messages with no model field (routing error path)"
CODE="$(curl -s -o "$WORKDIR/err.json" -w '%{http_code}' -X POST \
  "http://127.0.0.1:$SHUNT_PORT/v1/messages" -H 'content-type: application/json' -d '{}')"
[ "$CODE" = "400" ] && jq -e '.error.type == "invalid_request_error"' "$WORKDIR/err.json" >/dev/null \
  && pass "malformed request -> 400 invalid_request_error" \
  || fail "expected 400 invalid_request_error, got $CODE $(cat "$WORKDIR/err.json")"

echo
printf '\033[32mAll smoke checks passed.\033[0m\n'
