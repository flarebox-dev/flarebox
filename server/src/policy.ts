// SPDX-License-Identifier: AGPL-3.0-or-later

// The points where the server decides what a principal may do (RFC 0006 sections 3 and 10). In 0.1.0 every principal
// that authenticates is a full-scope device, so each point allows everything.

import type { Op } from "./gen/flarebox/v1/commit_pb";

/** A credential the server authenticated: a row of the Account DO's devices table (RFC 0006 section 2). */
export interface Principal {
  deviceId: string;
  userId: string;
  /** "device", or "agent" for the credentials of a later RFC. */
  kind: string;
  /** null means full scope. */
  scope: string | null;
}

export type Capability = "space.create" | "device.pair" | "device.manage" | "share.manage" | "space.rewind";

/** The transition one op made, read from the tree just before and just after it (RFC 0006 section 3). */
export interface OpEffect {
  /** Every node whose parent, name, status or content the op changed, by effective ID, the nodes it created included. */
  changed: ReadonlySet<string>;
  /** The nodes the op created, such as a new file, a new folder or a conflict copy. */
  created: ReadonlySet<string>;
  /** The node's ancestors from the root down before the op, or undefined for a node the op created. */
  ancestryBefore(nodeId: string): string[] | undefined;
  /** The node's ancestors from the root down after the op. */
  ancestryAfter(nodeId: string): string[];
  /** Every node under nodeId at any depth, trashed ones included. */
  subtree(nodeId: string): string[];
}

/** A credential written by a newer server, with a kind or scope this one does not know, gets nothing. */
export function knownPrincipal(kind: string, scope: string | null): boolean {
  return kind === "device" && scope === null;
}

/** Kept on one object so that tests can replace a decision. */
export const policy = {
  /** Whether the principal may make the op's transition, inside the commit transaction. */
  authorize(principal: Principal, _op: Op, _effect: OpEffect): boolean {
    return knownPrincipal(principal.kind, principal.scope);
  },

  /** Whether the principal may take an action that is not a commit. */
  can(principal: Principal, _capability: Capability): boolean {
    return knownPrincipal(principal.kind, principal.scope);
  },

  /** Whether a commit that sends trashedFiles live files to the trash may proceed without the owner's confirmation. */
  deletes(_principal: Principal, _spaceId: string, _trashedFiles: number): boolean {
    return true;
  },
};
