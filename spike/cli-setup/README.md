# CLI setup prototype (throwaway)

This prototype backs [docs/design/cli-setup.md](../../docs/design/cli-setup.md). It is not product code and is not meant to be merged. It ran once against the test account "Flarebox deploy test" on 2026-11-10.

- `fbsetup.mjs` runs the Cloudflare side of `flarebox setup` and `flarebox server upgrade` with an OAuth (PKCE) token: login with a loopback listener, refresh, revoke, the R2 check, discovery of a Flarebox Worker by script tag, bucket creation, the static asset upload, the script upload, the workers.dev route, the claim with a one-time Worker secret, upgrade, and teardown. It needs Node 22 and nothing else. Tokens and device credentials go to 0600 files under `$FB_STATE`, and the script prints only an 8-digit SHA-256 prefix of each token.
- `make-manifest.mjs` writes the `manifest.json` a release bundle would carry, from `server/wrangler.jsonc`.
- `server/src/account/claim.ts` on this branch lets a correct `FLAREBOX_RECOVERY_KEY` claim an unclaimed deployment. The design doc proposes the real change.

A bundle directory holds `manifest.json`, `worker/worker.js` (from `wrangler deploy --dry-run --outdir`) and `assets/` (a copy of `server/web/dist`).
