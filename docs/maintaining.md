# Maintaining this fork

This fork replaces the original author's (now offline) servers with a backend you own.

| Piece | Where | Notes |
|---|---|---|
| Mod catalog + thumbnails + download counts + reports | Cloudflare Worker + D1, source in the **Balatro-Mod-Index** fork (`worker/`) | Free tier is plenty. |
| Usage analytics (optional) | Your own Umami instance | Off unless configured at build time. |

## One-time backend deploy (Cloudflare)

In `Balatro-Mod-Index/worker`:

```sh
bun install
bunx wrangler login
bunx wrangler d1 create bmi            # copy the printed database_id into wrangler.jsonc
python3 -m pip install Pillow && bun run build:index
bunx wrangler d1 migrations apply bmi --remote
bunx wrangler secret put IP_SALT       # any long random string; salts the de-dup hashes
bunx wrangler deploy                   # prints https://bmi-api.<your-subdomain>.workers.dev
```

Then, for automatic redeploys, add repository secrets `CLOUDFLARE_API_TOKEN` (Workers + D1 edit) and
`CLOUDFLARE_ACCOUNT_ID` to the index repo and enable Actions. The `Update Mod Versions` job refreshes
versions hourly and `Deploy API` republishes the catalog.

## Point the app at it

Set `DEFAULT_BMI_SERVER_URL` in `src-tauri/src/bmi.rs` to the deployed Worker URL. For local development
run `bunx wrangler dev --port 8787` in the index repo's `worker/` and start the app with
`BMI_SERVER_URL=http://localhost:8787`.

Verify the full client/server round trip any time with:

```sh
BMI_SERVER_URL=http://localhost:8787 cargo test -p balatro-mod-manager live_backend -- --ignored
```

## Analytics (optional)

Build with `BMM_UMAMI_URL=https://<your-umami>/api/send` and `BMM_UMAMI_WEBSITE_ID=<id>`. If either is
missing, the app sends nothing. The existing in-app opt-out toggle still applies.

## Reading reports

```sh
bunx wrangler d1 execute bmi --remote --command "SELECT id, created_at, title, mm_version, os FROM reports ORDER BY id DESC LIMIT 20"
```

## Before cutting a release

- Set the real `DEFAULT_BMI_SERVER_URL`. The shipped placeholder (`bmi-api.invalid`) deliberately fails closed.
- `.github/workflows/*` release jobs have not been reviewed for the fork: they must export `BMM_UMAMI_URL` /
  `BMM_UMAMI_WEBSITE_ID` (as secrets) or telemetry is silently off, and signing/notarization secrets are needed
  for macOS builds. The README and install scripts point at `ahmmkh/balatro-mod-manager` releases, which
  don't exist until you publish one.
- Prefer a custom domain over `*.workers.dev` for the Worker: it keeps the URL independent of your Cloudflare
  account name inside shipped binaries.
- Not yet verified: launching the app itself (`BMI_SERVER_URL=http://localhost:8787 bun run tauri dev`) and
  installing a mod through the UI. Only the Rust client <-> Worker round trip is tested.

## Known gaps

- Download counts start at zero (the old totals went offline with the old server) and are cached for up to
  60 s. One count per visitor, per mod, per UTC day.
- The catalog is rebuilt from the index repo, so a mod removed from the index lingers in clients' local cache
  until the cache is rebuilt.
- `src-tauri/src/lfs.rs` still references the old author's Git LFS host. It only triggers on LFS pointer files,
  which the new backend never serves, and can be deleted.
- `scripts/aur-update.sh` still targets the original AUR package.
- The Ko-fi button in About still points at the original author.
