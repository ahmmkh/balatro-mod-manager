# BMI Rust Server and kh-kube Deployment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port the BMI backend to Rust, prove it behaves identically to the existing Bun/TypeScript server, then run it on the `kh-kube` MicroK8s node at `https://bmi.khatab.me` through a Cloudflare Tunnel, sized for **~50,000 users**, and point the Balatro Mod Manager at it.

**Architecture:** The code lives in the app repo's Cargo workspace as two crates: `bmi-core` (pure, synchronous library: wire types and catalog operations, no I/O) and `bmi-server` (a single-process axum service built on it). Keeping the logic in a library both the desktop app and the server can depend on is deliberate: compatibility checks and dependency resolution are planned next (see "Future direction") and must run offline in the app as well as on the server. The service holds the catalog in memory, counters in memory with write-behind to SQLite, and syncs the catalog from the index git repo by shelling out to `git` and `scripts/build_index.py`. The TypeScript server (`server/` on branch `feat/vps-server`) is the executable reference: it is not deleted until Task 7 shows byte-equivalent API output on the real catalog. Deployment is one Deployment (SQLite = one writer) plus two `cloudflared` connectors. At this scale the origin must not serve the catalog to every client: read endpoints send `Cache-Control` + `ETag` and a Cloudflare Cache Rule scoped to `bmi.khatab.me` lets the edge absorb them, and the app is changed to refresh download counts from a small `/downloads` map instead of re-paging the full catalog.

**Tech Stack:** Rust (edition 2024, Cargo workspace shared with the Tauri app), axum 0.8, tokio, rusqlite (bundled SQLite), serde/serde_json, reqwest (rustls) for link checks and Discord, sha2, subtle, tracing; Docker buildx (linux/amd64); MicroK8s via `sudo -n microk8s kubectl`; cloudflared (remotely-managed tunnel); Doppler CLI; Python 3 + Pillow (catalog build, kept in Python).

**Spec:** No standalone spec. The behavioural spec is the reference implementation `server/src/*.ts` and its 28 tests `server/test/*.test.ts` (index repo `/Users/kh/Projects/balatro-mod-index`, branch `feat/vps-server`, commits `36c23a50`..`89191468`), plus the client contract in `src-tauri/src/bmi.rs` and `docs/maintaining.md` of this repo.

## Global Constraints

- **Code location (app repo `/Users/kh/Projects/balatro-mod`, branch `feat/own-backend`):** new workspace members `crates/bmi-core` (library; dependencies limited to `serde` and `serde_json`; **no tokio, no rusqlite, no filesystem or network access**, so it can be used by the app and by future resolver code and could compile to WASM) and `crates/bmi-server` (library + binary `bmi-server`). Unknown mod metadata must flow through untouched: `CatalogMod.meta` stays an open `serde_json::Map` so new `meta.json` fields need no server change. The index repo (`/Users/kh/Projects/balatro-mod-index`) keeps only data, `scripts/build_index.py`, and — until Task 7 — the TypeScript reference `server/`, which is deleted there once parity is shown.
- **Wire contract is frozen** for everything the app already calls; `GET /downloads` is the one addition (below): `GET /healthz`; `GET /mods?limit&sort&cursor`; `GET /mods/changed?since&limit&sort&cursor`; `GET /mods/{id}`; `POST /mods/{id}/download` → `{"download_url"}`; `GET /thumbnails/{file}.webp`; `POST /report`; `GET /downloads` → `{ "<id>": {"total","today","week"}, … }` (counts only, no descriptions); `/admin/*` and `/metrics` with `Authorization: Bearer`. Sorts: `name_asc|name_desc|updated_asc|updated_desc|downloads_asc|downloads_desc`, default `name_asc`. Limit default 200, max 500. Cursor = decimal offset; malformed → 400. Each mod JSON carries `downloads: {total, today, week}`; `updated_at` is a string; tombstones are `{id, deleted: true, updated_at}` on the first page of `/mods/changed` only.
- **Behavioural constants (copy exactly):** one counted download per (visitor, mod, UTC day), key = SHA-256 of `secret|day|ip|mod`; counter flush every 5 s and on SIGTERM; rate limits per client IP: reads 120/min (thumbnails 600/min), download 60/min, admin 30/min, reports 5/hour burst 5; report body cap 1,000,000 bytes (413 beyond); link dead after 3 consecutive failures, https-only and non-private hosts, HEAD then ranged GET on 403/405/501, 10 s timeout, concurrency 6; `ADMIN_TOKEN` ≥ 24 chars else startup error; admin endpoints 404 when no token is configured; `/healthz` 503 until the catalog is loaded or when the DB is unreachable; long admin jobs (`/admin/reload`, `/admin/links/check`) return 202 and run in the background (409 if already running).
- **Scale target (design for, then verify in Task 7):** 50,000 active users; assume one launch per user per day with a 10× peak hour ⇒ ~1.4 launches/s peak (each = 1 health + 1 changed + 1 downloads map + thumbnails), 150,000 download POSTs/day (peak ≈ 50/s), first-run thumbnail fetch ≈ 10 MB per new user. Origin must sustain **300 req/s of `GET /mods?limit=200`** and **100 POST/s** with p99 < 100 ms on this node with the edge cache **bypassed**; with the cache on, origin traffic for `GET /mods*`, `/downloads`, `/thumbnails/*` should drop by > 90 %.
- **HTTP caching (server must send, edge must honour):** `GET /mods`, `/mods/changed`, `/mods/{id}`, `/downloads`: `Cache-Control: public, max-age=60, stale-while-revalidate=300` and a strong `ETag` (hash of the body) with `304` on `If-None-Match`; `GET /thumbnails/*`: `Cache-Control: public, max-age=86400`. Never cacheable (`Cache-Control: no-store`): `/healthz`, `/admin/*`, `/metrics`, `/report`, every non-GET. A hidden/removed mod must be gone from client-visible listings within 2 minutes (60 s TTL + revalidation).
- **Reports storage (disk safety):** reports live in their **own SQLite file** `$DATA_DIR/reports.sqlite` (`auto_vacuum=INCREMENTAL`), never in the counts database. The log is stored gzip-compressed (`log_gz BLOB`, plus `log_bytes` original size). Limits: 5 per IP per hour (burst 5, existing); global ceiling `REPORTS_PER_DAY` = 300 per UTC day; total file budget `REPORTS_MAX_BYTES` = 1 GiB (503 and a logged error beyond either limit); retention 30 days (daily prune, then `incremental_vacuum` and `wal_checkpoint(TRUNCATE)`). The counts database must stay small (< 100 MB expected) so backups are cheap.
- **Storage:** the cluster has **no StorageClass** and MicroK8s hostpath volumes are unquota'd directories on the shared root disk, so they are not used. Data lives on a **fixed-size 8 GiB ext4 image** (`/srv/bmi.img`) loop-mounted at `/srv/bmi` (fstab line with `loop,nofail,noatime`) and exposed as a static local PersistentVolume `bmi-data` (`storageClassName: bmi-local`, `persistentVolumeReclaimPolicy: Retain`, `nodeAffinity` on the node's hostname). A full volume must only break this service, never the host. Budget inside the volume: counts DB ≤ 100 MB, reports ≤ 1 GiB, catalog builds ≤ 2 × 50 MB, index clone ≤ 1 GiB, nightly snapshots ≤ 7 × counts DB. The service exposes `bmi_db_bytes{db="main"|"reports"}` and `bmi_volume_free_bytes` in `/metrics`; a Prometheus alert fires at 70 % used.
- **Report privacy:** the app redacts the log **before** it leaves the machine (home directory → `~`, account names in `/Users/<name>`, `/home/<name>`, `C:\Users\<name>` → `<user>`, e-mail addresses → `<email>`, IPv4 addresses → `<ip>`), shows the user exactly what will be sent, and requires an explicit tick-box before submitting. The server stores no IP (only a salted hash for rate limiting), reports are readable only with the admin token, and logs are deleted after 30 days.
- **Outage behaviour (client):** if the backend is unreachable the app must show the last cached catalog instead of an error.
- **Hostname:** `https://bmi.khatab.me` (zone `khatab.me`, Cloudflare account prefix `0823c0…`), effectively permanent.
- **Kubernetes and host:** touch only namespace `bmi`, via `ssh kh@kh-kube 'sudo -n microk8s kubectl …'`. Never modify `default`, `guident-test`, `kube-*`. The only host changes allowed are those in Task 9a (create `/srv/bmi.img`, create and mount `/srv/bmi`, add one line to `/etc/fstab`), and only after the user approves them.
- **Cloudflare:** never modify or delete pre-existing tunnels, DNS records or tokens. Create only tunnel `bmi-kh-kube`, the one CNAME `bmi.khatab.me`, and **append** one Cache Rule whose expression is limited to `http.host eq "bmi.khatab.me"` (never replace the zone's existing cache ruleset; list it first and POST to its `/rules`).
- **Credentials:** minting token = Doppler `shamel-operator/prd` → `CLOUDFLARE_MINTING_TOKEN` (user-approved for this use only). Mint **one** work token expiring ≤ 2 h with only `Cloudflare Tunnel Write` (account), `DNS Write` (zone `khatab.me`) and the zone-scoped cache-rules write permission (look up its exact name in `/user/tokens/permission_groups`; stop and ask if no zone-scoped cache permission exists); delete it as soon as Task 11 ends. Never print, log or commit any token; secrets go to the cluster via stdin to `kubectl create secret`, never into repo files.
- **Cross-platform bar (Track B):** the app's Rust and frontend tests must pass on `ubuntu-22.04`, `macos-latest` (Apple Silicon) and `windows-latest`. A failing test is fixed, not `#[ignore]`d or `cfg`-gated away; if one truly cannot run on an OS, the commit message states why and `docs/release-checklist.md` lists it as a manual check. Assumed supported platforms: Windows, macOS, Linux including Steam Deck/Proton; Intel Macs are covered only by the universal build, not by a test runner.
- **Archive safety must behave identically on every OS** (so a mod never installs on Linux and silently breaks on Windows): extraction rejects path escapes (`../`, absolute paths, drive letters), entry names illegal on Windows (`<>:"/\|?*`, reserved names `CON PRN AUX NUL COM1-9 LPT1-9`, trailing dot or space), case-insensitive name collisions, and total paths longer than 240 characters; a rejected or failed extraction leaves no partial folder behind. (Flag to the user at plan review: this is stricter than today on Linux/macOS.)
- **The canary and any test tooling must never call `POST /mods/{id}/download`** (it increments the public download counters); they read `meta.downloadURL` from the catalog instead.
- **Image:** `linux/amd64`; Deployment `strategy: Recreate`, `replicas: 1`.
- **No `git push`, PR, or enabling GitHub Actions without explicit user approval** (Task 14 is gated).
- `DEFAULT_BMI_SERVER_URL` stays `https://bmi-api.invalid` until Task 12 passes.

## Review Focus

- **Blocking SQLite and lock contention under load:** a `rusqlite` call or a held `Mutex` inside a handler stalls the executor at 300 req/s. Counter updates, report inserts and flushes must keep critical sections short and never span `.await`. Owned by Tasks 3 and 4 (tests: 200 concurrent downloads exact; a flush running while reads continue).
- **Mod ids with `@`, `!`, spaces, `%`, and path traversal:** decode once, never allow `..` or `/`. Owned by Task 4.
- **Edge cache correctness:** a Cache Rule must never cache `/admin/*`, `/metrics`, `/report`, `/healthz`, any `POST`, or a `4xx/5xx`; a hidden or removed mod must disappear within 2 minutes; `304` must work with the real ETag. Owned by Tasks 4 and 11 (tests: header assertions per route; live check with `curl -sI` for `cf-cache-status`).
- **Backend outage, disk fill and report privacy:** with the origin down the app must keep showing its cached catalog (Task 12a); a flood of reports must hit the per-day ceiling or the byte budget (503) rather than fill the volume, and even a full volume must not touch the host (Task 9a proves ENOSPC inside the mount while `/` is unaffected); a report must never contain the account name or home path (Task 12b tests, including a path straddling the truncation boundary). Owned by Tasks 4, 9a, 12a and 12b.
- **Counts lost on shutdown/crash and UTC rollover:** SIGTERM flushes within the 30 s grace period, a failed flush keeps pending counts, 23:59:59→00:00:00 resets `today` and the dedupe set. Owned by Task 3 (injected clock) and Task 10.
- **Behaviour that only exists on one OS and is never executed by today's CI:** unix-only `symlink` in the server's catalog swap, Windows path-length/reserved-name/case-collision rules, Steam library layouts per OS, `localhost` resolving to IPv6 on Windows runners. Owned by Tasks 6, 15, 16, 17 and 18 (the contract test uses `127.0.0.1`).
- **Known accepted limitation:** users behind one NAT/CGNAT/WARP IP count as one downloader per mod per day and share rate-limit buckets (120 reads/min is sized for a ~100-person NAT launching simultaneously). Documented in `docs/maintaining.md`, not engineered around.

---

### Task 1: Crate skeleton and pure catalog logic

**Files:**
- Modify: `Cargo.toml` (root workspace `members` += `crates/bmi-core`, `crates/bmi-server`)
- Create: `crates/bmi-core/Cargo.toml`, `crates/bmi-core/src/lib.rs`, `crates/bmi-core/src/catalog.rs`; `crates/bmi-server/Cargo.toml`, `crates/bmi-server/src/lib.rs`, `crates/bmi-server/src/main.rs` (prints version and exits for now)
- Test: `crates/bmi-core/src/catalog.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces (`bmi-core::catalog`, re-exported from `bmi-server`): `struct CatalogMod { id: String, name: String, updated_at: String, description: String, meta: serde_json::Map<String, Value>, thumbnail_url: Option<String>, unavailable: bool }` (serde: skip `thumbnail_url` when `None`, skip `unavailable` unless true); `struct Downloads { total: u64, today: u64, week: u64 }` (`Default` = zeros); `enum SortMode`; `fn parse_sort(&str) -> SortMode`; `fn parse_limit(Option<&str>) -> usize`; `fn parse_cursor(Option<&str>) -> Option<usize>`; `fn sort_mods(&[CatalogMod], SortMode, &HashMap<String, Downloads>) -> Vec<CatalogMod>`; `fn changed_since(&[CatalogMod], &str) -> Vec<CatalogMod>`; `struct Page<T> { items: Vec<T>, next_cursor: Option<String> }`; `fn paginate<T: Clone>(&[T], usize, usize) -> Page<T>`.

- [ ] **Step 1: Write the failing tests** by porting `server/test/catalog.test.ts` one-to-one: name sort is case-insensitive (`Apple` before `banana`) with id tiebreak, `updated_*` and `downloads_*` sorts treat missing counts as 0 and tie-break by name, `sort_mods` does not mutate input, `paginate` returns `next_cursor` only while more items remain (`Some("2")` then `None`), `parse_cursor`: `None→Some(0)`, `"12"→Some(12)`, `"-1"`/`"abc"`/10-digit → `None`, `parse_limit("9999")==500`, `"0"→200`, `None→200`, `changed_since(.., "20")` is strictly greater-than, non-numeric `since` returns everything.
- [ ] **Step 2: Run** `cargo test -p bmi-core` → FAIL (not implemented). Also assert the isolation rule: `cargo tree -p bmi-core --edges normal` lists only `serde`, `serde_json` and their transitive deps (no `tokio`, `rusqlite`, `reqwest`).
- [ ] **Step 3: Implement** the items listed under Interfaces; comparison for names via `str::to_lowercase` then id.
- [ ] **Step 4: Run** `cargo test -p bmi-core -p bmi-server && cargo clippy -p bmi-core -p bmi-server --all-targets -- -D warnings`, and confirm `cargo check -p balatro-mod-manager` still passes (the Tauri app is unaffected) → all pass, 0 warnings.
- [ ] **Step 5: Commit** `feat(bmi-core, bmi-server): workspace crates and catalog logic`.

### Task 2: Store with lifecycle state and tombstones

**Files:**
- Create: `crates/bmi-server/src/db.rs`, `crates/bmi-server/src/store.rs`
- Test: `crates/bmi-server/tests/store.rs`

**Interfaces:**
- Consumes: Task 1 types.
- Produces: `db::open(path: &Path) -> rusqlite::Result<Connection>` applying the schema from `server/src/db.ts` verbatim (tables `meta, mod_state, dl_total, dl_daily, dl_seen, link_health, banned` — **no `reports` table**, WAL, `busy_timeout 5000`); `db::open_reports(path: &Path) -> rusqlite::Result<Connection>` creating `reports(id INTEGER PRIMARY KEY AUTOINCREMENT, created_at INTEGER, title, description, mm_version, os, arch, cpu, gpu, ram, log_filename, log_gz BLOB, log_bytes INTEGER)` with `PRAGMA auto_vacuum=INCREMENTAL` set before the first table is created, WAL, `busy_timeout 5000`; `db::secret(&Connection) -> String` (persisted random salt); `type Db = Arc<Mutex<Connection>>`; `Store::new(Db) -> Store`; `Store::load(&mut self, dir: &Path, now: i64) -> Result<LoadStats>` where `LoadStats { total, added, removed, reappeared }`; `Store::{mods() -> Arc<Vec<CatalogMod>>, get(&str) -> Option<CatalogMod>, ready() -> bool, dir() -> Option<PathBuf>, tombstones_since(&str) -> Vec<Tombstone>, set_hidden(&str, bool, i64) -> bool, mark_unavailable(&HashSet<String>)}`; `Tombstone { id, deleted: true, updated_at: String }`.

- [ ] **Step 1: Write failing tests** porting the "catalog lifecycle" group: not ready before load; removing a mod yields exactly `[{id:"b@Beta", deleted:true, updated_at:"2000"}]` for `since=1500` and excludes it from `mods()`; the mod returning gets `reappeared == 1` and a new `updated_at`; first-seen mods get `updated_at = now` even if the index stamp is old (`5`) so `since=4000` returns them; `set_hidden` hides/unhides and creates/clears the tombstone; loading an `index.json` with an empty `mods` array is an error and keeps the previous catalog; `db::open` produces a database whose `sqlite_master` has no `reports` table, and `db::open_reports` produces a separate file with the table and `PRAGMA auto_vacuum` equal to `2` (incremental).
- [ ] **Step 2: Run** `cargo test -p bmi-server --test store` → FAIL.
- [ ] **Step 3: Implement** with a single DB transaction per `load`; the mods list is swapped atomically (`Arc` replace) so readers never see a half-loaded catalog.
- [ ] **Step 4: Run** `cargo test -p bmi-core -p bmi-server && cargo clippy -p bmi-core -p bmi-server --all-targets -- -D warnings` → pass.
- [ ] **Step 5: Commit** `feat(bmi-server): store, schema, tombstones`.

### Task 3: Download counter and rate limiter

**Files:**
- Create: `crates/bmi-server/src/downloads.rs`, `crates/bmi-server/src/ratelimit.rs`
- Test: `crates/bmi-server/tests/downloads.rs`, unit tests in `ratelimit.rs`

**Interfaces:**
- Consumes: `Db` from Task 2.
- Produces: `Downloads::new(db: Db, secret: String, today: Arc<dyn Fn() -> String + Send + Sync>) -> DownloadCounter`; `DownloadCounter::{record(&self, mod_id: &str, ip: &str) -> bool, snapshot(&self) -> Arc<HashMap<String, Downloads>>, get(&self, &str) -> Downloads, flush(&self) -> Result<usize>, hash(&self, parts: &[&str]) -> String}`; `RateLimiter::new(now: fn() -> Instant-like closure)`, `RateLimiter::allow(&self, cls: &str, client: &str, per_minute: f64, burst: f64) -> bool`, `RateLimiter::sweep(&self)`.

- [ ] **Step 1: Write failing tests** porting the "downloads" and "rate limiter" groups: three POSTs from one visitor count once, a second visitor counts, a different mod counts; after the injected day changes from `2026-11-15` to `2026-11-16` the same visitor counts again, `today` resets to 1 while `week` and `total` keep history (`{total:3, today:1, week:3}`); after `flush()` a freshly constructed counter on the same DB reports the same totals and `record` for an already-seen (visitor, mod, day) returns false; a flush that fails (DB made read-only) keeps pending counts and succeeds on retry; 200 concurrent `record` calls from distinct IPs via `tokio::spawn` yield exactly 200; limiter: 60 allowed then denied, one token refills after 1 s at 60/min, report bucket (`5/60` per minute, burst 5) allows exactly 5.
- [ ] **Step 2: Run** `cargo test -p bmi-server downloads ratelimit` → FAIL.
- [ ] **Step 3: Implement.** In-memory maps behind `parking_lot::Mutex` (held only for map updates, never across `.await` or SQLite calls); `flush` snapshots pending state, then writes in one transaction under the `Db` lock; on error, merge the batch back.
- [ ] **Step 4: Run** `cargo test -p bmi-core -p bmi-server && cargo clippy -p bmi-core -p bmi-server --all-targets -- -D warnings` → pass.
- [ ] **Step 5: Commit** `feat(bmi-server): download counter and rate limiter`.

### Task 4: Public HTTP API, thumbnails and reports

**Files:**
- Create: `crates/bmi-server/src/config.rs`, `crates/bmi-server/src/app.rs`, `crates/bmi-server/src/routes_public.rs`
- Test: `crates/bmi-server/tests/public_api.rs` (uses `tower::ServiceExt::oneshot`, no sockets)

**Interfaces:**
- Consumes: Tasks 1–3.
- Produces (additions for scale): `GET /downloads` handler returning the full counts map; `fn etag_for(body: &[u8]) -> String` (strong, hex of SHA-256 truncated to 16 bytes) and a response helper `cacheable(body, max_age)` that sets `Cache-Control`, `ETag`, returns `304` on a matching `If-None-Match`; `no-store` on every non-cacheable route; report handling against the separate reports database: global per-day ceiling (`Config.reports_per_day`, default 300) and total file budget (`Config.reports_max_bytes`, default 1 GiB, measured as the on-disk size of `reports.sqlite` plus its WAL), the log gzip-compressed with `flate2` before storage; `tower_http::compression::CompressionLayer` on all routes.
- Produces: `Config::from_env(&HashMap<String,String>) -> Result<Config>` (fields and defaults exactly as `server/src/config.ts`); `AppState { cfg: Arc<Config>, db: Db, reports: Db, store: Arc<RwLock<Store>>, downloads: Arc<DownloadCounter>, limiter: Arc<RateLimiter>, sync: Option<Arc<IndexSync>>, counters: Arc<Counters>, http: reqwest::Client }`; `pub fn router(state: AppState) -> axum::Router`; client IP resolution `fn client_ip(&Config, &HeaderMap, Option<SocketAddr>) -> String` (when `trust_proxy`: `cf-connecting-ip`, then first `x-forwarded-for`; otherwise the socket address, else `"unknown"`).

- [ ] **Step 1: Write failing tests** porting every non-admin case from `server/test/server.test.ts`: healthz 503→200 after load; paging walks 5 mods with `limit=2`, `cursor=abc`→400, `/mods/changed` without `since`→400; sort `downloads_desc` order from live counts; `POST /mods/a%40Alpha/download` returns `{"download_url":"https://example.com/a%40Alpha.zip"}` and counts once; `/mods/nope/download` and a mod without URL → 404; `/mods/c%40Gamma%21` resolves `c@Gamma!`; thumbnails: 200 with `content-type: image/webp` and `etag`, 304 on `If-None-Match`, 404 for missing, for `..%2Findex.json`, and for `.png`; reports: 201 for valid, 400 for invalid JSON and missing title, 413 for a 1,100,000-byte body **without** a `Content-Length` header, 6th report from one IP → 429; `trust_proxy=false` ignores a spoofed `cf-connecting-ip` (130 requests with distinct spoofed IPs still hit 429); tombstones appear on page 1 of `/mods/changed` only; `/downloads` returns `{"a@Alpha":{"total":2,"today":2,"week":2}}` after two counted downloads and is under 40 KB for the real 448-mod catalog; cache headers: `/mods`, `/mods/changed`, `/mods/{id}`, `/downloads` have `max-age=60` and an `ETag`, a repeat request with that ETag gets `304` with an empty body, `/thumbnails/*` has `max-age=86400`, and `/healthz`, `/admin/stats`, `/metrics`, `POST /report`, `POST …/download` carry `Cache-Control: no-store`; with `reports_per_day = 3` the 4th stored report of the day gets `503` while the first three succeed and the next UTC day resets it; with `reports_max_bytes = 4096` and incompressible 3 KB logs, reports stop with `503` once the file exceeds the budget; the stored `log_gz` decompresses byte-for-byte to the submitted log and `log_bytes` equals its original length; a report with a 900 KB repetitive log occupies under 20 KB on disk; none of these reports is written to the counts database.
- [ ] **Step 2: Run** `cargo test -p bmi-server --test public_api` → FAIL.
- [ ] **Step 3: Implement** handlers with axum extractors (`Path<String>` decodes once; reject file names not matching `^[^/\\]+\.webp$` or containing `..`); read bodies with `axum::body::to_bytes(body, 1_000_000)` mapping the limit error to 413; thumbnails stream with `tokio::fs::File` + `ReaderStream`; ETag = `W/"{size}-{mtime_ms}"`; report insert and Discord forward (fire-and-forget `tokio::spawn`, `allowed_mentions.parse = []`, content ≤ 1900 chars).
- [ ] **Step 4: Run** `cargo test -p bmi-core -p bmi-server && cargo clippy -p bmi-core -p bmi-server --all-targets -- -D warnings` → pass.
- [ ] **Step 5: Commit** `feat(bmi-server): public API, thumbnails, reports`.

### Task 5: Admin API, bans and metrics

**Files:**
- Create: `crates/bmi-server/src/routes_admin.rs`
- Modify: `crates/bmi-server/src/app.rs` (mount routes and the ban middleware)
- Test: `crates/bmi-server/tests/admin_api.rs`

**Interfaces:**
- Consumes: Task 4 `AppState`/`router`.
- Produces: routes `GET /admin/stats`, `POST /admin/reload`, `POST /admin/links/check`, `GET /admin/links`, `POST|DELETE /admin/mods/{id}/hide`, `GET /admin/reports`, `GET /admin/reports/{id}/log`, `POST /admin/ban` (`{"ip"}`), `GET /admin/whoami` (`{"ip"}`), `GET /metrics` (Prometheus text with `bmi_mods`, `bmi_downloads_total`, `bmi_dead_links`, `bmi_catalog_loaded_timestamp`, `bmi_sync_ok`, `bmi_requests_total{kind=…}`, `bmi_db_bytes{db="main"}`, `bmi_db_bytes{db="reports"}`, `bmi_volume_free_bytes`, `bmi_volume_total_bytes` — the last two from `statvfs` on `DATA_DIR`, via the `rustix` crate); `GET /admin/reports/{id}/log` returns the decompressed log as `text/plain`; banned clients get 403 on everything except `/healthz`.

- [ ] **Step 1: Write failing tests** porting "admin & abuse", "admin jobs" and the hide/unhide case: no token configured → 404 on `/admin/stats` and `/metrics`; wrong or missing bearer → 401; right bearer → 200 with `mods == 1`; constant-time comparison (use `subtle::ConstantTimeEq`); ban → that IP gets 403 on `/mods`, 200 on `/healthz`, other IPs unaffected; `POST /admin/links/check` → 202 then 409 while the first job is held open by an injected slow fetcher, `/admin/reload` → 400 when no repo is configured; `/admin/whoami` returns the proxy-resolved IP under `trust_proxy=true` and `"unknown"` otherwise; `/metrics` contains positive `bmi_db_bytes{db="main"}` and `bmi_db_bytes{db="reports"}` and a `bmi_volume_free_bytes` that is ≤ `bmi_volume_total_bytes`; `/admin/reports/{id}/log` returns the original text of a stored report and 404 for an unknown id.
- [ ] **Step 2: Run** `cargo test -p bmi-server --test admin_api` → FAIL.
- [ ] **Step 3: Implement** handlers; the link-check and reload handlers spawn tasks guarded by an `AtomicBool`; the HTTP client used for link probes is injected through `AppState` so tests can supply a stub.
- [ ] **Step 4: Run** `cargo test -p bmi-core -p bmi-server && cargo clippy -p bmi-core -p bmi-server --all-targets -- -D warnings` → pass.
- [ ] **Step 5: Commit** `feat(bmi-server): admin API, bans, metrics`.

### Task 6: Link health, git sync and process wiring

**Files:**
- Create: `crates/bmi-server/src/links.rs`, `crates/bmi-server/src/sync.rs`
- Modify: `crates/bmi-server/src/main.rs` (real startup), `crates/bmi-server/src/lib.rs`
- Test: `crates/bmi-server/tests/links.rs`, `crates/bmi-server/tests/sync.rs`

**Interfaces:**
- Consumes: Tasks 2, 4, 5.
- Produces: `sync::current_dir(catalog_dir: &Path) -> Option<PathBuf>` resolving the active build (a `current` symlink on unix; on non-unix a `current.txt` pointer file holding the build directory name, written to a temp file then renamed), so `bmi-server` **compiles and its tests pass on Windows and macOS** (no unguarded `std::os::unix`);
- Produces: `links::is_safe_url(&str) -> bool`; `links::probe(&dyn Fetch, &str) -> u16` (0 on error) with `trait Fetch { async fn head(..); async fn ranged_get(..); }` so tests stub it; `links::check_all(&Db, &mut Store, &dyn Fetch, concurrency: usize) -> LinkSummary { checked, dead }`; `IndexSync::new(Config, Arc<RwLock<Store>>) -> IndexSync`, `IndexSync::{load_from_disk() -> Option<LoadStats>, sync_once(force: bool) -> SyncOutcome, status() -> SyncStatus}`; `main` loads from disk, binds `PORT`, runs `flush` every 5 s, limiter sweep every 60 s, daily prune (`reports` older than 30 days followed by `PRAGMA incremental_vacuum` and `wal_checkpoint(TRUNCATE)` on the reports database; `dl_seen` before today; `dl_daily` older than 35 days), sync loop every `SYNC_INTERVAL_SEC` when `INDEX_REPO_URL` is set, first link check after 120 s then every `LINK_CHECK_INTERVAL_SEC`, graceful shutdown on SIGTERM/SIGINT that flushes counters before exit.

- [ ] **Step 1: Write failing tests**: `is_safe_url` accepts `https://github.com/x/y.zip` and rejects `http://…`, `localhost`, `127.0.0.1`, `10.0.0.5`, `192.168.1.1`, `169.254.169.254`, `172.16.0.1`, `svc.internal`, `[::1]`, `file:///etc/passwd`, `nope`; `probe` falls back to ranged GET on 405 and returns 0 when the fetcher errors; `check_all` flags a mod only on the **third** consecutive failure, clears it after one success, and does not flag a healthy neighbour; ``current_dir` returns the newest build on the current OS after a swap (runs on every CI OS); `sync_once` against a local bare git repo fixture clones, runs a stub build script (`--out <dir>` writes an `index.json`), swaps `current` atomically, loads it, writes `HEAD`, and prunes to 2 `build-*` dirs after 3 runs; a second call with no upstream change returns `changed=false` without rebuilding; a build script that exits non-zero leaves the previous catalog served and sets `lastError`; a sigterm-style shutdown calls `flush`; the daily prune deletes a 31-day-old report and keeps a 29-day-old one, and the reports file shrinks after the prune (size before > size after) when enough rows were removed.
- [ ] **Step 2: Run** `cargo test -p bmi-server --test links --test sync` → FAIL.
- [ ] **Step 3: Implement** (git and python via `tokio::process::Command`, errors carry the last 500 bytes of stderr; symlink swap by `symlink` to `current.tmp` then `rename`).
- [ ] **Step 4: Run** `cargo test -p bmi-core -p bmi-server && cargo clippy -p bmi-core -p bmi-server --all-targets -- -D warnings` → pass; start the binary against the real catalog (`CATALOG_DIR=<built dir> PORT=8788 ./target/release/bmi-server`) and `curl localhost:8788/healthz` → `ok`.
- [ ] **Step 5: Commit** `feat(bmi-server): link health, git sync, main wiring`.

### Task 7: Parity gate and retire the TypeScript server

**Files:**
- Create: `crates/bmi-server/tests/parity.sh`
- Delete in the index repo (only after the gate passes): `/Users/kh/Projects/balatro-mod-index/server/` (TypeScript reference)

**Interfaces:**
- Consumes: Task 6 binary; the TS server on the same real catalog.
- Produces: evidence that the contract is unchanged.

- [ ] **Step 1: Write `parity.sh`.** Build the real catalog (`python3 scripts/build_index.py --out $TMP/cat`), start the TS server on 8788 and the Rust server on 8789 (each with its own empty `DATA_DIR`, same `CATALOG_DIR`, `ADMIN_TOKEN` set, `TRUST_PROXY=1`), then diff normalized JSON (sorted keys; ignore `updated_at` of first-seen mods; ignore the additions the TS reference lacks: `GET /downloads`, `Cache-Control`/`ETag` headers, `no-store` on non-cacheable routes) for: `/mods?limit=500&sort=name_asc`, `…sort=updated_desc`, `/mods/changed?since=0&limit=500`, `/mods/{id}` for 5 ids including `JakeyBooBoo@!BooBooBalatro!`, status codes for the 14 error cases in the Task 4 list, and byte-compare 20 thumbnails.
- [ ] **Step 2: Run it.** Expected: every diff empty; any difference is a bug in the Rust server (fix and rerun) unless the TS output is the bug, in which case record it in the commit message.
- [ ] **Step 3: Run the app's contract test against the Rust binary:** `BMI_SERVER_URL=http://localhost:8789 cargo test -p balatro-mod-manager live_backend -- --ignored` (app repo) → PASS.
- [ ] **Step 4: Load check at the scale target** (install `oha` via `brew install oha` if missing; origin-direct, cache bypassed): 60 s at 300 req/s of `GET /mods?limit=200&sort=downloads_desc` with distinct `CF-Connecting-IP` values (limits raised via env for the test only) and, concurrently, 100 POST/s of `…/download` over 500 distinct IPs × 50 mods ⇒ zero 5xx, p99 < 100 ms for reads, RSS < 200 MB, and `/admin/stats` `totalDownloads` equals the exact number of distinct (ip, mod) pairs sent. Record req/s, p50/p99 and RSS in the commit message. Run the same on the TS server first for the baseline; expected: Rust ≥ 3× the TS throughput, but only the absolute targets above gate the task.
- [ ] **Step 5: Retire TS:** in the index repo, `git rm -r server` and commit `refactor: remove TypeScript reference server (Rust server in the app repo reached parity)`, quoting the parity run's summary lines in the body. In the app repo, commit `test(bmi-server): parity script`.

### Task 8: Container image

**Files:**
- Create: `crates/bmi-server/Dockerfile`, `.dockerignore` additions at the repo root (exclude `node_modules`, `build`, `target`, `src`, `.git`), `crates/bmi-server/tests/image-smoke.sh`

**Interfaces:**
- Produces: image `bmi-server:<git-sha>` (linux/amd64): multi-stage, build context = app repo root (workspace `Cargo.lock`), `rust:1.9x-slim-bookworm` building only `cargo build --release --locked -p bmi-server` (never the Tauri crate), runtime `debian:bookworm-slim` + `git python3 python3-pil ca-certificates`), runs as uid `10001`, `ENTRYPOINT ["/usr/local/bin/bmi-server"]`, `EXPOSE 8080`, writable `/data`; subcommand `bmi-server backup --keep 7` writes `VACUUM INTO` snapshots of the **counts database only** (never `reports.sqlite`) to `$DATA_DIR/backups/bmi-YYYY-MM-DD.sqlite`.

- [ ] **Step 1: Write the failing smoke script:** build image, run with a mounted built catalog and a 32-char `ADMIN_TOKEN`, assert `/healthz` → `ok`, `/mods?limit=1` has one item, `id -u` is `10001`, `python3 -c "import PIL"` works, and `docker run … backup` produces a snapshot whose `mod_state` row count matches the live DB and which contains no `reports` table.
- [ ] **Step 2: Run** → FAIL (no Dockerfile / no `backup` subcommand).
- [ ] **Step 3: Implement** the `backup` subcommand in `main.rs` (`fn backup(db: &Connection, dir: &Path, keep: usize) -> Result<PathBuf>`, with a unit test using a temp dir) and the Dockerfile (dependency-layer caching via a dummy `main.rs` build).
- [ ] **Step 4: Run** `docker buildx build --platform linux/amd64 --load -f crates/bmi-server/Dockerfile -t bmi-server:dev . && bash crates/bmi-server/tests/image-smoke.sh` → PASS; image size under 250 MB.
- [ ] **Step 5: Commit** `feat(server): container image and backup subcommand`.

### Task 9: Kubernetes manifests

**Files:**
- Create: `crates/bmi-server/deploy/k8s/{namespace.yaml,local-pv.yaml,bmi-server.yaml,cloudflared.yaml,backup-cronjob.yaml,README.md}`

**Interfaces:**
- Consumes: Task 8 image.
- Produces: Service `bmi-server.bmi.svc.cluster.local:8080`; Secrets `bmi-env` (`ADMIN_TOKEN`) and `cloudflared-token` (`TUNNEL_TOKEN`), created out-of-band; static PV `bmi-data` (8Gi, `local: {path: /srv/bmi}`, `storageClassName: bmi-local`, `Retain`, node affinity) plus PVC `bmi-data` in `bmi` bound to it, mounted at `/data`; CronJob `bmi-backup` (`23 2 * * *`, `concurrencyPolicy: Forbid`).

- [ ] **Step 1: Failing check:** `ssh kh@kh-kube 'sudo -n microk8s kubectl apply --dry-run=server -f -' < <file>` per file → FAIL before the files exist.
- [ ] **Step 2: Write the manifests** (`local-pv.yaml` holds the PV and the PVC; do not reference any StorageClass object, none exists). Deployment: `Recreate`, 1 replica, `terminationGracePeriodSeconds: 30`, non-root uid 10001, `readOnlyRootFilesystem` with `emptyDir` at `/tmp`, drop ALL caps, resources `requests 50m/64Mi, limits 1/1Gi` (Pillow build spike), env `PORT=8080 DATA_DIR=/data TRUST_PROXY=1 REPORTS_PER_DAY=300 REPORTS_MAX_BYTES=1073741824`, `ADMIN_TOKEN` from `bmi-env`, `startupProbe` `/healthz` `failureThreshold 60 × 5 s`, `readinessProbe` `/healthz`, `livenessProbe` TCP 8080 (never `/healthz`). `cloudflared`: **2 replicas** (the tunnel supports multiple connectors; survives one pod restarting), pinned image tag (verify it exists and is ≥ 2 weeks old), `tunnel --no-autoupdate run`, `TUNNEL_TOKEN` from secret, same hardening, liveness on `/ready`. README: apply order, secret-creation commands, restore procedure, rollback.
- [ ] **Step 3: Run the server-side dry-runs** → each prints `created (server dry run)`; no real resources created.
- [ ] **Step 4: Commit** `feat(server): k8s manifests for kh-kube`.

### Task 9a: Fixed-size data volume on kh-kube (GATED: edits host files)

**Files:** none in the repo; record the exact commands run and their output in `crates/bmi-server/deploy/k8s/README.md`.

**Interfaces:**
- Produces: `/srv/bmi` — an 8 GiB ext4 filesystem (loop-mounted from `/srv/bmi.img`, mounted at boot), owned by uid/gid `10001`, whose capacity is a hard limit independent of `/`.

- [ ] **Step 1: Ask the user to approve the host change** (one 8 GiB file, one directory, one `/etc/fstab` line; reversible by `umount`, deleting the line and the file). **Stop here unless approved.**
- [ ] **Step 2: Pre-checks (read-only):** `df -h /` shows ≥ 20 GiB free; `/srv/bmi.img` and `/srv/bmi` do not exist; `grep -c bmi /etc/fstab` is 0; back up `/etc/fstab` to `/etc/fstab.bak.bmi`.
- [ ] **Step 3: Create and mount:** `sudo fallocate -l 8G /srv/bmi.img && sudo mkfs.ext4 -q -L bmi /srv/bmi.img && sudo mkdir -p /srv/bmi`; append `/srv/bmi.img /srv/bmi ext4 loop,nofail,noatime 0 0` to `/etc/fstab`; `sudo mount /srv/bmi`; `sudo chown 10001:10001 /srv/bmi`. Verify `findmnt --verify` reports no errors for the new line, `df -h /srv/bmi` shows ≈ 7.8G size, and `mount -a` is a no-op.
- [ ] **Step 4: Prove the limit is hard.** As root, write `dd if=/dev/zero of=/srv/bmi/fill bs=1M count=9000`: it must stop with `No space left on device` at ≈ 7.8 GiB; while it ran, `df -h /` free space dropped by no more than the image's size (the image was pre-allocated, so it must not change at all); then `rm /srv/bmi/fill` and confirm `df /srv/bmi` is back to nearly empty.
- [ ] **Step 5: Create the PV/PVC** by applying `local-pv.yaml`; `kubectl get pv,pvc -n bmi` shows `Bound`.

### Task 10: Deploy with a seeded catalog; prove persistence

**Files:** none new (operational task). Records outcomes in `crates/bmi-server/deploy/k8s/README.md`.

**Interfaces:**
- Consumes: Tasks 8–9.
- Produces: running Deployment `bmi-server` (static catalog mode, `INDEX_REPO_URL` unset) with catalog seeded into `/data/catalog`.

- [ ] **Step 1: Ship the image:** `docker buildx build --platform linux/amd64 --load -f crates/bmi-server/Dockerfile -t bmi-server:$(git rev-parse --short HEAD) . && docker save bmi-server:<sha> | ssh kh@kh-kube 'sudo -n microk8s ctr image import -'`; set the tag in `bmi-server.yaml`.
- [ ] **Step 2: Create namespace and Secret `bmi-env`** (`openssl rand -hex 20` piped over SSH stdin into `kubectl create secret generic bmi-env --from-literal=ADMIN_TOKEN=…`); show the token to the user once and have them store it; apply PVC/Deployment/Service/backup CronJob.
- [ ] **Step 3: Seed the catalog:** build locally (`build_index.py --out $TMP/cat`), stream it into the PVC with `tar c -C $TMP/cat . | ssh … 'sudo -n microk8s kubectl -n bmi exec -i deploy/bmi-server -- tar x -C /data/catalog'`, then restart the pod.
- [ ] **Step 4: Verify the volume:** inside the pod `df -h /data` shows the ≈ 7.8G ext4 volume (not the host root) and `/metrics` reports `bmi_volume_total_bytes` ≈ that size. **Verify readiness:** `rollout status --timeout=300s`; in-cluster `/healthz` → `ok`; `kubectl -n bmi top pod` < 256Mi.
- [ ] **Step 5: Prove counts survive:** POST one download through a port-forward; `kubectl -n bmi rollout restart deploy/bmi-server`; after ready, `/admin/stats` `totalDownloads` is unchanged; then `kubectl delete pod` (SIGTERM path) and recheck; then trigger the backup job once (`kubectl create job --from=cronjob/bmi-backup`) and confirm `/data/backups/` has a file.

### Task 10a: Off-box backups (GATED on the user's choice of destination)

**Files:**
- Create: `crates/bmi-server/deploy/k8s/backup-offsite.yaml` (CronJob step or sidecar, per the chosen destination)

**Interfaces:**
- Consumes: nightly snapshots from Task 10.
- Produces: a copy of the newest snapshot stored **off the `kh-kube` disk** with 14-day retention.

- [ ] **Step 1: Ask the user where backups go** (Cloudflare R2 bucket via S3 API, Backblaze B2, or another machine on the tailnet via `scp`). The in-cluster snapshot lives on the same disk as the data, so it does not protect against disk or node loss; do not skip this. **Stop until answered.**
- [ ] **Step 2: Write the failing check:** a restore drill script `crates/bmi-server/tests/restore-drill.sh` that downloads the newest off-box snapshot to a temp dir, opens it read-only, and asserts `SELECT count(*) FROM dl_total` is > 0 and `PRAGMA integrity_check` is `ok`. Expected before implementing: FAIL.
- [ ] **Step 3: Implement** the upload (credentials as a Secret created via stdin; never in YAML) and 14-day retention; run the job once manually.
- [ ] **Step 4: Run the restore drill** → PASS.
- [ ] **Step 5: Commit** `feat(server): off-box backups`.

### Task 11: Cloudflare tunnel and DNS

**Files:** none new; update `crates/bmi-server/deploy/k8s/README.md` (tunnel name, hostname, rollback).

**Interfaces:**
- Consumes: Task 10 Service; Doppler `shamel-operator/prd` `CLOUDFLARE_MINTING_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`.
- Produces: tunnel `bmi-kh-kube` with ingress `bmi.khatab.me → http://bmi-server.bmi.svc.cluster.local:8080` + catch-all `http_status:404`; proxied CNAME `bmi.khatab.me → <tunnel-id>.cfargotunnel.com`; Secret `cloudflared-token`; running `cloudflared` pod.

- [ ] **Step 1: Pre-check (read-only):** `dig +short bmi.khatab.me` empty; with a short-lived Zone Read + Tunnel Read token confirm no tunnel `bmi-kh-kube` and no DNS record `bmi` exist. If either exists: STOP and report.
- [ ] **Step 2: Mint the work token** (≤ 2 h, policies as in Global Constraints); keep only in a shell variable.
- [ ] **Step 3: Create tunnel** (`POST /accounts/{id}/cfd_tunnel`, `config_src: "cloudflare"`), `PUT …/configurations` with the ingress, `GET …/token`.
- [ ] **Step 4: Create the CNAME** (proxied).
- [ ] **Step 5: Create Secret `cloudflared-token`** over stdin, apply `cloudflared.yaml`, wait for rollout (2 ready connectors).
- [ ] **Step 5b: Add the Cache Rule (append-only).** `GET /zones/{zone}/rulesets/phases/http_request_cache_settings/entrypoint`: record the existing rules (names only) in the task log; if the phase has a ruleset, `POST /zones/{zone}/rulesets/{id}/rules`, otherwise create the entrypoint ruleset. The one rule: expression `http.host eq "bmi.khatab.me" and http.request.method eq "GET" and (starts_with(http.request.uri.path, "/mods") or http.request.uri.path eq "/downloads" or starts_with(http.request.uri.path, "/thumbnails/"))`, action `set_cache_settings` with `cache: true`, `edge_ttl: { mode: "respect_origin" }`, `browser_ttl: { mode: "respect_origin" }`. After applying, re-list the phase's rules and assert every pre-existing rule is still present and unchanged.
- [ ] **Step 6: Delete the work token** and unset variables; confirm `success: true` and that the token now fails verification.
- [ ] **Step 7: Verify publicly:** `curl -fsS https://bmi.khatab.me/healthz` → `ok`; `/mods?limit=1` → one item; unauthenticated `/admin/stats` → 401; `curl -sI https://bmi.khatab.me/mods?limit=200` twice shows `cf-cache-status: MISS` then `HIT`; the same for `/downloads` and one thumbnail; `curl -sI` of `/healthz`, `/admin/stats`, `/metrics` never shows `HIT`; after `POST /admin/mods/{id}/hide` the mod is absent from a fresh `/mods` listing within 2 minutes.
- [ ] **Step 8: Edge load sanity.** `oha -z 30s -c 50 https://bmi.khatab.me/mods?limit=200` from this machine: no 5xx, and `bmi_requests_total` on the origin rose by far less than the requests sent (cache hit ratio > 90 %).

### Task 12: End-to-end verification and app default URL

**Files:**
- Modify: `src-tauri/src/bmi.rs:16` (`DEFAULT_BMI_SERVER_URL`); `docs/maintaining.md`

**Interfaces:**
- Consumes: Task 11 public URL.
- Produces: app default `https://bmi.khatab.me`; maintenance doc describing the Rust server on kh-kube (replaces the Cloudflare Worker sections).

- [ ] **Step 1:** `BMI_SERVER_URL=https://bmi.khatab.me cargo test -p balatro-mod-manager live_backend -- --ignored` → PASS.
- [ ] **Step 2: Prove real client IPs:** `curl -s -H "Authorization: Bearer $ADMIN_TOKEN" https://bmi.khatab.me/admin/whoami` equals `curl -s https://ifconfig.me`. If not, fix header handling before continuing.
- [ ] **Step 3: Prove limits are per client:** from this machine 130 rapid `GET /mods` yield some 429; a request from a different egress (phone hotspot) in the same moment is 200.
- [ ] **Step 4: Set the default URL, drop the `.invalid` doc rationale, update `docs/maintaining.md` (including: exactly what a report contains and the redaction rules, 30-day retention, how to read a report log with the admin token, and the storage budget table);** `cargo test --workspace` and `cargo clippy --workspace --all-targets` → 0 failures, 0 warnings.
- [ ] **Step 5: Launch the real app** (`bun run tauri dev`) with the user: list and thumbnails load, installing one mod increments its count in `/admin/stats`, a submitted report appears in `/admin/reports`. If the user cannot confirm, report exactly that.
- [ ] **Step 6: Commit** (app repo) `feat: default API URL https://bmi.khatab.me`.

### Task 12a: App resilience and lean refresh

**Files:**
- Modify: `/Users/kh/Projects/balatro-mod/src-tauri/src/bmi.rs` (`fetch_downloads_map`, `refresh_downloads`)
- Modify: `/Users/kh/Projects/balatro-mod/src-tauri/src/commands/repo.rs` (`fetch_repo_mods`, `fetch_repo_downloads`)
- Test: `src-tauri/src/bmi.rs` (`#[cfg(test)]`) and `src-tauri/src/commands/repo.rs`

**Interfaces:**
- Consumes: server `GET /downloads` (Task 4).
- Produces: `BmiClient::fetch_downloads_map(&self, sort: SortMode) -> Result<HashMap<String, ModDownloads>, String>` now issues one `GET downloads` request (the `sort` argument is retained for call-site compatibility but ignored); `fetch_repo_mods` returns the cached items (and logs a warning) when the health check or sync fails and a non-empty cache exists, and errors only when there is no cache.

- [ ] **Step 1: Write failing tests.** (a) `downloads_map_parses_lean_payload`: feed `{"a@Alpha":{"total":5,"today":1,"week":2}}` to the parsing function and assert `total == 5`, `today == 1`; (b) a pure function `fn cached_or_err(cache: Option<SyncCache>, err: String) -> Result<Vec<ArchiveModItem>, String>` returns the cache's items when non-empty and `Err(err)` otherwise — assert both; (c) extend `live_backend_roundtrip` to assert the downloads map has at least 100 entries and that the request count to `/mods` during a refresh is zero (use `GET /admin/stats` request counter delta or a response-header check; if unavailable, assert via a one-shot local stub server).
- [ ] **Step 2: Run** `cargo test -p balatro-mod-manager downloads_map cached_or_err` → FAIL.
- [ ] **Step 3: Implement** the two changes; keep `refresh_downloads` merging counts by `dir_name`.
- [ ] **Step 4: Run** `cargo test --workspace && cargo clippy --workspace --all-targets` → pass, 0 warnings; run the live test against the Rust server on 8789.
- [ ] **Step 5: Outage drill.** With the app's cache populated, stop the server and run the app (`bun run tauri dev`) with the user: the mod list still shows. Record the result (or that it was unconfirmed).
- [ ] **Step 6: Commit** (app repo) `feat: refresh download counts from /downloads; fall back to cached catalog when offline`.

### Task 12b: Report privacy in the app (redaction, preview, consent)

**Files:**
- Modify: `/Users/kh/Projects/balatro-mod/src-tauri/src/commands/report.rs`
- Modify: `/Users/kh/Projects/balatro-mod/src-tauri/Cargo.toml` (add `regex`)
- Modify: `/Users/kh/Projects/balatro-mod/src/components/ReportIssue.svelte`
- Test: `report.rs` (`#[cfg(test)]`), `/Users/kh/Projects/balatro-mod/src/components/ReportIssue.test.ts`

**Interfaces:**
- Produces: `fn redact(text: &str, home: Option<&Path>) -> String`; `fn prepare_log(raw: &str, home: Option<&Path>, max_bytes: usize) -> String` (redact first, then keep the newest `max_bytes`); Tauri command `preview_report() -> ReportPreview { os: String, arch: String, cpu: String, gpu: String, ram: String, log_filename: String, log: String, log_bytes: usize }` returning exactly the values `submit_report` will send; `submit_report(title, description, mm_version)` uses the same `prepare_log`.

- [ ] **Step 1: Write failing Rust tests** for `redact`: `/Users/kh/Library/Application Support/Balatro/mods` → `/Users/<user>/Library/Application Support/Balatro/mods`; `C:\Users\Jane Doe\AppData\Roaming\Balatro` → `C:\Users\<user>\AppData\Roaming\Balatro`; `/home/jane/.config/Balatro` → `/home/<user>/.config/Balatro`; with `home = Some("/Users/kh")` the text `opened /Users/kh/x` → `opened ~/x`; `contact a.b+c@example.com from 192.168.1.20 and 8.8.8.8` → `contact <email> from <ip> and <ip>`; a version string like `1.2.3.4` is also replaced (accepted over-redaction) but `0.4.1` and `2026-11-15` are untouched; ordinary lines (`Mod details fetch: title='Foo' id='a@Foo'`) are unchanged **except** that the `@` in a mod id is not mistaken for an e-mail (the id `a@Foo` stays, while an id with a dot in the mod name such as `x@Mod.Pack` is over-redacted to `<email>` — accepted and noted in `docs/maintaining.md`); `redact(redact(x)) == redact(x)`; `prepare_log` on a 500 KB input with a home path placed exactly across the truncation boundary never contains the username in its output.
- [ ] **Step 2: Write failing UI tests** (`ReportIssue.test.ts`): the submit button is disabled until the consent checkbox labelled "I understand this report includes the log shown below" is ticked; opening the form calls `preview_report` and renders the returned `log` in a scrollable region; submitting calls `submit_report` once with title, description and version only.
- [ ] **Step 3: Run** `cargo test -p balatro-mod-manager redact prepare_log` and `bun run test ReportIssue` → FAIL.
- [ ] **Step 4: Implement** the functions (precompiled `regex::Regex` in `std::sync::LazyLock`) and the UI (a collapsible "What will be sent" panel plus the checkbox); keep `MAX_LOG_BYTES = 400_000`.
- [ ] **Step 5: Run** `cargo test --workspace && cargo clippy --workspace --all-targets` (0 warnings) and `bun run check && bun run test` → pass.
- [ ] **Step 6: Manual check with the user:** open the real form, confirm the preview shows `~`/`<user>` in place of the home directory, the button stays disabled until ticked, and a submitted report's log fetched via `GET /admin/reports/{id}/log` contains no account name. Report exactly what was and was not confirmed.
- [ ] **Step 7: Commit** (app repo) `feat: redact reports, preview and consent before sending`.

### Task 13: Server-specific CI checks and retire the Worker

**Files:**
- Create: `/Users/kh/Projects/balatro-mod/.github/workflows/server-ci.yml` (path-filtered to `crates/**`, `Cargo.toml`, `Cargo.lock`; Linux only)
- Delete in the index repo: `.github/workflows/deploy-api.yml`, `.github/workflows/update-mod-versions.yml`, `worker/` (retire the Worker; the server's own sync replaces the version-update bot — if the user wants automatic version bumps kept, that is a separate task)

**Interfaces:**
- Produces: a Linux-only workflow running `cargo fmt --all --check` and building the container image (`docker buildx build --platform linux/amd64 -f crates/bmi-server/Dockerfile .` followed by `crates/bmi-server/tests/image-smoke.sh`). Test and clippy runs for `bmi-core`/`bmi-server` are **not** duplicated here: Task 15's OS matrix owns them.

- [ ] **Step 1:** Write the workflow (cache action pinned to a release at least two weeks old); validate with `actionlint`; locally run `cargo fmt --all --check` and the image smoke script → pass.
- [ ] **Step 2:** In the index repo `git rm -r worker .github/workflows/deploy-api.yml .github/workflows/update-mod-versions.yml` and commit `chore: retire Cloudflare Worker and version bot`; in the app repo commit `ci: server fmt and image build`.

### Task 14: Turn on git-driven sync (GATED: requires the user's approval to push)

**Files:**
- Modify: `crates/bmi-server/deploy/k8s/bmi-server.yaml` (env `INDEX_REPO_URL`, `INDEX_BRANCH`)

**Interfaces:**
- Consumes: a pushed index branch containing `scripts/build_index.py` with `--out` at `https://github.com/ahmmkh/Balatro-Mod-Index`.
- Produces: catalog rebuilt every 900 s from the fork.

- [ ] **Step 1: Ask the user for approval** to push the index branch and app branch to their forks and open PRs into their own `main`. **Stop here unless approved.**
- [ ] **Step 2:** Push/merge per instruction; verify `git show <branch>:scripts/build_index.py | grep -- --out`.
- [ ] **Step 3:** Set `INDEX_REPO_URL=https://github.com/ahmmkh/Balatro-Mod-Index.git` and `INDEX_BRANCH`, apply, wait for ready.
- [ ] **Step 4: Verify:** `POST /admin/reload` → 202; within 3 minutes `/admin/stats` shows `sync.lastError == null`, `sync.head` equals `git ls-remote` of the branch; after two more forced reloads `ls /data/catalog` shows ≤ 2 `build-*` dirs.
- [ ] **Step 5: Verify a failure is safe:** point `INDEX_BRANCH` at a branch without `--out` support (e.g. upstream `main`), reload: `/healthz` stays `ok`, the old catalog keeps serving, `sync.lastError` is non-null; restore the correct branch.
- [ ] **Step 6: Verify tombstones for real** on a scratch branch that deletes one mod: `/mods/changed?since=<recent>` includes `{id, deleted:true}`; switching back restores it.

## Track B: cross-platform and index testing (Tasks 15–22)

Requires Tasks 1–7 (the server compiles and is tested). Independent of the infrastructure Tasks 8–14, except that **anything that only a real GitHub Actions run can prove (Tasks 15, 18, 19, 20) is verified locally first and on CI only after the user approves the push in Task 14**; never report CI as green until the run has been observed.

### Task 15: Run the test suites on all three OSes

**Files:**
- Modify: `/Users/kh/Projects/balatro-mod/.github/workflows/ci.yml`

**Interfaces:**
- Produces: CI jobs `rust-tests` and `rust-lint` with `strategy.matrix.os: [ubuntu-22.04, macos-latest, windows-latest]` and `fail-fast: false`. Packages: `bmm-lib`, `balatro-mod-manager`, `bmi-core` on all three; `bmi-server` on all three (compile + unit tests; it is deployed only on Linux but must not rot elsewhere). Keep `-- --test-threads=1`. Frontend job stays on Linux.

- [ ] **Step 1: Write the matrix.** Linux-only system-dependency steps stay guarded by `if: runner.os == 'Linux'`; use `shell: bash` for cross-OS script steps; `cargo clippy --workspace --all-targets -- -D warnings` runs per OS because `cfg(windows)`/`cfg(target_os)` code is only compiled on its own OS.
- [ ] **Step 2: Validate locally.** Run `actionlint .github/workflows/ci.yml` (install with `brew install actionlint` if missing) → no findings; run the exact test and clippy commands on this macOS machine → pass.
- [ ] **Step 3: After the push is approved (Task 14), observe the first run on all three OSes.** Fix real failures (path separators, line endings, env-var assumptions) in code or tests per the cross-platform bar. Done when one run is green on all three OSes; paste the three job URLs into the commit message of the last fix.
- [ ] **Step 4: Commit** `ci: run Rust tests and clippy on ubuntu, macos, windows`.

### Task 16: Make game-path detection testable for every OS

**Files:**
- Modify: `/Users/kh/Projects/balatro-mod/src-tauri/bmm-lib/src/finder.rs`
- Test: `finder.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `pub enum Os { Windows, MacOs, Linux }` with `Os::current() -> Os`; `pub struct PathEnv { pub os: Os, pub home: Option<PathBuf>, … }` whose remaining fields are **exactly** the machine inputs the three existing `get_balatro_paths` bodies read (environment variables, registry-derived Steam roots, XDG dirs); list them in the commit message and add no others; `pub trait FsProbe { fn exists(&self, p: &Path) -> bool; fn read_to_string(&self, p: &Path) -> Option<String>; fn list_dir(&self, p: &Path) -> Vec<PathBuf>; }`; `pub fn balatro_paths_in(env: &PathEnv, fs: &dyn FsProbe) -> Vec<PathBuf>` containing all layout logic with branching on `env.os`, not on `cfg`; the existing `get_balatro_paths() -> Vec<PathBuf>` keeps its signature and becomes a thin wrapper that builds the real `PathEnv` (the only part still behind `cfg(target_os)`, e.g. registry reads) and a `RealFs`.

- [ ] **Step 1: Write failing tests** using an in-memory fake `FsProbe` that normalizes `\` to `/` so layout logic runs on every host: Windows default Steam (`C:/Program Files (x86)/Steam/steamapps/common/Balatro`); Windows second library listed in `libraryfolders.vdf`; macOS `~/Library/Application Support/Steam/steamapps/common/Balatro`; Linux native (`~/.local/share/Steam/...`); Linux Flatpak Steam (`~/.var/app/com.valvesoftware.Steam/.local/share/Steam/...`); Steam Deck/Proton layout containing `compatdata/2379780`; plus: no Balatro anywhere → empty vec; the same install reachable via two library entries → returned once; result order is stable across runs.
- [ ] **Step 2: Run** `cargo test -p bmm-lib finder` → FAIL (symbols missing).
- [ ] **Step 3: Implement** the extraction without changing behaviour: first run the old `get_balatro_paths()` on this machine and save its output, then confirm the wrapper returns an identical list (record both in the commit message).
- [ ] **Step 4: Run** `cargo test -p bmm-lib && cargo clippy -p bmm-lib --all-targets -- -D warnings` → pass; the new tests run on every CI OS (real Windows separators are exercised by the Windows job in Task 15).
- [ ] **Step 5: Commit** `refactor(finder): pure per-OS path detection with fake-filesystem tests`.

### Task 17: Install-flow tests with a fake game directory and unified archive safety

**Files:**
- Modify: `/Users/kh/Projects/balatro-mod/src-tauri/bmm-lib/src/lib.rs` (add `config_root`), `src-tauri/bmm-lib/src/installer.rs`, and every call site of `dirs::config_dir()` in `src-tauri/` (find with `grep -rn "dirs::config_dir()"`)
- Modify: `src-tauri/bmm-lib/Cargo.toml` (dev-dependency `tiny_http`; pick the newest release at least two weeks old)
- Create: `/Users/kh/Projects/balatro-mod/src-tauri/bmm-lib/tests/install_flow.rs`

**Interfaces:**
- Produces: `pub fn config_root() -> PathBuf` returning `$BMM_CONFIG_DIR` when set (non-empty) and `dirs::config_dir()` otherwise (needed because `dirs` ignores environment overrides on Windows); after this task `dirs::config_dir()` appears **only** inside `config_root`; test helpers `fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8>` and `struct FixtureServer { pub base_url: String }` with `FixtureServer::start(files: Vec<(String, Vec<u8>)>) -> FixtureServer` (stops on drop); `installer::install_mod(url: String, folder_name: Option<String>) -> Result<PathBuf, AppError>` unchanged in signature.

- [ ] **Step 1: Write failing tests** (each uses a tempdir as `BMM_CONFIG_DIR`, set under the crate's existing `ENV_LOCK`; run on every OS): installing a normal archive (`ModA/main.lua`, `ModA/manifest.json`) creates `<config_root>/Balatro/Mods/ModA` containing both files and returns that path; a flat archive (files at the root) with `folder_name = Some("Flat")` lands in `Mods/Flat`; unicode name `Ünïcödé Mod` installs and `finder::get_installed_mods()` lists it; entries `../evil.txt`, `/abs.txt` and `C:\x.txt` are each rejected with an error and **nothing** is written outside the mods dir (assert the tempdir tree); entries `a:b.lua`, `CON.lua`, `bad.` and `bad ` are rejected with the same error variant on every OS; `Foo.lua` + `foo.lua` in one archive is rejected as a case collision on every OS; an entry whose full target path exceeds 240 characters is rejected; an archive that fails halfway (second entry invalid) leaves no `Mods/<folder>` directory; a 404 from the server and a non-archive body each return a network/format error without touching the mods dir; `uninstall_mod` on a path outside the mods dir is refused.
- [ ] **Step 2: Run** `cargo test -p bmm-lib --test install_flow` → FAIL.
- [ ] **Step 3: Implement** `config_root` and route all call sites through it (verify with `grep -rn "dirs::config_dir()" src-tauri | wc -l` → 1); implement the unified validation as one function `fn validate_entry_name(name: &str) -> Result<(), AppError>` used for every archive entry before any write, plus extract-to-temp-then-rename so failures leave nothing behind.
- [ ] **Step 4: Run** `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings` → pass on this machine; the Windows/Linux runs are observed in Task 15's CI.
- [ ] **Step 5: Commit** `feat(installer): identical archive safety on every OS; config_root override; install-flow tests`.

### Task 18: Client/server contract test on every OS

**Files:**
- Create: `/Users/kh/Projects/balatro-mod/crates/bmi-server/tests/fixtures/catalog/index.json` (≥ 120 synthetic mods, 3 with `thumbnail_url`), `crates/bmi-server/tests/fixtures/catalog/thumbnails/*.webp` (three tiny valid webp files), `crates/bmi-server/tests/fixtures/mod_item.json` (one server-emitted mod item)
- Create: `/Users/kh/Projects/balatro-mod/.github/workflows/contract.yml`
- Modify: `/Users/kh/Projects/balatro-mod/src-tauri/src/bmi.rs` (test `server_mod_item_parses`)

**Interfaces:**
- Consumes: the existing ignored test `bmi::tests::live_backend_roundtrip`; the Rust server binary from Task 6.
- Produces: a CI job per OS (`ubuntu-22.04`, `macos-latest`, `windows-latest`) that builds `bmi-server`, starts it in the background with `CATALOG_DIR=<fixture>`, `PORT=8789`, `DATA_DIR=<tmp>` (no `INDEX_REPO_URL`), waits (≤ 30 s) for `http://127.0.0.1:8789/healthz`, runs `BMI_SERVER_URL=http://127.0.0.1:8789 cargo test -p balatro-mod-manager live_backend -- --ignored`, then kills the server (`shell: bash` on all three; never `localhost`).

- [ ] **Step 1: Write the failing wire-drift test** `server_mod_item_parses` in `bmi.rs`: `include_str!("../../crates/bmi-server/tests/fixtures/mod_item.json")` deserializes into `BmiMod` and, passed through `bmi_to_archive`, yields `has_thumbnail == true`, `meta.downloads` present, `dir_name == id`, and a `bmi://<id>` download URL. Generate `mod_item.json` from a real server response (`curl /mods/<id>` against the Rust server on the fixture) so it is genuinely server-emitted.
- [ ] **Step 2: Run** `cargo test -p balatro-mod-manager server_mod_item_parses` → FAIL until the fixture exists, then PASS.
- [ ] **Step 3: Write `contract.yml`** as described; validate with `actionlint`; run the same sequence by hand on this machine (server in background, test, kill) → PASS.
- [ ] **Step 4: After the push is approved, observe the job green on all three OSes.** A failure that is a real OS bug (for example a Windows TLS or path issue in the client) is fixed in the client, not worked around in the workflow.
- [ ] **Step 5: Commit** `test: client/server contract test on ubuntu, macos, windows`.

### Task 19: Packaged-app smoke test

**Files:**
- Modify: `/Users/kh/Projects/balatro-mod/src-tauri/src/lib.rs` (add `smoke_test`), `src-tauri/src/main.rs` (flag), `.github/workflows/build.yml`
- Test: `src-tauri/src/lib.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `pub fn smoke_test() -> Result<String, String>`: with `BMM_CONFIG_DIR` honoured, initializes logging, creates the config directories, constructs `BmiClient`, and (only when `BMI_SERVER_URL` is set) calls `check_health`; returns `Ok("BMM_SMOKE_OK <app version>")`. `main` handles `--smoke-test` before creating any window or Tauri runtime: prints the string and exits 0, or prints the error to stderr and exits 1.

- [ ] **Step 1: Write failing tests:** `smoke_test()` with `BMM_CONFIG_DIR` pointing at a tempdir returns `Ok` whose text starts with `BMM_SMOKE_OK ` and leaves a `Balatro/logs/bmm_*.log` file; with `BMI_SERVER_URL=http://127.0.0.1:9` (nothing listening) it returns `Err` mentioning health.
- [ ] **Step 2: Run** `cargo test -p balatro-mod-manager smoke_test` → FAIL.
- [ ] **Step 3: Implement** the function and the flag; in `build.yml` add, after each platform's bundle step, a `shell: bash` step running the built executable with `--smoke-test` and `BMM_CONFIG_DIR=$RUNNER_TEMP/bmm` (Windows `target/release/BMM.exe`, macOS `…/Balatro Mod Manager.app/Contents/MacOS/BMM`, Linux the built binary under `xvfb-run -a`), asserting stdout contains `BMM_SMOKE_OK`.
- [ ] **Step 4: Verify locally** on macOS: build with `bun run tauri build --debug --bundles app`, then run `"…/Contents/MacOS/BMM" --smoke-test` → prints `BMM_SMOKE_OK 0.4.1`, exit code 0, and no window appears.
- [ ] **Step 5: After the push is approved, observe the build workflow's smoke step pass on all three OSes.**
- [ ] **Step 6: Commit** `feat: --smoke-test flag and packaged-app smoke step in build workflow`.

### Task 20: Nightly "does the index actually install" canary

**Files:**
- Create: `/Users/kh/Projects/balatro-mod/src-tauri/bmm-lib/examples/canary.rs`, `/Users/kh/Projects/balatro-mod/.github/workflows/canary.yml`
- Test: unit tests inside `canary.rs`

**Interfaces:**
- Produces: `cargo run -p bmm-lib --example canary -- --catalog <URL-or-path> --sample <N|all> --seed <u64> --out <summary.json>`; `enum FailureClass { Network, NotAnArchive, UnsafePath, WindowsIllegalName, CaseCollision, TooLong, Empty, TooLarge, Other }`; `fn classify(err: &AppError) -> FailureClass`; `fn sample_ids(ids: &[String], n: usize, seed: u64) -> Vec<String>` (deterministic; `n >= len` returns all, stable order); `fn render_summary(results: &[ModResult]) -> String` (Markdown table grouped by `FailureClass`). Behaviour: reads `meta.downloadURL` from the catalog (**never** calls `POST …/download`), installs each sampled mod into a temp `BMM_CONFIG_DIR` via `install_mod`, verifies the folder contains a `.json` manifest or a `.lua` file, uninstalls it, skips archives larger than 100 MB (recording `TooLarge`), concurrency 2, identifying User-Agent, uses `GITHUB_TOKEN` for github.com URLs when present. Exit code 0 unless the harness itself fails or more than 50 % of the sample fails (that signals an outage, not bad mods).

- [ ] **Step 1: Write failing unit tests:** `classify` table (one `AppError` per class, using the variants Task 17 introduced); `sample_ids` returns the same 3 ids for the same seed and a different set for another seed, returns all when `n` exceeds the length, never repeats an id; `render_summary` snapshot with two failures in different classes and one success.
- [ ] **Step 2: Run** `cargo test -p bmm-lib --example canary` → FAIL.
- [ ] **Step 3: Implement** the example; run it locally against the fixture server from Task 18 (`--catalog http://127.0.0.1:8789 --sample 5 --seed 1`) → exits 0 and writes `summary.json`; confirm `/admin/stats` `totalDownloads` is unchanged afterwards (the canary did not touch the counters).
- [ ] **Step 4: Write `canary.yml`:** cron `17 4 * * *` plus `workflow_dispatch` (input `sample`, default `40`; `all` allowed), matrix over the three OSes, seed = the UTC date as an integer, uploads `canary-<os>.json`, appends the Markdown summary to `$GITHUB_STEP_SUMMARY`; validate with `actionlint`. Catalog source: `https://bmi.khatab.me` (after Task 12) or, until then, the index repo's built `index.json`.
- [ ] **Step 5: Commit** `test: nightly install canary across the catalog`.

### Task 21: Index data validation in the index repo

**Files:**
- Create: `/Users/kh/Projects/balatro-mod-index/scripts/validate_index.py`, `/Users/kh/Projects/balatro-mod-index/scripts/test_validate_index.py`, `/Users/kh/Projects/balatro-mod-index/scripts/requirements-dev.txt` (`pytest`, `Pillow`), `/Users/kh/Projects/balatro-mod-index/.github/workflows/validate.yml`

**Interfaces:**
- Produces: `@dataclass Finding(severity: Literal["error","warning"], code: str, mod: str, message: str)`; `def validate_mod(mod_dir: Path) -> list[Finding]`; CLI `python scripts/validate_index.py [--mods-dir mods] [--changed-only <git-range>]` printing one line per finding and exiting 1 if any `error` exists. Rules: `error`: `meta.json` unparsable (lenient parse as in `build_index.py`), missing `title`/`categories`/`author`/`repo`/`downloadURL`, `downloadURL` not `https`, `folderName` illegal on Windows (same rules as the Task 17 constants) or longer than 100 characters, mod directory name not `<author>@<name>` or containing characters unsafe in a URL path, `description.md` missing, thumbnail not decodable or larger than 5 MB; `warning`: `repo` not on github.com, thumbnail wider than 2000 px, two mods whose `title` matches case-insensitively.

- [ ] **Step 1: Write failing tests** (pytest, one temp mod directory per case): a valid mod yields no findings; each `error` rule above yields exactly one finding with the expected `code`; each `warning` rule yields a warning and does not make the CLI exit non-zero; `--changed-only HEAD~1..HEAD` validates only mods touched in that range.
- [ ] **Step 2: Run** `python -m pytest scripts/test_validate_index.py` → FAIL.
- [ ] **Step 3: Implement** the script; then run it on the real index: every pre-existing `error` is triaged — fix the data in `mods/` (own commit) or, if the rule is wrong for legitimate mods, downgrade it to `warning` with the reason in a comment. End state: the repo exits 0.
- [ ] **Step 4: Write `validate.yml`** (on `pull_request` and push to the default branch; `fetch-depth: 0`; runs `--changed-only` on PRs and a full run on push); validate with `actionlint`.
- [ ] **Step 5: Commit** `ci: validate mod metadata, URLs, folder names and thumbnails`.

### Task 22: Release checklist for what automation cannot cover

**Files:**
- Create: `/Users/kh/Projects/balatro-mod/docs/release-checklist.md`

**Interfaces:**
- Produces: a checklist run once per release on each real platform, results pasted into the release notes.

- [ ] **Step 1: Write the document** with a table (rows = checks, columns = Windows 11, macOS Apple Silicon, Linux native, Steam Deck/Proton) covering: install the packaged app; game path auto-detected (and manual selection works); Steamodded and Lovely install and are detected; install, update and uninstall one normal mod; a mod with a Unicode name; launch the game from the app and confirm Lovely injects (mod visible in the game's menu); report form shows the redacted log and the consent box; app shows the cached catalog with the network disabled; update prompt appears for an older build. Each row has an `Expected` column and an empty `Result` column; a final section records which OSes were **not** covered for that release.
- [ ] **Step 2: Dry-run the macOS column with the user** against the current build, recording results; any row that cannot be checked is marked "not covered", not "pass".
- [ ] **Step 3: Commit** `docs: per-release manual test checklist`.

## Out of scope (separate plans)

- Self-hosted Umami + Postgres on kh-kube and wiring `BMM_UMAMI_URL` / `BMM_UMAMI_WEBSITE_ID` into the app's release workflow.
- Release workflows, macOS signing/notarization, publishing the first fork release.
- Browser/desktop UI end-to-end automation (Playwright flows, `tauri-driver` on Windows/Linux). Component tests already exist and the packaged-app smoke test (Task 19) covers startup; revisit if UI regressions escape.
- Automating a real Balatro launch with Lovely injection (the game is paid software and cannot run in CI); covered by the manual checklist in Task 22.
- Porting `scripts/build_index.py` to Rust (it stays Python; the image carries `python3-pil`).
- Making the Tauri app depend on `bmi-core` for its wire types (mechanical follow-up now that both are in one workspace; the app's `BmiMod` is deliberately more tolerant than the server's strict types, so do it deliberately, not as a drive-by).

## Future direction: compatibility checks and dependency resolution (not tasks)

Planned after this deploys. Recorded here only so today's choices don't block it:

- **Where it goes:** a new `crates/bmi-resolve` library (depends on `bmi-core`; candidate solver: the `pubgrub` crate, which provides conflict explanations suitable for user-facing messages), used by (a) the app, offline, before installing, (b) `bmi-server` behind a `POST /resolve`, and (c) a small `bmi-cli check` run in the index repo's PR CI to reject submissions whose declared requirements cannot be satisfied.
- **Data it will need that the index does not have today:** each mod's own manifest (Steamodded-style `dependencies`, `conflicts`, version ranges including its `~` beta syntax), plus game and Lovely/Steamodded versions. This will come from a server-side ingestion job that reads manifests out of the mod archives per version and stores them (new table, keyed by mod id and version) — not from hand-edited `meta.json`.
- **Guards already in this plan:** `bmi-core` has no I/O or async dependencies; `CatalogMod.meta` is an open map so new fields pass through; `mod_state` is keyed by stable mod id; the server is a library plus binary so a resolver endpoint is just another router module.
