# Maintaining this fork

This fork replaces the original author's (now offline) servers with a backend you own.

| Piece | Where | Notes |
|---|---|---|
| Catalog, thumbnails, download counts, reports | `crates/bmi-server` (Rust), running on the `kh-kube` MicroK8s node, public at **https://bmi.khatab.me** through a Cloudflare Tunnel | Cloudflare caches catalog reads at the edge. |
| Wire types and catalog logic | `crates/bmi-core` | No I/O; shared by the server and, later, the app and a dependency resolver. |
| Catalog data | the `Balatro-Mod-Index` fork (`mods/`, `scripts/build_index.py`) | The server rebuilds from it when git sync is on. |
| Usage analytics (optional) | Your own Umami instance | Off unless configured at build time. |

## Layout on kh-kube

Everything is in namespace `bmi` (never touch `default`, `guident-test`, `kube-*`): Deployment `bmi-server`
(1 replica, SQLite), Deployment `cloudflared` (2 connectors), CronJob `bmi-backup`, Secrets `bmi-env` (admin
token) and `cloudflared-token`. Data lives on a **fixed 8 GiB ext4 image** (`/srv/bmi.img`, mounted at `/srv/bmi`)
so a full volume can only hurt this service. Manifests, apply order, restore and rollback:
`crates/bmi-server/deploy/k8s/README.md`.

Cloudflare: tunnel `bmi-kh-kube`, proxied CNAME `bmi.khatab.me`, and one cache rule (host `bmi.khatab.me`, GET
`/mods*`, `/downloads`, `/thumbnails/*`, respect origin headers). The server sends `Cache-Control` and `ETag`
on those routes and `no-store` on everything else, so a hidden mod disappears within ~2 minutes.

## Deploying a new version

```sh
SHA=$(git rev-parse --short HEAD)
docker buildx build --platform linux/amd64 --load -f crates/bmi-server/Dockerfile -t bmi-server:$SHA .
bash crates/bmi-server/tests/image-smoke.sh bmi-server:$SHA <python-with-Pillow>
docker save bmi-server:$SHA | ssh kh@kh-kube 'sudo -n microk8s ctr image import -'
# set the tag in deploy/k8s/bmi-server.yaml and backup-cronjob.yaml, then:
ssh kh@kh-kube 'sudo -n microk8s kubectl apply -f -' < crates/bmi-server/deploy/k8s/bmi-server.yaml
```

Counters are flushed every 5 s and on SIGTERM, so a rollout loses nothing.

## Catalog updates

Without `INDEX_REPO_URL` the server serves the catalog already on its volume (seeded with
`build_index.py --out` and `tar`). With `INDEX_REPO_URL`/`INDEX_BRANCH` set it clones the index repo and
rebuilds every 15 minutes (`POST /admin/reload` forces it). A failed build never replaces the served catalog.

## Admin API

`Authorization: Bearer <ADMIN_TOKEN>` (the `bmi-env` secret): `GET /admin/stats`, `POST /admin/reload`,
`POST /admin/links/check`, `GET /admin/links`, `POST|DELETE /admin/mods/{id}/hide`, `GET /admin/reports`,
`GET /admin/reports/{id}/log`, `POST /admin/ban {"ip"}`, `GET /admin/whoami`, `GET /metrics`.

## Reports and privacy

The app redacts the log **before** sending (home directory -> `~`, account names in `/Users/<n>`, `/home/<n>`,
`C:\Users\<n>` -> `<user>`, e-mail addresses -> `<email>`, IPv4 addresses -> `<ip>`), shows exactly what will be
sent, and needs a tick-box. Redaction is intentionally over-eager: a mod id with a dot in its name
(`x@Mod.Pack`) and a four-part version number are also replaced. The server stores no IP (only a salted
hash for rate limiting). Reports live in their own SQLite file (`reports.sqlite`, gzip-compressed logs),
are readable only with the admin token, and are deleted after 30 days. Limits: 5 per IP per hour, 300 per UTC
day, 1 GiB total (the server answers 503 beyond either).

## Storage budget (inside the 8 GiB volume)

| What | Budget |
|---|---|
| counts DB (`bmi.sqlite`) | < 100 MB expected |
| reports DB | <= 1 GiB (hard stop) |
| catalog builds | 2 x ~50 MB |
| index clone | <= 1 GiB |
| nightly snapshots | 7 x counts DB |

`/metrics` exposes `bmi_db_bytes{db}` and `bmi_volume_free_bytes` / `bmi_volume_total_bytes`.

## Testing

```sh
cargo test --workspace -- --test-threads=1                # all crates
BMI_SERVER_URL=https://bmi.khatab.me cargo test -p balatro-mod-manager live_backend -- --ignored
bash crates/bmi-server/tests/parity.sh                     # historical: Rust vs the retired TS server
```
CI runs the Rust tests and clippy on Ubuntu, macOS and Windows, a per-OS contract test (app client vs
`bmi-server`), a packaged-app `--smoke-test`, and a nightly install canary. The per-release manual checklist
is `docs/release-checklist.md`.

## Analytics (optional)

Build with `BMM_UMAMI_URL=https://<your-umami>/api/send` and `BMM_UMAMI_WEBSITE_ID=<id>`. If either is
missing, the app sends nothing. The existing in-app opt-out toggle still applies.

## Known limits and gaps

- Users behind one NAT/CGNAT/WARP address count as one downloader per mod per day and share rate-limit
  buckets (reads: 120/min per IP).
- Download counts started at zero; one count per visitor, per mod, per UTC day.
- Backups: nightly snapshots stay on the same disk. Off-box copies are still to be set up (R2 chosen).
- Release workflows, signing, and publishing the fork's first release are not done; the README and install
  scripts point at `ahmmkh/balatro-mod-manager` releases that do not exist yet.
- `src-tauri/src/lfs.rs` still references the old author's Git LFS host (harmless, can be deleted);
  `scripts/aur-update.sh` still targets the original AUR package; the Ko-fi button still points at the
  original author.
