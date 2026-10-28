#!/usr/bin/env bash
# End-to-end check of a real machine against the tutorial's example app
# (github.com/fghj-examples): wire it, bring up the `full` flow, and check
# every path a user relies on — DNS through the OS, HTTPS through the proxy,
# raw ports, and the services talking to each other.
#
# Runs as you, not root, against an fghjd that's already running:
#   sudo brew services start fghj    # macOS
#   sudo fghjd                       # or in the foreground, anywhere
#   e2e/files-example.sh [--fresh]
#
# FGHJ (default: `fghj` on PATH) picks the CLI; FGHJ_E2E_WORKSPACE (default:
# ~/fghj-e2e/files-e2e) the workspace, which is reused between runs so repos
# and images aren't fetched again. --fresh deletes it first.
set -uo pipefail

FGHJ="${FGHJ:-fghj}"
WS="${FGHJ_E2E_WORKSPACE:-$HOME/fghj-e2e/files-e2e}"
ENTRY=https://github.com/fghj-examples/files-web.git
FLOW=files-web/full
PROXY_IP=127.222.0.1

case "$(uname -s)" in
  Darwin) STATE_ROOT=/private/var/lib/fghjd ;;
  *) STATE_ROOT=/var/lib/fghjd ;;
esac
CA="$STATE_ROOT/certs/cert.pem"

pass=0 fail=0
ok()   { pass=$((pass + 1)); printf '✓ %s\n' "$1"; }
bad()  { fail=$((fail + 1)); printf '✗ %s\n' "$1"; [ -n "${2:-}" ] && printf '    %s\n' "$2"; }
check() { local name=$1; shift; local out; if out=$("$@" 2>&1); then ok "$name"; else bad "$name" "$(tail -n 3 <<<"$out")"; fi; }
step() { printf '\n== %s\n' "$*"; }

for tool in jq curl; do
  command -v "$tool" >/dev/null || { echo "needs $tool"; exit 2; }
done

if [ "${1:-}" = --fresh ]; then
  rm -rf "$WS"
fi
mkdir -p "$WS"

step "doctor, before wiring"
"$FGHJ" doctor || true

step "wire $ENTRY into $WS"
"$FGHJ" wire "$ENTRY" --workspace "$WS" || { bad "wire"; exit 1; }
ok "wire"

step "pull every repo the workspace references (the UI's Download all)"
"$FGHJ" pull --workspace "$WS" || { bad "pull"; exit 1; }
ok "pull"

step "up $FLOW (builds on a first run take a few minutes)"
if ! "$FGHJ" up "$FLOW" --workspace "$WS"; then
  bad "up $FLOW"
  "$FGHJ" status --workspace "$WS"
  printf '\n%d passed, %d failed: nothing else can be checked without the flow up\n' "$pass" "$fail"
  exit 1
fi
ok "up $FLOW"

step "status"
"$FGHJ" status --workspace "$WS"
STATUS=$("$FGHJ" status --workspace "$WS" --json) || { bad "status --json"; exit 1; }

node() { jq -c --arg id "$1" '.containers[$id] // empty' <<<"$STATUS"; }
route() { node "$1" | jq -r '[.desired.routes[] | select(.wildcard != true)][0].domain // empty'; }
raw_domain() { node "$1" | jq -r '.desired.raw_domain // empty'; }

step "nodes"
for id in web.files-web api.files-api db.files-api s3.files-api \
  redis.files-platform worker.files-worker; do
  condition=$(node "$id" | jq -r '.condition // "missing"')
  if [ "$condition" = running ]; then ok "$id is running"; else bad "$id is running" "it's $condition"; fi
done

WEB=$(route web.files-web)
S3=$(route s3.files-api)
DB_RAW=$(raw_domain db.files-api)
S3_RAW=$(raw_domain s3.files-api)
REDIS_RAW=$(raw_domain redis.files-platform)

# What the OS resolver gives a program, not what fghjd's server would say.
resolve() {
  if command -v getent >/dev/null; then
    getent ahostsv4 "$1" | awk '{print $1; exit}'
  else
    dscacheutil -q host -a name "$1" | awk '/ip_address/ {print $2; exit}'
  fi
}

resolves_to() { [ "$(resolve "$1")" = "$2" ]; }
in_raw_pool() { [[ $(resolve "$1") == 127.223.* ]]; }
resolves_outside() { local ip; ip=$(resolve "$1"); [[ -n $ip && $ip != 127.* ]]; }

step "DNS through the OS"
check "$WEB resolves to the proxy" resolves_to "$WEB" "$PROXY_IP"
check "$DB_RAW resolves into the raw pool" in_raw_pool "$DB_RAW"
check "other names still resolve normally" resolves_outside github.com

step "HTTPS through the proxy"
code() { curl -sS -o /dev/null -w '%{http_code}' -m 20 "$@"; }
check "web over fghj's CA" test "$(code --cacert "$CA" "https://$WEB/")" = 200
check "api through web's /api/" test "$(code --cacert "$CA" "https://$WEB/api/health")" = 200
check "s3 over the http zone" test "$(code --cacert "$CA" "https://$S3/minio/health/live")" = 200
check "the system trusts fghj's CA" test "$(code "https://$WEB/")" = 200

tcp_open() { (exec 3<>"/dev/tcp/$(resolve "$1")/$2") 2>&1; }
redis_pong() {
  local reply
  exec 3<>"/dev/tcp/$(resolve "$1")/$2" || return 1
  printf 'PING\r\n' >&3
  read -t 5 -r reply <&3
  exec 3>&-
  [[ $reply == +PONG* ]]
}

step "raw zone"
check "s3 on $S3_RAW:9000" test "$(code "http://$S3_RAW:9000/minio/health/live")" = 200
check "postgres on $DB_RAW:5432" tcp_open "$DB_RAW" 5432
check "redis answers PING on $REDIS_RAW:6379" redis_pong "$REDIS_RAW" 6379

step "doctor, after"
if "$FGHJ" doctor; then ok "doctor"; else bad "doctor"; fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
