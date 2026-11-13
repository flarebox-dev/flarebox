// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * Bookmarks compare lexically in time order, and a restore armed with restoreOnNextStart applies before the object
 * next starts.
 */
export interface Pitr {
  currentBookmark(): Promise<string>;
  bookmarkForTime(timestampMs: number): Promise<string | undefined>;
  restoreOnNextStart(bookmark: string): Promise<void>;
}

const BEFORE_PITR_HISTORY_MESSAGE = "Requested time is before this database existed.";

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
          if (error instanceof Error && error.message === BEFORE_PITR_HISTORY_MESSAGE) return undefined;
          throw error;
        }),
      restoreOnNextStart: async (bookmark) => {
        await storage.onNextSessionRestoreBookmark(bookmark);
      },
    };
  },
};
