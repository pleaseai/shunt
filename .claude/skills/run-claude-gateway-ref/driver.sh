#!/usr/bin/env bash
# Driver for the local Claude apps gateway reference deployment.
# See SKILL.md in this directory. Every step here was verified live
# against claude gateway 2.1.211 + ghcr.io/dexidp/dex:latest (2026-07-16).
#
# Usage: driver.sh <up|login|probe|sink-start|sink-stop|status|down>
set -euo pipefail

SKILL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="${GWREF_WORK:-/tmp/shunt-gw-probe}"
CAP="$WORK/capture"
BASE=http://127.0.0.1:8790
DEX=http://127.0.0.1:5556
PG_PORT=55432
SINK_PORT=44318

# Extract a Location header (case-insensitive, CR-stripped) from a
# curl -D dump file.
loc() { awk 'tolower($1)=="location:"{print $2}' "$1" | tr -d '\r' | tail -1; }
jget() { python3 -c "import json,sys;print(json.load(open(sys.argv[1]))[sys.argv[2]])" "$1" "$2"; }

cmd_up() {
  mkdir -p "$CAP"
  docker rm -f shunt-gw-pg shunt-gw-dex >/dev/null 2>&1 || true
  docker run --rm -d --name shunt-gw-pg -p "$PG_PORT":5432 \
    -e POSTGRES_HOST_AUTH_METHOD=trust postgres:16-alpine >/dev/null
  docker run --rm -d --name shunt-gw-dex -p 5556:5556 \
    -v "$SKILL_DIR/dex.yaml":/etc/dex/config.yaml:ro \
    ghcr.io/dexidp/dex:v2.44.0 dex serve /etc/dex/config.yaml >/dev/null
  for _ in $(seq 1 30); do
    curl -sf "$DEX/dex/.well-known/openid-configuration" >/dev/null 2>&1 && break
    sleep 1
  done
  for _ in $(seq 1 30); do
    docker exec shunt-gw-pg pg_isready -U postgres -q 2>/dev/null && break
    sleep 1
  done
  CLAUDE_GATEWAY_ALLOW_LOOPBACK=1 nohup claude gateway \
    --config "$SKILL_DIR/gateway.yaml" >"$WORK/gateway.log" 2>&1 &
  echo $! >"$WORK/gateway.pid"
  for _ in $(seq 1 30); do
    curl -sf -o /dev/null "$BASE/healthz" 2>/dev/null && break
    sleep 1
  done
  curl -s -o /dev/null -w "gateway healthz: %{http_code}\n" "$BASE/healthz"
  echo "log: $WORK/gateway.log"
}

# Walk the full RFC 8628 device flow non-interactively (Dex static
# password stands in for the browser leg) and leave a bearer in
# $CAP/token.json.
cmd_login() {
  mkdir -p "$CAP"; cd "$CAP"
  rm -f gw-cookies.txt dex-cookies.txt

  curl -s "$BASE/.well-known/oauth-authorization-server" >discovery.json
  DA=$(jget discovery.json device_authorization_endpoint)
  TE=$(jget discovery.json token_endpoint)

  curl -s -X POST "$DA" -d 'client_id=claude-code' >device_auth.json
  DC=$(jget device_auth.json device_code)
  UC=$(jget device_auth.json user_code)
  echo "user_code: $UC"

  # Browser leg. The /device confirm POST is CSRF-guarded: without
  # same-origin Origin/Referer/Sec-Fetch-Site it returns 200 with
  # "request came from another site and was blocked".
  curl -s -c gw-cookies.txt -o /dev/null "$BASE/device?user_code=$UC"
  curl -s -b gw-cookies.txt -c gw-cookies.txt -X POST "$BASE/device" \
    -H "Origin: $BASE" -H "Referer: $BASE/device?user_code=$UC" \
    -H "Sec-Fetch-Site: same-origin" \
    -d "user_code=$UC" -D h-device.txt -o /dev/null
  AUTH_URL=$(loc h-device.txt)
  [ -n "$AUTH_URL" ] || { echo "device POST did not redirect (CSRF?)"; exit 1; }

  # Dex: /dex/auth → /dex/auth/local → login form → POST credentials.
  curl -s -c dex-cookies.txt -D h1.txt -o /dev/null "$AUTH_URL"
  L1=$(loc h1.txt)
  curl -s -b dex-cookies.txt -c dex-cookies.txt -D h2.txt -o /dev/null "$DEX$L1"
  L2=$(loc h2.txt)
  curl -s -b dex-cookies.txt -c dex-cookies.txt -o /dev/null "$DEX$L2"
  curl -s -b dex-cookies.txt -c dex-cookies.txt -X POST "$DEX$L2" \
    --data-urlencode 'login=dev@example.com' --data-urlencode 'password=password' \
    -D h3.txt -o /dev/null
  CB=$(loc h3.txt)
  case "$CB" in "$BASE"/oauth/callback*) ;; *) echo "unexpected callback: $CB"; exit 1;; esac

  # Gateway callback needs the gw_dev cookie from the /device POST.
  curl -s -b gw-cookies.txt -o /dev/null "$CB"

  for _ in $(seq 1 12); do
    curl -s -X POST "$TE" -H 'content-type: application/x-www-form-urlencoded' \
      -d "grant_type=urn:ietf:params:oauth:grant-type:device_code&device_code=$DC&client_id=claude-code" \
      >token.json
    grep -q access_token token.json && break
    sleep 5
  done
  grep -q access_token token.json || { echo "token poll never succeeded:"; cat token.json; exit 1; }
  echo "bearer captured → $CAP/token.json"
}

# Capture the bearer-authenticated surface into $CAP/. Run login first.
cmd_probe() {
  cd "$CAP"
  AT=$(jget token.json access_token)
  RT=$(jget token.json refresh_token)

  curl -s "$BASE/protocol" >protocol.md
  echo "protocol.md: $(wc -c <protocol.md) bytes"

  curl -s -D h-ms.txt -H "Authorization: Bearer $AT" \
    "$BASE/managed/settings" -o managed_settings.json
  python3 -m json.tool managed_settings.json
  ET=$(awk 'tolower($1)=="etag:"{print $2}' h-ms.txt | tr -d '\r')
  curl -s -o /dev/null -w "managed/settings If-None-Match: %{http_code} (expect 304)\n" \
    -H "Authorization: Bearer $AT" -H "If-None-Match: $ET" "$BASE/managed/settings"

  curl -s -H "Authorization: Bearer $AT" "$BASE/v1/models" -o models.json
  echo "models: $(python3 -c "import json;print([m['id'] for m in json.load(open('models.json'))['data']])")"

  curl -s -o token_refresh.json -w "refresh grant: %{http_code} (expect 200)\n" \
    -X POST "$BASE/oauth/token" -H 'content-type: application/x-www-form-urlencoded' \
    -d "grant_type=refresh_token&refresh_token=$RT&client_id=claude-code"

  # Minimal OTLP/HTTP JSON metric — enough for the relay to forward.
  curl -s -o /dev/null -w "POST /v1/metrics: %{http_code} (expect 200)\n" \
    -X POST "$BASE/v1/metrics" -H "Authorization: Bearer $AT" \
    -H 'content-type: application/json' \
    -d '{"resourceMetrics":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"probe"}}]},"scopeMetrics":[{"scope":{"name":"probe"},"metrics":[{"name":"claude_code.token.usage","unit":"tokens","sum":{"dataPoints":[{"asInt":"42","timeUnixNano":"1700000000000000000","attributes":[{"key":"type","value":{"stringValue":"input"}}]}],"aggregationTemporality":2,"isMonotonic":true}}]}]}]}'

  curl -s -o bootstrap.json -w "GET /user/bootstrap: %{http_code} (404 unless a policy has desktop:)\n" \
    -H "Authorization: Bearer $AT" "$BASE/user/bootstrap"

  echo "captures in $CAP/"
}

# Tiny OTLP sink so the telemetry relay path can be observed
# (gateway.yaml forward_to points here). Prints every POST it receives.
cmd_sink_start() {
  mkdir -p "$WORK"
  nohup python3 -u -c "
from http.server import HTTPServer, BaseHTTPRequestHandler
class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get('content-length', 0) or 0)
        body = self.rfile.read(n)
        print('==', self.path, self.headers.get('content-type'), f'{n}B', flush=True)
        print('   headers:', {k: v for k, v in self.headers.items()
                              if k.lower() not in ('host', 'connection', 'accept-encoding')}, flush=True)
        print('   body:', body[:2000], flush=True)
        self.send_response(200); self.end_headers()
    def log_message(self, *a): pass
HTTPServer(('127.0.0.1', $SINK_PORT), H).serve_forever()
" >"$WORK/sink.log" 2>&1 &
  echo $! >"$WORK/sink.pid"
  echo "sink on :$SINK_PORT, log: $WORK/sink.log"
}

cmd_sink_stop() {
  [ -f "$WORK/sink.pid" ] && kill "$(cat "$WORK/sink.pid")" 2>/dev/null || true
  rm -f "$WORK/sink.pid"
}

cmd_status() {
  curl -s -o /dev/null -w "gateway healthz: %{http_code}\n" "$BASE/healthz" || true
  curl -s -o /dev/null -w "dex discovery:   %{http_code}\n" "$DEX/dex/.well-known/openid-configuration" || true
  docker ps --format '{{.Names}}\t{{.Status}}' | grep -E 'shunt-gw' || echo "no shunt-gw containers"
}

cmd_down() {
  [ -f "$WORK/gateway.pid" ] && kill "$(cat "$WORK/gateway.pid")" 2>/dev/null || true
  rm -f "$WORK/gateway.pid"
  cmd_sink_stop
  docker rm -f shunt-gw-pg shunt-gw-dex >/dev/null 2>&1 || true
  echo "down"
}

case "${1:-}" in
  up) cmd_up ;;
  login) cmd_login ;;
  probe) cmd_probe ;;
  sink-start) cmd_sink_start ;;
  sink-stop) cmd_sink_stop ;;
  status) cmd_status ;;
  down) cmd_down ;;
  *) echo "usage: $0 <up|login|probe|sink-start|sink-stop|status|down>"; exit 2 ;;
esac
