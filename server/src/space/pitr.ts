// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * Bookmarks compare lexically in time order, and a restore armed with restoreOnNextStart applies before the object
 * next starts. bookmarkForTime answers undefined for a time before the object's PITR history begins.
 */
export interface Pitr {
  currentBookmark(): Promise<string>;
  bookmarkForTime(timestampMs: number): Promise<string | undefined>;
  restoreOnNextStart(bookmark: string): Promise<void>;
}

/** Cloudflare's PITR history of a new object begins some time after the object is created and reports this only as an error message. */
const BEFORE_HISTORY = "Requested time is before this database existed";

/**
 * The default uses the Durable Object storage API; local workerd does not implement it, so tests replace open with a
 * simulation.
 */
export const pitrBackend = {
  open(state: DurableObjectState): Pitr {
    const storage = state.storage;
    return {
      currentBookmark: () => storage.getCurrentBookmark(),
      bookmarkForTime: (timestampMs) =>
        storage.getBookmarkForTime(timestampMs).catch((error: unknown) => {
          if (error instanceof Error && error.message.startsWith(BEFORE_HISTORY)) return undefined;
          throw error;
        }),
      restoreOnNextStart: async (bookmark) => {
        await storage.onNextSessionRestoreBookmark(bookmark);
      },
    };
  },
};
