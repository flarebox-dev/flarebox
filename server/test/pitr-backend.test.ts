// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from "vitest";
import { pitrBackend } from "../src/space/pitr";

function storageRejecting(error: Error): DurableObjectState {
  return { storage: { getBookmarkForTime: () => Promise.reject(error) } } as unknown as DurableObjectState;
}

describe("the Durable Object PITR backend", () => {
  it("answers no bookmark for a time before the object's PITR history begins, and passes any other error on", async () => {
    const before = pitrBackend.open(storageRejecting(new Error("Requested time is before this database existed.")));
    expect(await before.bookmarkForTime(1)).toBeUndefined();
    const other = new Error("internal error");
    await expect(pitrBackend.open(storageRejecting(other)).bookmarkForTime(1)).rejects.toBe(other);
  });
});
