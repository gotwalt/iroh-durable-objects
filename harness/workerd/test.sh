#!/usr/bin/env bash
# End-to-end test of the iroh-do Worker under local workerd (`wrangler dev`).
#
#   harness/workerd/test.sh [--build] [--port N]
#
# Checks, against the bundle in build/worker/:
#   1. router: a bad or missing admin token gets 401; unminted, non-z32 and hex
#      labels get 404; /ping and /generate_204 are answered by the router;
#   2. mint returns {id, relay_url, published} with a 52-char z32 id;
#   3. iroh-do-chat (stock iroh 1.4, default features) reaches
#      http://<E>.localhost:<port>/: echo/1 round trips, ctl/1 lines, /status
#      and the object's unprompted uni stream;
#   4. `lifecycle`: the object closes the idle connection, hibernates (the
#      persisted `wakes` counter increases between sessions), and a /callback
#      dial-back reaches the peer afterwards over the peer's own relay socket.
# Starts its own wrangler dev and always stops it.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
port=8787
build=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --build) build=1 ;;
    --port) port="$2"; shift ;;
    *) echo "usage: $0 [--build] [--port N]" >&2; exit 2 ;;
  esac
  shift
done

work="$(mktemp -d "${TMPDIR:-/tmp}/iroh-do-workerd.XXXXXX")"
log="$work/wrangler.log"
pid=""
cleanup() {
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  # wrangler spawns workerd; make sure nothing of ours keeps the port.
  pkill -f "wrangler dev --port $port" 2>/dev/null || true
  echo "logs: $work"
}
trap cleanup EXIT

if [[ $build -eq 1 || ! -f "$repo/build/worker/index_bg.wasm" ]]; then
  "$repo/crates/iroh-do/build-worker.sh"
fi
(cd "$repo" && cargo build -q -p iroh-do-demo)
demo="$repo/target/debug/iroh-do-chat"

token="$(openssl rand -hex 24)"
# .dev.vars is gitignored; it carries the local ADMIN_TOKEN secret.
printf 'ADMIN_TOKEN=%s\n' "$token" > "$here/.dev.vars"

(cd "$here" && WRANGLER_SEND_METRICS=false CI=1 exec wrangler dev --port "$port" --ip 127.0.0.1 \
  --inspector-port $((port + 1000)) --persist-to "$work/state" > "$log" 2>&1) &
pid=$!
for _ in $(seq 1 120); do
  grep -q "Ready on" "$log" 2>/dev/null && break
  sleep 0.5
done
grep -q "Ready on" "$log" || { echo "wrangler dev did not start"; cat "$log"; exit 1; }

base="http://127.0.0.1:$port"
fails=0
check() { # name, expected, actual
  if [[ "$2" == "$3" ]]; then echo "PASS $1"; else echo "FAIL $1: expected $2, got $3"; fails=$((fails + 1)); fi
}
status() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
ws_headers=(-H "Upgrade: websocket" -H "Connection: Upgrade" -H "Sec-WebSocket-Version: 13"
  -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" -H "Sec-WebSocket-Protocol: iroh-relay-v2, iroh-relay-v1")

# --- router -----------------------------------------------------------------
check "mint with bad token -> 401" 401 "$(status -X POST -H "Host: admin.localhost:$port" -H "Authorization: Bearer nope" "$base/mint")"
check "mint without token -> 401" 401 "$(status -X POST -H "Host: admin.localhost:$port" "$base/mint")"
check "GET /mint -> 405" 405 "$(status -H "Host: admin.localhost:$port" "$base/mint")"
unminted="yr9ge793ppirfkkm4edbyu56mbnctp4i9xsf3468xsda9o8cuo6o"
check "unminted z32 upgrade -> 404 (no identity, no write)" 404 \
  "$(status --max-time 5 "${ws_headers[@]}" -H "Host: $unminted.localhost:$port" "$base/relay")"
check "non-z32 label -> 404 (no DO touched)" 404 "$(status "${ws_headers[@]}" -H "Host: notanid.localhost:$port" "$base/relay")"
check "hex label -> 404" 404 "$(status "${ws_headers[@]}" -H "Host: $(printf '%064d' 0).localhost:$port" "$base/relay")"
check "/_internal never routed -> 404" 404 "$(status -X POST -H "Host: $unminted.localhost:$port" "$base/_internal/mint")"
check "relay without upgrade -> 426" 426 "$(status -H "Host: $unminted.localhost:$port" "$base/relay")"
check "/ping answered by router" 200 "$(status -H "Host: $unminted.localhost:$port" "$base/ping")"
check "/generate_204 answered by router" 204 "$(status -H "Host: $unminted.localhost:$port" "$base/generate_204")"

# --- mint -------------------------------------------------------------------
mint="$(curl -s -X POST -H "Host: admin.localhost:$port" -H "Authorization: Bearer $token" "$base/mint")"
echo "mint: $mint"
E="$(python3 -c 'import sys,json; print(json.loads(sys.argv[1])["id"])' "$mint")"
relay="$(python3 -c 'import sys,json; print(json.loads(sys.argv[1])["relay_url"])' "$mint")"
check "relay_url is http://<E>.localhost:$port/" "http://$E.localhost:$port/" "$relay"
check "minted id is 52-char z32" 52 "${#E}"
check "upgrade to minted id -> 101" 101 "$(status --max-time 2 "${ws_headers[@]}" -H "Host: $E.localhost:$port" "$base/relay" || true)"

# --- stock client: echo + ctl ----------------------------------------------
export RUST_LOG="${RUST_LOG:-warn}"
if "$demo" --local --local-port "$port" ping "$E" -c 2 | tee "$work/ping.txt" | grep -c "echo 32 B ok" | grep -q 2; then
  echo "PASS echo/1 x2 through the DO"
else
  echo "FAIL echo/1"; cat "$work/ping.txt"; fails=$((fails + 1))
fi
printf 'hello\n/status\n/quit\n' | "$demo" --local --local-port "$port" ctl "$E" > "$work/ctl.txt" 2>&1 || true
grep -q "echo: hello" "$work/ctl.txt" && echo "PASS ctl/1 echo line" || { echo "FAIL ctl/1 echo"; fails=$((fails + 1)); }
grep -q "\"state\":\"Active\"" "$work/ctl.txt" && echo "PASS ctl/1 /status" || { echo "FAIL ctl/1 /status"; fails=$((fails + 1)); }
grep -q "(uni) hello from $E" "$work/ctl.txt" && echo "PASS DO-originated uni stream" || { echo "FAIL uni stream"; fails=$((fails + 1)); }

# --- lifecycle: idle close, hibernation, dial-back after hibernation ---------
"$demo" --local --local-port "$port" lifecycle "$E" --json "$work/lifecycle.json" | tee "$work/lifecycle.txt" || true
python3 - "$work/lifecycle.json" <<'EOF' || fails=$((fails + 1))
import json, sys
rep = json.load(open(sys.argv[1]))
need = [
    "s1 connect ctl/1",
    "s1 echo",
    "s1 unprompted uni stream",
    "DO closes idle connection",
    "s2 connect ctl/1",
    "s2 wakes increased",
    "notify/1 dial-back from DO received",
]
bad = 0
for prefix in need:
    rows = [r for r in rep["rows"] if r["step"].startswith(prefix)]
    ok = rows and all(r["verdict"] == "pass" for r in rows)
    print(("PASS " if ok else "FAIL ") + "lifecycle: " + prefix + ("" if ok else f" {rows}"))
    bad += 0 if ok else 1
hib = [r for r in rep["rows"] if r["step"].startswith("DO hibernated before dial-back")]
print("info lifecycle: " + (hib[0]["detail"] if hib else "no hibernation row"))
sys.exit(1 if bad else 0)
EOF

echo
grep -E '"event":"(wake|alarm|pkarr_published|pkarr_publish_failed|identity_invariant_violated)"' "$log" | tail -20 || true
echo
if [[ $fails -eq 0 ]]; then echo "ALL PASS"; else echo "$fails FAILURE(S)"; exit 1; fi
