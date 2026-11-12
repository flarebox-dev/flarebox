// SPDX-License-Identifier: AGPL-3.0-or-later

// The services the server logic asks of the platform it runs on. Each interface keeps only the members the logic
// calls, under the names Cloudflare's Durable Object API uses, so the Durable Object classes pass Cloudflare's own
// objects unchanged and another host implements the same few members.

/** A value SQLite stores in a column or returns from one. */
export type SqlValue = ArrayBuffer | string | number | null;

/** A value a statement binds to a `?` placeholder. */
export type SqlBinding = SqlValue | Uint8Array;

export type SqlRow = Record<string, SqlValue>;

/** The rows of one statement. Reading them consumes the cursor. */
export interface SqlCursor<T extends SqlRow> extends Iterable<T> {
  toArray(): T[];
  /** The only row; throws unless the statement returned exactly one. */
  one(): T;
  /** Rows the statement wrote, read after the cursor is consumed. */
  readonly rowsWritten: number;
}

/** A synchronous SQLite connection: every statement runs to completion before exec returns. */
export interface Sql {
  exec<T extends SqlRow = SqlRow>(query: string, ...bindings: SqlBinding[]): SqlCursor<T>;
}

/** An object's SQLite database and its transactions. */
export interface Database {
  readonly sql: Sql;
  /** Runs closure in one transaction: it commits when closure returns and rolls back when closure throws. */
  transactionSync<T>(closure: () => T): T;
}
