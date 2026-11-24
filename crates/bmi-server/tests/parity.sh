#!/usr/bin/env bash
# Parity gate: the Rust server must answer exactly like the TypeScript reference on the real catalog.
# Usage: PY=<python with Pillow> tests/parity.sh   (run from the app repo root)
set -euo pipefail
INDEX_REPO="${INDEX_REPO:-$HOME/Projects/balatro-mod-index}"
PY="${PY:-python3}"
T=$(mktemp -d); trap 'kill ${TS_PID:-} ${RS_PID:-} 2>/dev/null || true' EXIT
TOKEN=$(printf 'p%.0s' {1..32})

(cd "$INDEX_REPO" && "$PY" scripts/build_index.py --out "$T/cat" | tail -1)
cargo build -q --release -p bmi-server
mkdir -p "$T/ts" "$T/rs"
(cd "$INDEX_REPO/server" && DATA_DIR="$T/ts" CATALOG_DIR="$T/cat" PORT=8788 ADMIN_TOKEN=$TOKEN TRUST_PROXY=1 bun src/index.ts >"$T/ts.log" 2>&1) & TS_PID=$!
DATA_DIR="$T/rs" CATALOG_DIR="$T/cat" PORT=8789 ADMIN_TOKEN=$TOKEN TRUST_PROXY=1 target/release/bmi-server >"$T/rs.log" 2>&1 & RS_PID=$!
for p in 8788 8789; do for _ in $(seq 50); do curl -sf "localhost:$p/healthz" >/dev/null && break; sleep 0.2; done; done

python3 - "$T" <<'PY'
import json, sys, urllib.request, urllib.error, urllib.parse, hashlib, pathlib
T = pathlib.Path(sys.argv[1]); TS, RS = "http://localhost:8788", "http://localhost:8789"
fails = 0
def call(base, path, method="GET", body=None, headers=None):
    h = {"cf-connecting-ip": "9.9.9.9", **(headers or {})}
    req = urllib.request.Request(base + path, data=body, method=method, headers=h)
    try:
        with urllib.request.urlopen(req) as r: return r.status, r.read(), r.headers
    except urllib.error.HTTPError as e: return e.code, e.read(), e.headers
def norm(raw):
    try: return json.dumps(json.loads(raw), sort_keys=True)
    except Exception: return raw
def check(name, ok, detail=""):
    global fails
    print(("ok   " if ok else "FAIL ") + name + (("  " + detail) if detail and not ok else ""))
    fails += (not ok)

def drop_updated(v):
    if isinstance(v, dict): return {k: drop_updated(x) for k, x in v.items() if k != "updated_at"}
    if isinstance(v, list): return [drop_updated(x) for x in v]
    return v

for path in ["/mods?limit=500&sort=name_asc", "/mods?limit=500&sort=updated_desc", "/mods/changed?since=0&limit=500", "/mods?limit=200&cursor=200&sort=downloads_desc"]:
    a, b = call(TS, path), call(RS, path)
    ja, jb = json.loads(a[1]), json.loads(b[1])
    check(path + " status", a[0] == b[0] == 200)
    check(path + " order", [i["id"] for i in ja["items"]] == [i["id"] for i in jb["items"]])
    check(path + " body (ignoring first-seen updated_at)", drop_updated(ja) == drop_updated(jb))

ids = [i["id"] for i in json.loads(call(RS, "/mods?limit=500")[1])["items"]]
sample = ["JakeyBooBoo@!BooBooBalatro!"] + [ids[i] for i in (0, 100, 200, 400)]
for mid in sample:
    p = "/mods/" + urllib.parse.quote(mid, safe="")
    a, b = call(TS, p), call(RS, p)
    check(p, a[0] == b[0] == 200 and drop_updated(json.loads(a[1])) == drop_updated(json.loads(b[1])))
    d = [call(base, p + "/download", "POST") for base in (TS, RS)]
    check(p + "/download", d[0][0] == d[1][0] and norm(d[0][1]) == norm(d[1][1]))

cases = [
    ("GET", "/mods?cursor=abc", None, 400), ("GET", "/mods/changed", None, 400), ("GET", "/mods/nope", None, 404),
    ("POST", "/mods/nope/download", None, 404), ("GET", "/thumbnails/missing.webp", None, 404),
    ("GET", "/thumbnails/..%2Findex.json", None, 404), ("GET", "/thumbnails/x.png", None, 404),
    ("POST", "/report", b"x", 400), ("POST", "/report", b'{"description":"d"}', 400),
    ("POST", "/report", b"x" * 1_100_000, 413), ("GET", "/admin/stats", None, 401),
    ("GET", "/metrics", None, 401), ("GET", "/nothing", None, 404), ("GET", "/mods?cursor=-1", None, 400),
]
for i, (m, p, body, want) in enumerate(cases):
    h = {"cf-connecting-ip": f"7.7.7.{i}"}
    a, b = call(TS, p, m, body, h), call(RS, p, m, body, h)
    check(f"{m} {p[:40]} -> {want}", a[0] == b[0] == want, f"ts={a[0]} rs={b[0]}")

thumbs = sorted(p.name for p in (T / "cat" / "thumbnails").iterdir())[:20]
same = 0
for t in thumbs:
    a, b = call(TS, "/thumbnails/" + urllib.parse.quote(t)), call(RS, "/thumbnails/" + urllib.parse.quote(t))
    same += a[0] == b[0] == 200 and hashlib.sha256(a[1]).digest() == hashlib.sha256(b[1]).digest()
check(f"{len(thumbs)} thumbnails byte-identical", same == len(thumbs), f"{same}/{len(thumbs)}")

a, b = (json.loads(call(x, "/admin/stats", headers={"authorization": "Bearer " + "p" * 32})[1]) for x in (TS, RS))
check("admin stats totals", a["totalDownloads"] == b["totalDownloads"] and a["mods"] == b["mods"], f"{a['totalDownloads']} vs {b['totalDownloads']}")
print(f"\n{'PARITY OK' if not fails else str(fails) + ' DIFFERENCES'}")
sys.exit(1 if fails else 0)
PY
