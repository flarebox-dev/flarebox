// Throwaway prototype for docs/design/cli-setup.md. Not product code.
// Proves the Cloudflare API calls behind "flarebox setup" with an OAuth (PKCE) token. Node 22, no dependencies.
// Tokens and device credentials live in 0600 files under $FB_STATE (default ~/.capy/work/BOX-87/secrets).
// Nothing secret is printed: tokens appear only as the first 8 hex digits of their SHA-256.

import { createHash, randomBytes } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { homedir } from "node:os";
import { extname, join, relative } from "node:path";

const AUTH = "https://dash.cloudflare.com/oauth2/auth";
const TOKEN = "https://dash.cloudflare.com/oauth2/token";
const REVOKE = "https://dash.cloudflare.com/oauth2/revoke";
const API = "https://api.cloudflare.com/client/v4";
const TEST_ACCOUNT = "4494ddfb6d3bbb1a287ba086b727eeef";
const TAG = "flarebox-server";
const KEY_NAME = "FLAREBOX_RECOVERY_KEY";

const STATE = process.env.FB_STATE ?? join(homedir(), ".capy/work/BOX-87/secrets");
const TOKENS = join(STATE, "oauth.json");
mkdirSync(STATE, { recursive: true, mode: 0o700 });
chmodSync(STATE, 0o700);

const log = (...a) => console.log(new Date().toISOString(), ...a);
const fp = (s) => (s ? createHash("sha256").update(s).digest("hex").slice(0, 8) : "none");
const b64url = (buf) => buf.toString("base64url");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function saveSecret(path, value) {
  writeFileSync(path, JSON.stringify(value, null, 2), { mode: 0o600 });
  chmodSync(path, 0o600);
}
const load = (path) => JSON.parse(readFileSync(path, "utf8"));

function tokenSummary(t) {
  return {
    token_type: t.token_type,
    expires_in: t.expires_in,
    scope: t.scope,
    access: fp(t.access_token),
    access_len: t.access_token?.length,
    refresh: fp(t.refresh_token),
    refresh_len: t.refresh_token?.length,
    id_token: Boolean(t.id_token),
    other_fields: Object.keys(t).filter((k) => !["access_token", "refresh_token", "token_type", "expires_in", "scope", "id_token"].includes(k)),
  };
}

async function tokenRequest(form) {
  const res = await fetch(TOKEN, { method: "POST", headers: { "Content-Type": "application/x-www-form-urlencoded" }, body: new URLSearchParams(form) });
  const text = await res.text();
  let body;
  try {
    body = JSON.parse(text);
  } catch {
    body = { raw: text.slice(0, 300) };
  }
  if (!res.ok) return { ok: false, status: res.status, error: body.error, error_description: body.error_description ?? body.raw };
  return { ok: true, status: res.status, body };
}

// OAuth

async function login({ clientId, port, path = "/oauth/callback", host = "127.0.0.1", scopes, out = TOKENS }) {
  const verifier = b64url(randomBytes(32));
  const challenge = b64url(createHash("sha256").update(verifier).digest());
  const state = b64url(randomBytes(16));
  const redirectUri = "http://" + host + ":" + port + path;
  const url = new URL(AUTH);
  url.search = new URLSearchParams({
    response_type: "code",
    client_id: clientId,
    redirect_uri: redirectUri,
    scope: scopes.join(" "),
    state,
    code_challenge: challenge,
    code_challenge_method: "S256",
  }).toString();

  const result = await new Promise((resolve, reject) => {
    const server = createServer(async (req, res) => {
      const u = new URL(req.url, redirectUri);
      if (u.pathname !== path) {
        res.writeHead(404).end();
        return;
      }
      const params = Object.fromEntries(u.searchParams);
      log("callback received with parameters:", Object.keys(params).join(","), params.error ? "error=" + params.error + " " + (params.error_description ?? "") : "");
      if (params.state !== state) {
        res.writeHead(400).end("state mismatch");
        log("state mismatch; ignoring");
        return;
      }
      res.writeHead(200, { "Content-Type": "text/plain" }).end(params.code ? "Signed in. You can close this tab." : "Sign-in failed: " + (params.error ?? "unknown"));
      server.close();
      resolve(params);
    });
    server.on("error", reject);
    server.listen(Number(port), host, () => {
      log("listening on " + redirectUri);
      log("AUTHORIZE_URL " + url.toString());
    });
  });
  if (!result.code) throw new Error("no code: " + result.error);
  const started = Date.now();
  const exchanged = await tokenRequest({ grant_type: "authorization_code", code: result.code, redirect_uri: redirectUri, client_id: clientId, code_verifier: verifier });
  if (!exchanged.ok) throw new Error("token exchange failed: " + JSON.stringify(exchanged));
  saveSecret(out, { ...exchanged.body, client_id: clientId, obtained_at: started });
  log("token exchange ok", JSON.stringify(tokenSummary(exchanged.body)));
}

async function refresh({ scope, out = TOKENS, file = TOKENS, reuse = false }) {
  const t = load(file);
  const form = { grant_type: "refresh_token", refresh_token: t.refresh_token, client_id: t.client_id };
  if (scope) form.scope = scope.replaceAll(",", " ");
  const r = await tokenRequest(form);
  if (!r.ok) {
    log("refresh failed", JSON.stringify(r));
    return;
  }
  log("refresh ok", JSON.stringify({ ...tokenSummary(r.body), rotated: r.body.refresh_token !== t.refresh_token, old_access: fp(t.access_token), old_refresh: fp(t.refresh_token) }));
  saveSecret(out, { ...r.body, client_id: t.client_id, obtained_at: Date.now() });
  if (reuse && r.body.refresh_token !== t.refresh_token) {
    const again = await tokenRequest({ grant_type: "refresh_token", refresh_token: t.refresh_token, client_id: t.client_id });
    log("reusing the old refresh token:", JSON.stringify({ ok: again.ok, status: again.status, error: again.error }));
  }
}

async function revoke({ file = TOKENS }) {
  const t = load(file);
  for (const [kind, token] of [["refresh_token", t.refresh_token], ["access_token", t.access_token]]) {
    if (!token) continue;
    const res = await fetch(REVOKE, { method: "POST", headers: { "Content-Type": "application/x-www-form-urlencoded" }, body: new URLSearchParams({ token, token_type_hint: kind, client_id: t.client_id }) });
    log("revoke " + kind + ": HTTP " + res.status);
  }
  const res = await fetch(API + "/accounts", { headers: { Authorization: "Bearer " + t.access_token } });
  log("GET /accounts with the revoked access token -> HTTP " + res.status);
  const r = await tokenRequest({ grant_type: "refresh_token", refresh_token: t.refresh_token, client_id: t.client_id });
  log("refresh with the revoked refresh token:", JSON.stringify({ ok: r.ok, status: r.status, error: r.error }));
}

// Cloudflare API

function cf(file = TOKENS) {
  const t = load(file);
  return async function call(method, path, { json, form } = {}) {
    const init = { method, headers: { Authorization: "Bearer " + t.access_token } };
    if (json !== undefined) {
      init.headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(json);
    }
    if (form) init.body = form;
    const res = await fetch(API + path, init);
    const text = await res.text();
    let body;
    try {
      body = JSON.parse(text);
    } catch {
      body = { raw: text.slice(0, 300) };
    }
    const errors = (body.errors ?? []).map((e) => e.code + ": " + e.message);
    log(method + " " + path + " -> HTTP " + res.status + (errors.length ? " errors=" + JSON.stringify(errors) : ""));
    return { status: res.status, ok: res.ok && body.success !== false, body, errors };
  };
}

async function pickAccount(call) {
  const accounts = (await call("GET", "/accounts")).body.result ?? [];
  log("accounts visible:", JSON.stringify(accounts.map((a) => ({ id: a.id, name: a.name }))));
  if (accounts.length !== 1 || accounts[0].id !== TEST_ACCOUNT) {
    throw new Error("refusing to continue: the token must see exactly the test account");
  }
  return accounts[0].id;
}

async function r2Status(call, account) {
  const r = await call("GET", "/accounts/" + account + "/r2/buckets");
  if (r.ok) return { enabled: true, buckets: (r.body.result?.buckets ?? []).map((b) => b.name) };
  return { enabled: false, off: r.errors.some((e) => e.startsWith("10042")), errors: r.errors, dashboard: "https://dash.cloudflare.com/" + account + "/r2/overview" };
}

async function findServers(call, account) {
  const scripts = (await call("GET", "/accounts/" + account + "/workers/scripts")).body.result ?? [];
  log("scripts in account:", JSON.stringify(scripts.map((s) => ({ id: s.id, tags: s.tags, migration_tag: s.migration_tag, has_assets: s.has_assets }))));
  return scripts.filter((s) => (s.tags ?? []).includes(TAG));
}

async function workersDevUrl(call, account, name) {
  const sub = (await call("GET", "/accounts/" + account + "/workers/subdomain")).body.result?.subdomain;
  return "https://" + name + "." + sub + ".workers.dev";
}

function walk(dir) {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}

const CONTENT_TYPES = { ".html": "text/html", ".js": "application/javascript", ".css": "text/css", ".json": "application/json", ".svg": "image/svg+xml", ".png": "image/png" };

async function uploadAssets(call, account, name, dir) {
  const files = walk(dir).filter((p) => !["_headers", "_redirects", ".assetsignore"].includes(relative(dir, p)));
  const manifest = {};
  const byHash = {};
  for (const p of files) {
    const content = readFileSync(p);
    const hash = createHash("sha256").update(content.toString("base64") + extname(p).slice(1)).digest("hex").slice(0, 32);
    manifest["/" + relative(dir, p)] = { hash, size: content.length };
    byHash[hash] = p;
  }
  const session = await call("POST", "/accounts/" + account + "/workers/scripts/" + name + "/assets-upload-session", { json: { manifest } });
  if (!session.ok) throw new Error("assets session failed");
  const { jwt, buckets } = session.body.result;
  log("assets: " + files.length + " files, " + buckets.flat().length + " to upload in " + buckets.length + " batches");
  let completion = jwt;
  for (const bucket of buckets) {
    const form = new FormData();
    for (const hash of bucket) {
      const p = byHash[hash];
      form.append(hash, new File([readFileSync(p).toString("base64")], hash, { type: CONTENT_TYPES[extname(p)] ?? "application/null" }));
    }
    const up = await fetch(API + "/accounts/" + account + "/workers/assets/upload?base64=true", { method: "POST", headers: { Authorization: "Bearer " + jwt }, body: form });
    const body = await up.json();
    log("assets upload batch -> HTTP " + up.status + " " + (body.success ? "" : JSON.stringify(body.errors)));
    if (body.result?.jwt) completion = body.result.jwt;
  }
  const config = {};
  for (const special of ["_headers", "_redirects"]) if (existsSync(join(dir, special))) config[special] = readFileSync(join(dir, special), "utf8");
  return { jwt: completion, config };
}

async function uploadWorker(call, account, { name, bucket, bundle, secret, existing, codeUpdate }) {
  const m = JSON.parse(readFileSync(join(bundle, "manifest.json"), "utf8"));
  const tags = m.migrations.map((x) => x.tag);
  let migrations;
  if (!existing?.migration_tag) migrations = { new_tag: tags.at(-1), steps: m.migrations.map(({ tag, ...rest }) => rest) };
  else if (existing.migration_tag !== tags.at(-1)) {
    const i = tags.indexOf(existing.migration_tag);
    if (i < 0) throw new Error("deployed migration tag " + existing.migration_tag + " is not in this release");
    migrations = { old_tag: existing.migration_tag, new_tag: tags.at(-1), steps: m.migrations.slice(i + 1).map(({ tag, ...rest }) => rest) };
  }
  const bindings = [
    ...m.durable_objects.map((d) => ({ type: "durable_object_namespace", name: d.name, class_name: d.class_name })),
    { type: "r2_bucket", name: m.r2_binding, bucket_name: bucket },
  ];
  const metadata = {
    main_module: m.main_module,
    compatibility_date: m.compatibility_date,
    bindings,
    tags: [TAG],
    keep_bindings: ["secret_text", "secret_key"],
    ...(migrations ? { migrations } : {}),
    ...(m.observability ? { observability: m.observability } : {}),
    ...(codeUpdate ? { code_update_strategy: { mode: codeUpdate } } : {}),
  };
  if (secret) bindings.push({ type: "secret_text", name: KEY_NAME, text: secret });
  if (m.assets) {
    metadata.assets = await uploadAssets(call, account, name, join(bundle, "assets"));
    bindings.push({ type: "assets", name: m.assets.binding });
  }
  log("upload metadata:", JSON.stringify({ ...metadata, assets: metadata.assets ? { config: Object.keys(metadata.assets.config) } : undefined, bindings: bindings.map((b) => ({ ...b, text: b.text ? "<secret>" : undefined })) }));
  const form = new FormData();
  form.set("metadata", JSON.stringify(metadata));
  form.set(m.main_module, new File([readFileSync(join(bundle, "worker", m.main_module))], m.main_module, { type: "application/javascript+module" }));
  const r = await call("PUT", "/accounts/" + account + "/workers/scripts/" + name, { form });
  if (!r.ok) throw new Error("upload failed: " + JSON.stringify(r.errors));
  log("uploaded:", JSON.stringify({ id: r.body.result?.id, migration_tag: r.body.result?.migration_tag, tags: r.body.result?.tags, has_assets: r.body.result?.has_assets, observability: r.body.result?.observability }));
}

async function serverInfo(url, { want, tries = 40 } = {}) {
  for (let i = 0; i < tries; i++) {
    try {
      const res = await fetch(url + "/v1/server", { headers: { Accept: "application/json" }, signal: AbortSignal.timeout(10_000) });
      if (res.ok) {
        const info = await res.json();
        if (!want || info.serverVersion === want) {
          log("GET /v1/server after " + i + " retries:", JSON.stringify(info));
          return info;
        }
      } else log("GET /v1/server -> HTTP " + res.status);
    } catch (e) {
      log("GET /v1/server failed:", e.cause?.code ?? e.message);
    }
    await sleep(3000);
  }
  throw new Error("server did not answer as expected");
}

async function claim(url, key, deviceName) {
  const res = await fetch(url + "/v1/claim", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ code: key, userName: "Prototype owner", deviceName, platform: "macos" }),
  });
  const body = await res.json().catch(() => ({}));
  log("POST /v1/claim (" + deviceName + ") -> HTTP " + res.status, res.ok ? JSON.stringify({ userId: body.userId, deviceId: body.deviceId }) : JSON.stringify(body));
  return res.ok ? body : undefined;
}

async function deleteSecret(call, account, name) {
  await call("DELETE", "/accounts/" + account + "/workers/scripts/" + name + "/secrets/" + KEY_NAME);
  const list = await call("GET", "/accounts/" + account + "/workers/scripts/" + name + "/secrets");
  log("secrets left:", JSON.stringify((list.body.result ?? []).map((s) => s.name)));
}

// flarebox setup after a Cloudflare login: deploy and claim, or add this device to the server it finds.
async function setup({ name, bucket, bundle, device, tries = 20, fresh = false }) {
  const call = cf();
  const account = await pickAccount(call);
  const r2 = await r2Status(call, account);
  log("R2:", JSON.stringify(r2));
  if (!r2.enabled) throw new Error("R2 is off; open " + r2.dashboard);

  const servers = fresh ? [] : await findServers(call, account);
  log("Flarebox servers found:", JSON.stringify(servers.map((s) => s.id)));
  const key = b64url(randomBytes(32));
  let url;
  if (servers.length === 0) {
    if (!r2.buckets.includes(bucket)) await call("POST", "/accounts/" + account + "/r2/buckets", { json: { name: bucket } });
    await uploadWorker(call, account, { name, bucket, bundle, secret: key });
    await call("POST", "/accounts/" + account + "/workers/scripts/" + name + "/subdomain", { json: { enabled: true, previews_enabled: false } });
    url = await workersDevUrl(call, account, name);
    await serverInfo(url);
  } else {
    name = servers[0].id;
    url = await workersDevUrl(call, account, name);
    const info = await serverInfo(url);
    log("existing server:", name, "claimed:", info.claimed);
    await call("PUT", "/accounts/" + account + "/workers/scripts/" + name + "/secrets", { json: { name: KEY_NAME, text: key, type: "secret_text" } });
  }
  let creds;
  for (let i = 0; i < Number(tries) && !creds; i++) {
    creds = await claim(url, key, device);
    if (!creds) await sleep(3000);
  }
  if (creds) saveSecret(join(STATE, "device-" + device + ".json"), { url, ...creds });
  await deleteSecret(call, account, name);
  if (!creds) throw new Error("claim with the one-time secret failed");
  await sleep(5000);
  const again = await claim(url, key, device + "-replay");
  log("replaying the used key was", again ? "ACCEPTED (bad)" : "refused (good)");
  await serverInfo(url);
}

async function upgrade({ bundle, want, name, newKey = false, codeUpdate }) {
  const call = cf();
  const account = await pickAccount(call);
  const found = (await findServers(call, account)).find((s) => !name || s.id === name);
  if (!found) throw new Error("no Flarebox server in this account");
  const settings = await call("GET", "/accounts/" + account + "/workers/scripts/" + found.id + "/settings");
  const bucket = settings.body.result?.bindings?.find((b) => b.type === "r2_bucket")?.bucket_name;
  log("current bindings:", JSON.stringify(settings.body.result?.bindings?.map((b) => b.type + ":" + b.name + (b.bucket_name ? "=" + b.bucket_name : ""))), "observability:", JSON.stringify(settings.body.result?.observability));
  let secret;
  if (newKey) {
    secret = b64url(randomBytes(32));
    saveSecret(join(STATE, "key-" + found.id + ".json"), { key: secret });
  }
  await uploadWorker(call, account, { name: found.id, bucket, bundle, existing: found, secret, codeUpdate });
  const url = await workersDevUrl(call, account, found.id);
  await serverInfo(url, { want });
  const after = await call("GET", "/accounts/" + account + "/workers/scripts/" + found.id + "/settings");
  log("after upgrade observability:", JSON.stringify(after.body.result?.observability), "bindings:", JSON.stringify(after.body.result?.bindings?.map((b) => b.type + ":" + b.name)));
}

async function claimOnly({ name, device, tries = 20, reuseKey = false, keep = false }) {
  const call = cf();
  const account = await pickAccount(call);
  const url = await workersDevUrl(call, account, name);
  let key;
  if (reuseKey) key = load(join(STATE, "key-" + name + ".json")).key;
  else {
    key = b64url(randomBytes(32));
    await call("PUT", "/accounts/" + account + "/workers/scripts/" + name + "/secrets", { json: { name: KEY_NAME, text: key, type: "secret_text" } });
  }
  const start = Date.now();
  let creds;
  for (let i = 0; i < Number(tries) && !creds; i++) {
    creds = await claim(url, key, device);
    if (!creds) await sleep(3000);
  }
  log(creds ? "claimed " + (Date.now() - start) / 1000 + " s after the secret was set" : "never claimed in " + (Date.now() - start) / 1000 + " s");
  if (creds) saveSecret(join(STATE, "device-" + device + ".json"), { url, ...creds });
  if (!keep) await deleteSecret(call, account, name);
}

async function device({ device, create }) {
  const d = load(join(STATE, "device-" + device + ".json"));
  const tok = await fetch(d.url + "/v1/token", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ deviceId: d.deviceId, refreshCredential: d.refreshCredential }) });
  const access = await tok.json();
  log("POST /v1/token (" + device + ") -> HTTP " + tok.status);
  if (!tok.ok) return;
  const auth = { Authorization: "Bearer " + access.token, "Content-Type": "application/json" };
  if (create) {
    const res = await fetch(d.url + "/v1/spaces", { method: "POST", headers: auth, body: JSON.stringify({ name: create }) });
    log("POST /v1/spaces -> HTTP " + res.status, JSON.stringify(await res.json()));
  }
  const list = await fetch(d.url + "/v1/spaces", { headers: auth });
  log("GET /v1/spaces (" + device + ") -> HTTP " + list.status, JSON.stringify(await list.json()));
}

async function teardown({ name, bucket }) {
  const call = cf();
  const account = await pickAccount(call);
  await call("DELETE", "/accounts/" + account + "/workers/scripts/" + name + "?force=true");
  await call("DELETE", "/accounts/" + account + "/r2/buckets/" + bucket);
  const ns = await call("GET", "/accounts/" + account + "/workers/durable_objects/namespaces");
  log("DO namespaces left:", JSON.stringify((ns.body.result ?? []).map((n) => ({ id: n.id, name: n.name, script: n.script, class: n.class }))));
  await findServers(call, account);
  log("R2:", JSON.stringify(await r2Status(call, account)));
}

async function emptyAndDelete({ bucket }) {
  const call = cf();
  const account = await pickAccount(call);
  const list = await call("GET", "/accounts/" + account + "/r2/buckets/" + bucket + "/objects?per_page=1000");
  const keys = (list.body.result ?? []).map((o) => o.key);
  log("objects in " + bucket + ":", JSON.stringify(keys.map((k) => k.replace(/[0-9a-f]{8,}/g, "<hex>"))), "truncated:", JSON.stringify(list.body.result_info?.is_truncated ?? list.body.result_info?.cursor ?? null));
  for (const key of keys) await call("DELETE", "/accounts/" + account + "/r2/buckets/" + bucket + "/objects/" + encodeURIComponent(key));
  await call("DELETE", "/accounts/" + account + "/r2/buckets/" + bucket);
  log("R2:", JSON.stringify(await r2Status(call, account)));
  const ns = await call("GET", "/accounts/" + account + "/workers/durable_objects/namespaces");
  log("DO namespaces:", JSON.stringify(ns.body.result ?? []));
  await findServers(call, account);
}

async function probe({ file = TOKENS, name }) {
  const call = cf(file);
  const account = TEST_ACCOUNT;
  await call("GET", "/accounts");
  await call("GET", "/user");
  await call("GET", "/accounts/" + account + "/r2/buckets");
  await call("GET", "/accounts/" + account + "/workers/scripts");
  await call("GET", "/accounts/" + account + "/workers/subdomain");
  if (name) {
    await call("GET", "/accounts/" + account + "/workers/scripts/" + name + "/settings");
    await call("GET", "/accounts/" + account + "/workers/scripts/" + name + "/secrets");
  }
  const scopes = await call("GET", "/oauth/scopes");
  const wanted = /account-settings|user-details|workers-scripts|r2|observability|workers-tail/;
  log("scope ids:", JSON.stringify((scopes.body.result ?? []).filter((s) => wanted.test(s.id ?? s.name ?? "")).map((s) => s.id + " = " + s.name)));
}

const [cmd, ...rest] = process.argv.slice(2);
const opts = Object.fromEntries(rest.map((a) => a.replace(/^--/, "").split("=")).map(([k, v]) => [k, v ?? true]));
if (opts.scopes) opts.scopes = opts.scopes.split(",");
if (opts.file) opts.file = join(STATE, opts.file);
if (opts.out) opts.out = join(STATE, opts.out);
const commands = { login, refresh, revoke, setup, upgrade, claimOnly, device, teardown, emptyAndDelete, probe };
if (!commands[cmd]) {
  console.error("usage: fbsetup.mjs " + Object.keys(commands).join("|") + " [--key=value ...]");
  process.exit(2);
}
await commands[cmd](opts).catch((e) => {
  log("FAILED:", e.message);
  process.exitCode = 1;
});
