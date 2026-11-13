// SPDX-License-Identifier: AGPL-3.0-or-later

export function errorText(error: unknown): string {
  return error instanceof Error ? error.name + ": " + error.message : String(error);
}
