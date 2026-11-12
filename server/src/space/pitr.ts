// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * Bookmarks compare lexically in time order, and a restore armed with restoreOnNextStart applies before the object
 * next starts.
 */
export interface Pitr {
  currentBookmark(): Promise<string>;
  bookmarkForTime(timestampMs: number): Promise<string>;
  restoreOnNextStart(bookmark: string): Promise<void>;
}

/**
 * The default uses the Durable Object storage API; local workerd does not implement it, so tests replace open with a
 * simulation.
 */
export const pitrBackend = {
  open(state: DurableObjectState): Pitr {
    const storage = state.storage;
    return {
      currentBookmark: () => storage.getCurrentBookmark(),
      bookmarkForTime: (timestampMs) => storage.getBookmarkForTime(timestampMs),
      restoreOnNextStart: async (bookmark) => {
        await storage.onNextSessionRestoreBookmark(bookmark);
      },
    };
  },
};
