// SPDX-License-Identifier: AGPL-3.0-or-later

type NodeKind = "NODE_KIND_UNSPECIFIED" | "NODE_KIND_DIR" | "NODE_KIND_FILE" | "NODE_KIND_SYMLINK";

/** The proto3 JSON forms: 64-bit numbers arrive as strings, defaults are omitted. */
interface ShareInfo {
  passwordRequired?: boolean;
  kind?: NodeKind;
  name?: string;
  rootId?: string;
  size?: string;
  mtimeMs?: string;
  expiresAtMs?: string;
}

interface ShareEntry {
  nodeId?: string;
  name?: string;
  kind?: NodeKind;
  size?: string;
  mtimeMs?: string;
}

interface ShareListing {
  entries?: ShareEntry[];
  path?: ShareEntry[];
  nextCursor?: string;
}

interface ApiError {
  retryAfterMs?: string;
}

interface Reply<T> {
  status: number;
  body: T | null;
}

type Props = Record<string, string | ((event: Event) => unknown)>;

const linkId = location.pathname.split("/")[2] ?? "";
const api = "/v1/share/" + encodeURIComponent(linkId);
const main = document.getElementById("main")!;

function el<K extends keyof HTMLElementTagNameMap>(tag: K, props: Props = {}, ...children: (Node | string)[]): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (typeof value === "function") node.addEventListener(key.slice(2), value);
    else if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else node.setAttribute(key, value);
  }
  node.append(...children);
  return node;
}

function show(...nodes: Node[]): void {
  main.replaceChildren(...nodes);
}

function formatSize(text: string | undefined): string {
  let size = Number(text ?? 0);
  const units = ["bytes", "KB", "MB", "GB", "TB"];
  let unit = 0;
  while (size >= 1000 && unit < units.length - 1) {
    size /= 1000;
    unit++;
  }
  return unit === 0 ? size + (size === 1 ? " byte" : " bytes") : size.toFixed(size < 10 ? 1 : 0) + " " + units[unit];
}

function formatTime(text: string | number | undefined): string {
  const ms = Number(text ?? 0);
  if (!ms) return "";
  return new Date(ms).toLocaleString("en-US", { dateStyle: "medium", timeStyle: "short" });
}

function waitText(error: ApiError | null): string {
  const ms = Number(error?.retryAfterMs ?? 0);
  if (!ms) return "Try again later.";
  const minutes = Math.ceil(ms / 60_000);
  return "Try again in " + minutes + (minutes === 1 ? " minute." : " minutes.");
}

function problem(status: number, error: ApiError | null): string {
  if (status === 404) return "This link does not exist, has expired, or was revoked.";
  if (status === 429) return "Too many attempts. " + waitText(error);
  if (status === 503) return "This link is temporarily unavailable. " + waitText(error);
  if (status === 401) return "This link needs its password.";
  return "Something went wrong. " + waitText(error);
}

async function request<T>(path: string, init: { method?: string; body?: string } = {}): Promise<Reply<T>> {
  let response: Response;
  try {
    response = await fetch(api + path, {
      ...init,
      credentials: "same-origin",
      headers: { Accept: "application/json", ...(init.body ? { "Content-Type": "application/json" } : {}) },
    });
  } catch {
    return { status: 0, body: null };
  }
  let body: T | null = null;
  if (response.status !== 204) {
    try {
      body = (await response.json()) as T;
    } catch {
      body = null;
    }
  }
  return { status: response.status, body };
}

function fail(status: number, error: ApiError | null): void {
  show(el("h1", { text: "Unavailable" }), el("p", { class: "error", text: problem(status, error) }));
}

function downloadLink(nodeId: string, label = "Download"): HTMLAnchorElement {
  return el("a", { class: "button", href: api + "/file/" + encodeURIComponent(nodeId), download: "", text: label });
}

function passwordForm(message?: string): void {
  const input = el("input", { type: "password", name: "password", autocomplete: "off", required: "", "aria-label": "Password" });
  const note = el("p", { class: message ? "error" : "status", text: message ?? "Enter the password you were given." });
  const button = el("button", { type: "submit", text: "Open" });
  let pending = false;
  const form = el(
    "form",
    {
      onsubmit: async (event: Event) => {
        event.preventDefault();
        if (pending) return;
        pending = true;
        button.disabled = true;
        const { status, body } = await request<ApiError>("/unlock", { method: "POST", body: JSON.stringify({ password: input.value }) });
        if (status === 204) return start();
        if (status === 401) return passwordForm("Wrong password.");
        if (status === 429) return passwordForm("Too many wrong passwords. " + waitText(body));
        fail(status, body);
      },
    },
    input,
    button,
  );
  show(el("h1", { text: "This link is protected" }), note, form);
  input.focus();
}

function expiry(info: ShareInfo): string {
  const ms = Number(info.expiresAtMs ?? 0);
  return ms ? "Link expires " + formatTime(ms) : "";
}

function showFile(info: ShareInfo): void {
  const details = [formatSize(info.size), formatTime(info.mtimeMs) && "modified " + formatTime(info.mtimeMs), expiry(info)].filter(Boolean).join(" · ");
  show(el("h1", { text: info.name ?? "" }), el("p", { class: "meta", text: details }), downloadLink(info.rootId ?? ""));
}

function currentDir(info: ShareInfo): string {
  const match = /^#dir=([0-9a-f-]{36})$/.exec(location.hash);
  return match ? match[1]! : (info.rootId ?? "");
}

function entryRow(entry: ShareEntry): HTMLTableRowElement {
  const isDir = entry.kind === "NODE_KIND_DIR";
  const nodeId = entry.nodeId ?? "";
  const name = isDir
    ? el("button", { type: "button", text: entry.name ?? "", onclick: () => (location.hash = "dir=" + nodeId) })
    : el("span", { text: entry.name ?? "" });
  return el(
    "tr",
    {},
    el("td", { class: "name" }, el("span", { class: "kind", text: isDir ? "Folder" : "File" }), name),
    el("td", { class: "num hide-small", text: isDir ? "" : formatSize(entry.size) }),
    el("td", { class: "num hide-small", text: isDir ? "" : formatTime(entry.mtimeMs) }),
    el("td", { class: "num" }, isDir ? "" : downloadLink(nodeId)),
  );
}

async function showFolder(info: ShareInfo): Promise<void> {
  const dir = currentDir(info);
  const rows = el("tbody");
  const crumbs = el("nav", { class: "crumbs", "aria-label": "Folder path" });
  const more = el("div", { class: "more" });
  const heading = el("h1", { text: info.name ?? "" });
  show(
    heading,
    el("p", { class: "meta", text: expiry(info) }),
    crumbs,
    el(
      "table",
      {},
      el("thead", {}, el("tr", {}, el("th", { text: "Name" }), el("th", { class: "num hide-small", text: "Size" }), el("th", { class: "num hide-small", text: "Modified" }), el("th"))),
      rows,
    ),
    more,
  );
  let first = true;
  const load = async (cursor: string): Promise<void> => {
    const query = "?dir=" + encodeURIComponent(dir) + (cursor ? "&cursor=" + encodeURIComponent(cursor) : "");
    const { status, body } = await request<ShareListing & ApiError>("/list" + query);
    if (!heading.isConnected) return;
    if (status === 401) return passwordForm();
    if (status !== 200 || !body) return fail(status, body);
    if (first) {
      first = false;
      const path = body.path ?? [];
      heading.textContent = path.at(-1)?.name ?? info.name ?? "";
      path.forEach((folder, i) => {
        if (i > 0) crumbs.append(el("span", { text: "/" }));
        crumbs.append(
          el("button", { type: "button", text: folder.name || "Shared folder", onclick: () => (location.hash = i === 0 ? "" : "dir=" + (folder.nodeId ?? "")) }),
        );
      });
    }
    for (const entry of body.entries ?? []) rows.append(entryRow(entry));
    if (rows.children.length === 0) rows.append(el("tr", {}, el("td", { class: "status", colspan: "4", text: "This folder is empty." })));
    more.replaceChildren();
    const next = body.nextCursor;
    if (next) more.append(el("button", { type: "button", class: "secondary", text: "Show more", onclick: () => load(next) }));
  };
  await load("");
}

let info: ShareInfo | undefined;

async function start(): Promise<void> {
  const { status, body } = await request<ShareInfo & ApiError>("");
  if (status === 401 && body?.passwordRequired) return passwordForm();
  if (status !== 200 || !body) return fail(status, body);
  info = body;
  document.title = (info.name ?? "") + " · Koidrive";
  if (info.kind === "NODE_KIND_DIR") return showFolder(info);
  showFile(info);
}

window.addEventListener("hashchange", () => {
  if (info?.kind === "NODE_KIND_DIR") void showFolder(info);
});

void start();
