// SPDX-License-Identifier: AGPL-3.0-or-later

import type { Sql } from "./host";

export type Migration = string | ((sql: Sql) => void);

export function migrate(sql: Sql, versionTable: string, steps: readonly Migration[]): void {
  const hasTable = sql.exec("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?", versionTable).toArray().length > 0;
  let version = hasTable
    ? Number(sql.exec(`SELECT value FROM ${versionTable} WHERE key = 'schema_version'`).toArray()[0]?.value ?? 0)
    : 0;
  if (version > steps.length) {
    throw new Error(
      `storage schema version ${version} is newer than this server knows (${steps.length}); deploy the release that wrote it or a later one`,
    );
  }
  while (version < steps.length) {
    const step = steps[version]!;
    if (typeof step === "string") sql.exec(step);
    else step(sql);
    version++;
    sql.exec(
      `INSERT INTO ${versionTable} (key, value) VALUES ('schema_version', ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value`,
      version,
    );
  }
}
