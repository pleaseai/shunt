#!/usr/bin/env bash
# Prove smoke.sh cannot report a green against the wrong process, and that it
# refuses a bad port before it asserts anything.
#
#   run_collision   a listener already holds the port, so the real process dies
#                   at bind and the driver names which one died.
#   run_wrong_port  shunt is alive on a port the driver never asked for; the
#                   pid-derived readiness check is what catches it.
#   run_bad_port    a port outside 0..65535 is refused at the guard, by name.
#
# The impostor answers every assertion smoke.sh makes. That is what gives the
# collision cases their teeth: a driver that trusted $SHUNT_PORT instead of the
# process it started would go fully green against this listener.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
WORKDIR="$(mktemp -d)"
PORT_FILE="$WORKDIR/port"
IMPOSTOR_LOG="$WORKDIR/impostor.log"
IMPOSTOR_PID=""

cleanup() {
  if [ -n "$IMPOSTOR_PID" ]; then
    kill "$IMPOSTOR_PID" 2>/dev/null || true
    wait "$IMPOSTOR_PID" 2>/dev/null || true
  fi
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

start_impostor() {
  python3 - "$PORT_FILE" > "$IMPOSTOR_LOG" 2>&1 <<'PY' &
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def send_json(self, code, value):
        payload = json.dumps(value).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_HEAD(self):
        self.send_response(200)
        self.send_header("content-length", "0")
        self.end_headers()

    def do_GET(self):
        route = self.path.split("?")[0]
        if route == "/health":
            self.send_json(200, {"status": "ok", "version": "0.0.0-impostor"})
        elif route == "/protocol":
            self.send_json(200, {
                "name": "shunt",
                "format": "anthropic-messages",
                "endpoints": [{"path": "/v1/messages"}],
            })
        elif route == "/routes":
            self.send_json(200, {
                "data": [{"model": "claude-opus-via-codex", "provider": "anthropic"}],
            })
        else:
            self.send_json(200, {"data": [{"id": "claude-opus-via-codex"}]})

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        body = json.loads(self.rfile.read(length) or b"{}")
        if "model" not in body:
            self.send_json(400, {"error": {"type": "invalid_request_error"}})
        else:
            self.send_json(200, {
                "id": "msg_impostor",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": f"hello from the mock upstream {self.path}"}],
            })

    def log_message(self, *args):
        pass


server = HTTPServer(("127.0.0.1", 0), Handler)
with open(sys.argv[1], "w", encoding="utf-8") as output:
    output.write(str(server.server_port))
server.serve_forever()
PY
  IMPOSTOR_PID=$!
  local ready=0
  for _ in $(seq 1 50); do
    if [ -s "$PORT_FILE" ]; then
      ready=1
      break
    fi
    if ! job_running "$IMPOSTOR_PID"; then
      wait "$IMPOSTOR_PID" 2>/dev/null || true
      printf 'impostor failed: %s\n' "$(<"$IMPOSTOR_LOG")" >&2
      exit 1
    fi
    sleep 0.1
  done
  if [ "$ready" != 1 ]; then
    printf 'impostor did not report its port\n' >&2
    exit 1
  fi
  assert_impostor_answers "$(<"$PORT_FILE")"
}

# Drive the impostor's own answers on every run. Left unexercised, a typo in
# them would surface only on the day a mutation needs it to look convincing,
# turning an "accepted an impostor" red into an expected-string red.
assert_impostor_answers() {
  local port=$1
  local body
  local deadline=(--connect-timeout 2 --max-time 10)
  curl -sf "${deadline[@]}" -I "http://127.0.0.1:$port/" >/dev/null ||
    { printf 'impostor did not answer HEAD /\n' >&2; exit 1; }
  body="$(curl -sf "${deadline[@]}" -X POST "http://127.0.0.1:$port/v1/messages" \
    -H 'content-type: application/json' -d '{"model":"claude-opus-via-codex"}')" ||
    { printf 'impostor did not answer POST /v1/messages\n' >&2; exit 1; }
  jq -e '.content[0].text == "hello from the mock upstream /v1/messages"' >/dev/null <<<"$body" ||
    { printf 'impostor message body would not satisfy the driver: %s\n' "$body" >&2; exit 1; }
  body="$(curl -sf "${deadline[@]}" "http://127.0.0.1:$port/health")" ||
    { printf 'impostor did not answer GET /health\n' >&2; exit 1; }
  jq -e '.status == "ok"' >/dev/null <<<"$body" ||
    { printf 'impostor health body would not satisfy the driver: %s\n' "$body" >&2; exit 1; }
}

job_running() {
  local wanted=$1
  local pid
  while IFS= read -r pid; do
    [ "$pid" = "$wanted" ] && return 0
  done < <(jobs -pr)
  return 1
}

run_collision() {
  local occupied=$1
  local occupied_port
  local output
  local expected

  : > "$PORT_FILE"
  start_impostor
  occupied_port="$(<"$PORT_FILE")"
  if [ "$occupied" = mock ]; then
    expected="mock upstream exited during startup"
    if output="$(SHUNT_PORT=0 MOCK_PORT="$occupied_port" \
      timeout 600 "$REPO_ROOT/.claude/skills/run-shunt/smoke.sh" 2>&1)"; then
      printf 'smoke accepted a mock-port impostor\n%s\n' "$output" >&2
      exit 1
    fi
  else
    expected="shunt exited during startup"
    if output="$(SHUNT_PORT="$occupied_port" MOCK_PORT=0 \
      timeout 600 "$REPO_ROOT/.claude/skills/run-shunt/smoke.sh" 2>&1)"; then
      printf 'smoke accepted a shunt-port impostor\n%s\n' "$output" >&2
      exit 1
    fi
  fi
  [[ $output == *"$expected"* ]] || {
    printf 'smoke rejected %s-port impostor without %s\n%s\n' "$occupied" "$expected" "$output" >&2
    exit 1
  }

  kill "$IMPOSTOR_PID"
  wait "$IMPOSTOR_PID" 2>/dev/null || true
  IMPOSTOR_PID=""
}

run_bad_port() {
  local output
  # validate_port runs before the build, so this case costs nothing.
  if output="$(SHUNT_PORT=99999999999999999999999 MOCK_PORT=0 \
    timeout 600 "$REPO_ROOT/.claude/skills/run-shunt/smoke.sh" 2>&1)"; then
    printf 'smoke accepted a port outside 0..65535\n%s\n' "$output" >&2
    exit 1
  fi
  [[ $output == *"SHUNT_PORT must be an integer from 0 to 65535"* ]] || {
    printf 'smoke rejected the bad port without naming the guard\n%s\n' "$output" >&2
    exit 1
  }
}

run_wrong_port() {
  local output
  # SHUNT_SERVER__BIND outranks the config file, so shunt binds an ephemeral
  # port while the driver asked for a fixed one.
  if output="$(SHUNT_PORT=31799 MOCK_PORT=0 SHUNT_SERVER__BIND=127.0.0.1:0 \
    timeout 600 "$REPO_ROOT/.claude/skills/run-shunt/smoke.sh" 2>&1)"; then
    printf 'smoke accepted a shunt bound to a port it never requested\n%s\n' "$output" >&2
    exit 1
  fi
  [[ $output == *"shunt listened on unexpected port"* ]] || {
    printf 'smoke rejected the wrong-port shunt without naming the port\n%s\n' "$output" >&2
    exit 1
  }
}

run_bad_port
run_collision mock
run_collision shunt
run_wrong_port

if ! positive_output="$(SHUNT_PORT=0 MOCK_PORT=0 \
  timeout 600 "$REPO_ROOT/.claude/skills/run-shunt/smoke.sh" 2>&1)"; then
  printf 'positive smoke failed\n%s\n' "$positive_output" >&2
  exit 1
fi
printf 'smoke readiness tests ok\n'
