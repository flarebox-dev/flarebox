// SPDX-License-Identifier: AGPL-3.0-or-later

import { CASE_FOLDING } from "./casefold-data";

const encoder = new TextEncoder();
const MAX_NAME_BYTES = 255;
const MAX_DEVICE_NAME_CHARS = 32;
const MAX_DEVICE_NAME_BYTES = 128;

let folding: Map<number, string> | undefined;

function foldingTable(): Map<number, string> {
  if (folding) return folding;
  folding = new Map();
  for (const entry of CASE_FOLDING.split(",")) {
    const [code, mapping] = entry.split(":") as [string, string];
    folding.set(parseInt(code, 16), String.fromCodePoint(...mapping.split(" ").map((cp) => parseInt(cp, 16))));
  }
  return folding;
}

export function casefold(text: string): string {
  const table = foldingTable();
  let out = "";
  for (const char of text) out += table.get(char.codePointAt(0)!) ?? char;
  return out;
}

export function nameKey(name: string): string {
  return casefold(name.normalize("NFD")).normalize("NFD");
}

export function isReservedAtRoot(name: string): boolean {
  return nameKey(name) === ".koidrive";
}

export function normalizeName(name: string): string | undefined {
  const nfc = name.normalize("NFC");
  const bytes = encoder.encode(nfc).length;
  if (bytes < 1 || bytes > MAX_NAME_BYTES) return undefined;
  if (nfc.includes("/") || nfc.includes("\0") || nfc === "." || nfc === "..") return undefined;
  return nfc;
}

export function conflictName(name: string, deviceName: string, atMs: number, attempt: number): string {
  const dot = name.lastIndexOf(".");
  const stem = dot > 0 ? name.slice(0, dot) : name;
  const ext = dot > 0 ? name.slice(dot) : "";
  const deviceChars = [...deviceName.replaceAll("/", "").replaceAll("\0", "")].slice(0, MAX_DEVICE_NAME_CHARS);
  while (encoder.encode(deviceChars.join("").normalize("NFC")).length > MAX_DEVICE_NAME_BYTES) deviceChars.pop();
  const device = deviceChars.join("") || "unknown device";
  const date = new Date(atMs).toISOString().slice(0, 10);
  const suffix = ` (conflict from ${device} ${date}${attempt > 1 ? ` ${attempt}` : ""})${ext}`;
  const fits = (text: string) => encoder.encode(text.normalize("NFC")).length <= MAX_NAME_BYTES;
  const stemChars = [...stem];
  while (stemChars.length > 0 && !fits(stemChars.join("") + suffix)) stemChars.pop();
  const chars = [...(stemChars.join("") + suffix)];
  while (!fits(chars.join(""))) chars.pop();
  return chars.join("").normalize("NFC");
}
