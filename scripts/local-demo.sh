#!/bin/sh
# Native local demo: fixtures -> seeder -> server with a 120M budget -> add 4 movies.
# Requires a built binary (devbox run build) and a .env with TORNAS_TMDB_TOKEN.
set -eu
cd "$(dirname "$0")/.."
BIN=${BIN:-target/debug/tornas}
DATA=${DATA:-/tmp/tornas-demo}
rm -rf "$DATA"; mkdir -p "$DATA"
[ -f fixtures/fixtures.json ] || "$BIN" fixtures --count 4 --seconds 20
"$BIN" seed --dir fixtures --listen 127.0.0.1:15100 > "$DATA/seed.log" 2>&1 &
SEED=$!
set -a; [ -f .env ] && . ./.env; set +a
TORNAS_DATA_DIR="$DATA/data" TORNAS_DISK_BUDGET=${TORNAS_DISK_BUDGET:-120M} TORNAS_MIN_FREE=100M TORNAS_STREAM_GRACE=30s TORNAS_SWEEP_INTERVAL=30s \
  "$BIN" server --disable-dht --disable-upnp-port-forward --http-listen 127.0.0.1:3030 > "$DATA/server.log" 2>&1 &
SRV=$!
trap 'kill $SEED $SRV 2>/dev/null' EXIT
sleep 2
python3 - <<'PY'
import json, urllib.request, time
fx = json.load(open("fixtures/fixtures.json"))
for f in fx:
    body = json.dumps({"imdb_id": f["imdb_id"], "magnet": f["magnet"], "initial_peers": ["127.0.0.1:15100"]}).encode()
    req = urllib.request.Request("http://127.0.0.1:3030/api/movies", body, {"Content-Type": "application/json"})
    try:
        print(f["imdb_id"], urllib.request.urlopen(req).status)
    except urllib.error.HTTPError as e:
        print(f["imdb_id"], e.code, e.read().decode())
    time.sleep(1)
PY
"$BIN" status
echo; echo "Stremio addon: http://127.0.0.1:3030/manifest.json   (server log: $DATA/server.log)"
echo "press ctrl-c to stop"
wait $SRV
