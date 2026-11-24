#!/usr/bin/env bash
# Build-independent smoke test of a bmi-server image. Usage: image-smoke.sh <image> [python-with-Pillow]
set -euo pipefail
IMG="${1:-bmi-server:dev}"; PY="${2:-python3}"
INDEX_REPO="${INDEX_REPO:-$HOME/Projects/balatro-mod-index}"
T=$(mktemp -d); chmod 777 "$T"; CID=""
trap '[ -n "$CID" ] && docker rm -f "$CID" >/dev/null 2>&1; rm -rf "$T"' EXIT
(cd "$INDEX_REPO" && "$PY" scripts/build_index.py --out "$T/catalog" | tail -1)
chmod -R a+rX "$T"
TOKEN=$(printf 's%.0s' {1..32})

[ "$(docker run --rm --entrypoint id "$IMG" -u)" = 10001 ] && echo "ok uid 10001"
docker run --rm --entrypoint python3 "$IMG" -c "import PIL; print('ok PIL', PIL.__version__)"
docker run --rm --entrypoint git "$IMG" --version >/dev/null && echo "ok git"
CID=$(docker run -d -p 18090:8080 -v "$T:/data" -e ADMIN_TOKEN="$TOKEN" "$IMG")
for _ in $(seq 50); do curl -sf localhost:18090/healthz >/dev/null && break; sleep 0.3; done
[ "$(curl -s localhost:18090/healthz)" = ok ] && echo "ok healthz"
[ "$(curl -s 'localhost:18090/mods?limit=1' | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["items"]))')" = 1 ] && echo "ok mods"
docker stop -t 10 "$CID" >/dev/null   # SIGTERM -> graceful shutdown, DB released
docker rm "$CID" >/dev/null; CID=""
OUT=$(docker run --rm -v "$T:/data" "$IMG" backup --keep 7)
echo "ok backup wrote $OUT"
SNAP=$(ls "$T"/backups/bmi-*.sqlite | head -1)
python3 - "$T/bmi.sqlite" "$SNAP" <<'PY'
import sqlite3, sys
live, snap = (sqlite3.connect(p) for p in sys.argv[1:3])
q = "select count(*) from mod_state"
assert live.execute(q).fetchone() == snap.execute(q).fetchone(), "mod_state mismatch"
assert not snap.execute("select 1 from sqlite_master where name='reports'").fetchall(), "snapshot must not contain reports"
print("ok snapshot matches live mod_state and has no reports table")
PY
SIZE=$(docker image inspect "$IMG" --format '{{.Size}}'); echo "image size: $((SIZE/1000000)) MB"
# git + python3 + Pillow (needed for in-cluster catalog sync) put the image near 340 MB.
if [ "$SIZE" -lt 400000000 ]; then echo "ok under 400 MB"; else echo "FAIL image too large"; exit 1; fi
