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

/** Metadata of a stored object. */
export interface StoredObject {
  readonly key: string;
  readonly size: number;
  readonly uploaded: Date;
  /** Checksums the store computed or was given at upload; sha256 is absent when it has none. */
  readonly checksums: { readonly sha256?: ArrayBuffer };
}

/** A stored object and its contents, read once from body. */
export interface StoredObjectBody extends StoredObject {
  readonly body: ReadableStream<Uint8Array>;
}

/** One page of a listing in key order. A truncated page names the cursor of the next. */
export type ObjectList = { objects: StoredObject[] } & ({ truncated: true; cursor: string } | { truncated: false });

export interface PutOptions {
  /** The SHA-256 the body must have, in hex; a body with another digest is refused with an error that says "checksum". */
  sha256?: string;
  /** With etagDoesNotMatch "*", the put writes only when no object has the key and otherwise leaves the stored object. */
  onlyIf?: { etagDoesNotMatch: string };
}

/** An object store with a flat key space: the chunks of every space and the metadata export (RFC 0004). */
export interface ObjectStore {
  head(key: string): Promise<StoredObject | null>;
  get(key: string): Promise<StoredObjectBody | null>;
  put(key: string, value: ReadableStream | ArrayBuffer | ArrayBufferView | string | null, options?: PutOptions): Promise<unknown>;
  list(options: { prefix: string; cursor?: string; limit: number }): Promise<ObjectList>;
  delete(keys: string | string[]): Promise<void>;
}

/** One end of a hibernatable WebSocket. The attachment is a small value kept with the socket across hibernation. */
export interface Socket {
  readonly readyState: number;
  send(message: ArrayBuffer | ArrayBufferView | string): void;
  close(code?: number, reason?: string): void;
  serializeAttachment(value: unknown): void;
  deserializeAttachment(): unknown;
}

/** An object's database, its durable writes and its one alarm. */
export interface ObjectStorage extends Database {
  /** Resolves when every write made so far is durable. */
  sync(): Promise<void>;
  /** Runs the object's alarm handler at scheduledTimeMs, replacing any earlier setting. */
  setAlarm(scheduledTimeMs: number): Promise<void>;
  deleteAlarm(): Promise<void>;
}

/** What an object asks of its host: storage, its open sockets, exclusive sections and a restart. */
export interface ObjectState {
  readonly storage: ObjectStorage;
  /** The open sockets, or those accepted with tag. */
  getWebSockets(tag?: string): Socket[];
  /** Runs callback while no other event of this object runs. */
  blockConcurrencyWhile<T>(callback: () => Promise<T>): Promise<T>;
  /** Ends the object's current instance; the next event starts a fresh one from storage. */
  abort(reason?: string): void;
}
