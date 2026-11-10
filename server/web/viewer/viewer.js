// SPDX-License-Identifier: AGPL-3.0-or-later

// The share-link viewer of RFC 0005 section 8. It talks only to this origin and renders every name with textContent.

const linkId = location.pathname.split("/")[2] ?? "";
const api = "/v1/share/" + encodeURIComponent(linkId);
const main = document.getElementById("main");

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value);
  }
  for (const child of children) node.append(child);
  return node;
}

function show(...nodes) {
  main.replaceChildren(...nodes);
}

function formatSize(text) {
  let size = Number(text ?? 0);
  const units = ["bytes", "KB", "MB", "GB", "TB"];
  let unit = 0;
  while (size >= 1000 && unit < units.length - 1) {
    size /= 1000;
    unit++;
  }
  return unit === 0 ? size + (size === 1 ? " byte" : " bytes") : size.toFixed(size < 10 ? 1 : 0) + " " + units[unit];
}

function formatTime(text) {
  const ms = Number(text ?? 0);
  if (!ms) return "";
  return new Date(ms).toLocaleString("en-US", { dateStyle: "medium", timeStyle: "short" });
}

function waitText(error) {
  const ms = Number(error?.retryAfterMs ?? 0);
  if (!ms) return "Try again later.";
  const minutes = Math.ceil(ms / 60_000);
  return "Try again in " + minutes + (minutes === 1 ? " minute." : " minutes.");
}

function problem(status, error) {
  if (status === 404) return "This link does not exist, has expired, or was revoked.";
  if (status === 429) return "Too many attempts. " + waitText(error);
  if (status === 503) return "This link is temporarily unavailable. " + waitText(error);
  if (status === 401) return "This link needs its password.";
  return "Something went wrong. " + waitText(error);
}

async function request(path, init = {}) {
  let response;
  try {
    response = await fetch(api + path, {
      ...init,
      credentials: "same-origin",
      headers: { Accept: "application/json", ...(init.body ? { "Content-Type": "application/json" } : {}) },
    });
  } catch {
    return { status: 0, body: null };
  }
  let body = null;
  if (response.status !== 204) {
    try {
      body = await response.json();
    } catch {
      body = null;
    }
  }
  return { status: response.status, body };
}

function fail(status, error) {
  show(el("h1", { text: "Unavailable" }), el("p", { class: "error", text: problem(status, error) }));
}

function downloadLink(nodeId, label = "Download") {
  return el("a", { class: "button", href: api + "/file/" + encodeURIComponent(nodeId), download: "", text: label });
}

function passwordForm(message) {
  const input = el("input", { type: "password", name: "password", autocomplete: "off", required: "", "aria-label": "Password" });
  const note = el("p", { class: message ? "error" : "status", text: message ?? "Enter the password you were given." });
  const form = el(
    "form",
    {
      onsubmit: async (event) => {
        event.preventDefault();
        const { status, body } = await request("/unlock", { method: "POST", body: JSON.stringify({ password: input.value }) });
        if (status === 204) return start();
        if (status === 401) return passwordForm("Wrong password.");
        if (status === 429) return passwordForm("Too many wrong passwords. " + waitText(body));
        fail(status, body);
      },
    },
    input,
    el("button", { type: "submit", text: "Open" }),
  );
  show(el("h1", { text: "This link is protected" }), note, form);
  input.focus();
}

function expiry(info) {
  const ms = Number(info.expiresAtMs ?? 0);
  return ms ? "Link expires " + formatTime(ms) : "";
}

function showFile(info) {
  const details = [formatSize(info.size), formatTime(info.mtimeMs) && "modified " + formatTime(info.mtimeMs), expiry(info)].filter(Boolean).join(" · ");
  show(el("h1", { text: info.name }), el("p", { class: "meta", text: details }), downloadLink(info.rootId));
}

function currentDir(info) {
  const match = /^#dir=([0-9a-f-]{36})$/.exec(location.hash);
  return match ? match[1] : info.rootId;
}

function entryRow(entry) {
  const isDir = entry.kind === "NODE_KIND_DIR";
  const name = isDir
    ? el("button", { type: "button", text: entry.name, onclick: () => (location.hash = "dir=" + entry.nodeId) })
    : el("span", { text: entry.name });
  return el(
    "tr",
    {},
    el("td", { class: "name" }, el("span", { class: "kind", text: isDir ? "Folder" : "File" }), name),
    el("td", { class: "num hide-small", text: isDir ? "" : formatSize(entry.size) }),
    el("td", { class: "num hide-small", text: isDir ? "" : formatTime(entry.mtimeMs) }),
    el("td", { class: "num" }, isDir ? "" : downloadLink(entry.nodeId)),
  );
}

async function showFolder(info) {
  const dir = currentDir(info);
  const rows = el("tbody");
  const crumbs = el("nav", { class: "crumbs", "aria-label": "Folder path" });
  const more = el("div", { class: "more" });
  const heading = el("h1", { text: info.name });
  show(
    heading,
    el("p", { class: "meta", text: expiry(info) }),
    crumbs,
    el(
      "table",
      {},
      el(
        "thead",
        {},
        el("tr", {}, el("th", { text: "Name" }), el("th", { class: "num hide-small", text: "Size" }), el("th", { class: "num hide-small", text: "Modified" }), el("th", {})),
      ),
      rows,
    ),
    more,
  );
  let first = true;
  const load = async (cursor) => {
    const query = "?dir=" + encodeURIComponent(dir) + (cursor ? "&cursor=" + encodeURIComponent(cursor) : "");
    const { status, body } = await request("/list" + query);
    if (status === 401) return passwordForm();
    if (status !== 200) return fail(status, body);
    if (first) {
      first = false;
      const path = body.path ?? [];
      heading.textContent = path.at(-1)?.name ?? info.name;
      path.forEach((folder, i) => {
        if (i > 0) crumbs.append(el("span", { text: "/" }));
        crumbs.append(el("button", { type: "button", text: folder.name || "Shared folder", onclick: () => (location.hash = i === 0 ? "" : "dir=" + folder.nodeId) }));
      });
    }
    for (const entry of body.entries ?? []) rows.append(entryRow(entry));
    if (rows.children.length === 0) rows.append(el("tr", {}, el("td", { class: "status", colspan: "4", text: "This folder is empty." })));
    more.replaceChildren();
    if (body.nextCursor) more.append(el("button", { type: "button", class: "secondary", text: "Show more", onclick: () => load(body.nextCursor) }));
  };
  await load("");
}

let info;

async function start() {
  const { status, body } = await request("");
  if (status === 401 && body?.passwordRequired) return passwordForm();
  if (status !== 200) return fail(status, body);
  info = body;
  document.title = info.name + " · Flarebox";
  if (info.kind === "NODE_KIND_DIR") return showFolder(info);
  showFile(info);
}

window.addEventListener("hashchange", () => {
  if (info?.kind === "NODE_KIND_DIR") showFolder(info);
});

start();
