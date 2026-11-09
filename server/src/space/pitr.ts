// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * The point-in-time recovery calls a rewind makes on the Space DO's storage (RFC 0003). Bookmarks compare lexically
 * in time order, and a restore armed with restoreOnNextStart applies before the object next starts.
 */
export interface Pitr {
  currentBookmark(): Promise<string>;
  bookmarkForTime(timestampMs: number): Promise<string>;
  restoreOnNextStart(bookmark: string): Promise<void>;
}

/**
 * Opens the PITR interface when a Space DO starts. The default uses the Durable Object storage API; local workerd does
 * not implement it, so tests replace open with a simulation.
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
