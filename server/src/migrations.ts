// SPDX-License-Identifier: AGPL-3.0-or-later

/** One forward-only schema step. A step must leave a database that already has its effect unchanged. */
export type Migration = string | ((sql: SqlStorage) => void);

/**
 * Brings a Durable Object's SQLite schema to the newest version. The version is the number of steps applied, kept under
 * the key 'schema_version' of a key-value table that the first step creates. Callers run this inside transactionSync,
 * so a failed step leaves the previous version in place. Storage written by a newer release is refused: rolling back
 * the code after a migration ran is not supported, because older code would misread the newer schema.
 */
export function migrate(sql: SqlStorage, versionTable: string, steps: readonly Migration[]): void {
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
