// SPDX-License-Identifier: AGPL-3.0-or-later

import { type Migration, migrate as runMigrations } from "../migrations";

// Account DO storage, following spec v0 section 4.2. Migrations only move forward.

/**
 * Append-only: never edit or reorder a step that has been merged. The first two steps are the unversioned schemas of
 * #8 and #25, kept with IF NOT EXISTS so that storage those builds wrote migrates in place.
 */
export const MIGRATIONS: readonly Migration[] = [
  `
  CREATE TABLE IF NOT EXISTS users (user_id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at_ms INTEGER NOT NULL);
  CREATE TABLE IF NOT EXISTS devices (device_id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL,
    platform TEXT NOT NULL, created_at_ms INTEGER NOT NULL, last_seen_ms INTEGER, refresh_hash BLOB NOT NULL,
    revoked_at_ms INTEGER);
  CREATE TABLE IF NOT EXISTS spaces (space_id TEXT PRIMARY KEY, name TEXT NOT NULL, owner_id TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL, format_version INTEGER NOT NULL);
  CREATE TABLE IF NOT EXISTS share_links (link_id TEXT PRIMARY KEY, space_id TEXT NOT NULL, node_id TEXT NOT NULL,
    created_by TEXT NOT NULL, created_at_ms INTEGER NOT NULL, expires_at_ms INTEGER, password_kdf TEXT,
    password_hash BLOB, password_salt BLOB, password_iterations INTEGER, revoked_at_ms INTEGER,
    failed_attempts INTEGER NOT NULL DEFAULT 0, locked_until_ms INTEGER);
  CREATE TABLE IF NOT EXISTS pairing_codes (code_hash BLOB PRIMARY KEY, device_id TEXT NOT NULL,
    expires_at_ms INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0);
  CREATE TABLE IF NOT EXISTS claim_codes (code_hash BLOB PRIMARY KEY, expires_at_ms INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0);
  CREATE TABLE IF NOT EXISTS server_keys (kid TEXT PRIMARY KEY, secret BLOB NOT NULL, created_at_ms INTEGER NOT NULL,
    status TEXT NOT NULL);
  CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
  `,
  `
  CREATE TABLE IF NOT EXISTS chunk_retirements (space_id TEXT NOT NULL, hash_hex TEXT NOT NULL, incarnation TEXT NOT NULL,
    retired_at_ms INTEGER NOT NULL, PRIMARY KEY (space_id, hash_hex, incarnation));
  CREATE TABLE IF NOT EXISTS rewinds (rewind_id TEXT PRIMARY KEY, space_id TEXT NOT NULL, restore_point_ms INTEGER NOT NULL,
    expected_history_id TEXT NOT NULL, bookmark TEXT NOT NULL, history_id TEXT NOT NULL, rewind_number INTEGER NOT NULL,
    previous_history_id TEXT, previous_head_seq INTEGER, attempt INTEGER NOT NULL, phase TEXT NOT NULL, result BLOB,
    error_code INTEGER, error TEXT, admitted_at_ms INTEGER NOT NULL, ended_at_ms INTEGER, tries INTEGER NOT NULL DEFAULT 0,
    next_try_ms INTEGER);
  CREATE INDEX IF NOT EXISTS rewinds_space ON rewinds(space_id);
  CREATE TABLE IF NOT EXISTS space_rewinds (space_id TEXT PRIMARY KEY, rewind_count INTEGER NOT NULL, completed_at_ms INTEGER);
  `,
  `
  CREATE TABLE IF NOT EXISTS claim_failures (scope TEXT NOT NULL, client TEXT NOT NULL, failures INTEGER NOT NULL,
    window_start_ms INTEGER NOT NULL, PRIMARY KEY (scope, client));
  CREATE TABLE IF NOT EXISTS used_recovery_keys (key_hash BLOB PRIMARY KEY, used_at_ms INTEGER NOT NULL);
  `,
  // RFC 0006 section 2: every device is a principal. Existing rows become full-scope devices that never expire.
  (sql) => {
    const columns = sql.exec("SELECT name FROM pragma_table_info('devices')").toArray().map((row) => row.name);
    if (!columns.includes("kind")) sql.exec("ALTER TABLE devices ADD COLUMN kind TEXT NOT NULL DEFAULT 'device'");
    if (!columns.includes("scope")) sql.exec("ALTER TABLE devices ADD COLUMN scope TEXT");
    if (!columns.includes("expires_at_ms")) sql.exec("ALTER TABLE devices ADD COLUMN expires_at_ms INTEGER");
    if (!columns.includes("issued_by")) sql.exec("ALTER TABLE devices ADD COLUMN issued_by TEXT");
  },
  // RFC 0006 section 7: a revocation's notice to each space, kept until the space took it.
  `
  CREATE TABLE IF NOT EXISTS revocation_notices (device_id TEXT NOT NULL, space_id TEXT NOT NULL, next_try_ms INTEGER NOT NULL,
    PRIMARY KEY (device_id, space_id));
  `,
  // RFC 0007 section 2: spaces are listed in the order their rows were stored, which a space id does not give.
  (sql) => {
    const columns = sql.exec("SELECT name FROM pragma_table_info('spaces')").toArray().map((row) => row.name);
    if (!columns.includes("list_seq")) sql.exec("ALTER TABLE spaces ADD COLUMN list_seq INTEGER");
    sql.exec("UPDATE spaces SET list_seq = rowid WHERE list_seq IS NULL");
    sql.exec("CREATE UNIQUE INDEX IF NOT EXISTS spaces_list_seq ON spaces(list_seq)");
  },
];

export function migrate(sql: SqlStorage): void {
  runMigrations(sql, "settings", MIGRATIONS);
}
