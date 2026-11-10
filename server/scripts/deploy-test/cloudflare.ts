// SPDX-License-Identifier: AGPL-3.0-or-later

// The Cloudflare API calls of the deploy test (docs/deploy-test.md), made with the scoped token from
// CLOUDFLARE_API_TOKEN. Messages name the method and path only, never a header, a body or a credential.

const API = "https://api.cloudflare.com/client/v4";
const ATTEMPTS = 4;
const REQUEST_MS = 60_000;

export interface Account {
  id: string;
  token: string;
}

export interface R2Object {
  key: string;
  size: number;
}

export interface Namespace {
  id: string;
  class: string;
  script: string;
}

export class ApiError extends Error {
  readonly status: number;
  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

/** The account and token from the environment, or undefined when either is missing. */
export function accountFromEnv(): Account | undefined {
  const id = process.env.CLOUDFLARE_ACCOUNT_ID ?? "";
  const token = process.env.CLOUDFLARE_API_TOKEN ?? "";
  return id && token ? { id, token } : undefined;
}

export function requireAccount(): Account {
  const account = accountFromEnv();
  if (!account) throw new Error("CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID must be set; see docs/deploy-test.md");
  return account;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** An R2 object key as a URL path: slashes stay literal, every segment is percent-encoded. */
function keyPath(key: string): string {
  return key.split("/").map(encodeURIComponent).join("/");
}

/**
 * One API request, retried on network errors, 429 and 5xx. Returns the response for a 2xx status, or undefined for a 404
 * when allowMissing is set, and throws ApiError otherwise with Cloudflare's error codes and messages.
 */
async function send(account: Account, method: string, path: string, init: { body?: unknown; allowMissing?: boolean } = {}): Promise<Response | undefined> {
  const what = method + " " + path.split("?")[0];
  for (let attempt = 1; ; attempt++) {
    let response: Response;
    try {
      response = await fetch(API + path, {
        method,
        headers: { Authorization: "Bearer " + account.token, ...(init.body === undefined ? {} : { "Content-Type": "application/json" }) },
        body: init.body === undefined ? undefined : JSON.stringify(init.body),
        signal: AbortSignal.timeout(REQUEST_MS),
      });
    } catch (error) {
      if (attempt >= ATTEMPTS) throw new ApiError(what + " failed: " + (error instanceof Error ? error.message : String(error)), 0);
      await sleep(2000 * attempt);
      continue;
    }
    if (response.ok) return response;
    if (response.status === 404 && init.allowMissing) {
      await response.body?.cancel();
      return undefined;
    }
    if ((response.status === 429 || response.status >= 500) && attempt < ATTEMPTS) {
      await response.body?.cancel();
      await sleep(2000 * attempt);
      continue;
    }
    const text = await response.text();
    let detail = text.slice(0, 300);
    try {
      const errors = (JSON.parse(text) as { errors?: { code: number; message: string }[] }).errors ?? [];
      if (errors.length > 0) detail = errors.map((error) => error.code + " " + error.message).join("; ");
    } catch {}
    const hint = response.status === 403 ? " (the token lacks a permission this call needs; see docs/deploy-test.md)" : "";
    throw new ApiError(what + " returned HTTP " + response.status + ": " + detail + hint, response.status);
  }
}

interface Envelope<T> {
  result: T;
  result_info?: { cursor?: string; is_truncated?: boolean; page?: number; total_pages?: number };
}

async function call<T>(account: Account, method: string, path: string, body?: unknown): Promise<Envelope<T>> {
  const response = await send(account, method, path, { body });
  return (await response!.json()) as Envelope<T>;
}

export async function createBucket(account: Account, name: string): Promise<void> {
  await call(account, "POST", `/accounts/${account.id}/r2/buckets`, { name });
}

/** Every object in a bucket under a prefix, or undefined when the bucket does not exist. */
export async function listObjects(account: Account, bucket: string, prefix = ""): Promise<R2Object[] | undefined> {
  const objects: R2Object[] = [];
  let cursor = "";
  for (;;) {
    const query = new URLSearchParams({ per_page: "1000", ...(prefix ? { prefix } : {}), ...(cursor ? { cursor } : {}) });
    const response = await send(account, "GET", `/accounts/${account.id}/r2/buckets/${bucket}/objects?${query}`, { allowMissing: true });
    if (!response) return undefined;
    const page = (await response.json()) as Envelope<{ key: string; size: number }[]>;
    for (const object of page.result ?? []) objects.push({ key: object.key, size: Number(object.size) });
    cursor = page.result_info?.cursor ?? "";
    if (!page.result_info?.is_truncated || !cursor) return objects;
  }
}

/** An object's bytes, or undefined when it does not exist. */
export async function getObject(account: Account, bucket: string, key: string): Promise<Uint8Array | undefined> {
  const response = await send(account, "GET", `/accounts/${account.id}/r2/buckets/${bucket}/objects/${keyPath(key)}`, { allowMissing: true });
  return response ? new Uint8Array(await response.arrayBuffer()) : undefined;
}

export async function deleteObject(account: Account, bucket: string, key: string): Promise<void> {
  await send(account, "DELETE", `/accounts/${account.id}/r2/buckets/${bucket}/objects/${keyPath(key)}`, { allowMissing: true });
}

/** Deletes an empty bucket. Returns false when it did not exist. */
export async function deleteBucket(account: Account, bucket: string): Promise<boolean> {
  return (await send(account, "DELETE", `/accounts/${account.id}/r2/buckets/${bucket}`, { allowMissing: true })) !== undefined;
}

/**
 * Deletes a Worker. With force, Cloudflare also deletes the Durable Object namespaces it implements. Returns false when
 * it did not exist.
 */
export async function deleteWorker(account: Account, name: string): Promise<boolean> {
  return (await send(account, "DELETE", `/accounts/${account.id}/workers/scripts/${name}?force=true`, { allowMissing: true })) !== undefined;
}

/** The Durable Object namespaces a Worker implements. */
export async function namespacesOf(account: Account, script: string): Promise<Namespace[]> {
  const found: Namespace[] = [];
  for (let page = 1; ; page++) {
    const listed = await call<Namespace[]>(account, "GET", `/accounts/${account.id}/workers/durable_objects/namespaces?per_page=1000&page=${page}`);
    found.push(...(listed.result ?? []).filter((namespace) => namespace.script === script));
    if (!listed.result_info?.total_pages || page >= listed.result_info.total_pages) return found;
  }
}

/** The version a Worker's newest deployment sends all traffic to, or undefined during a split deployment. */
export async function activeVersion(account: Account, name: string): Promise<string | undefined> {
  const listed = await call<{ deployments: { created_on: string; versions: { version_id: string; percentage: number }[] }[] }>(
    account,
    "GET",
    `/accounts/${account.id}/workers/scripts/${name}/deployments`,
  );
  const newest = [...(listed.result?.deployments ?? [])].sort((a, b) => b.created_on.localeCompare(a.created_on))[0];
  return newest?.versions.find((version) => version.percentage === 100)?.version_id;
}

/** A GraphQL Analytics API query; throws with the API's error messages. */
export async function graphql<T>(account: Account, query: string, variables: Record<string, unknown>): Promise<T> {
  const response = await call<never>(account, "POST", "/graphql", { query, variables });
  const body = response as unknown as { data?: T; errors?: { message: string }[] | null };
  if (body.errors?.length) throw new ApiError("GraphQL: " + body.errors.map((error) => error.message).join("; "), 200);
  return body.data as T;
}
