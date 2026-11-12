// SPDX-License-Identifier: AGPL-3.0-or-later

import type { Op } from "./gen/koidrive/v1/commit_pb";

export interface Principal {
  deviceId: string;
  userId: string;
  kind: string;
  scope: string | null;
}

export type Capability = "space.create" | "device.pair" | "device.manage" | "share.manage" | "space.rewind" | "space.usage";

export interface OpEffect {
  nodes: ReadonlySet<string>;
  changed: ReadonlySet<string>;
  created: ReadonlySet<string>;
  ancestryBefore(nodeId: string): string[] | undefined;
  ancestryAfter(nodeId: string): string[];
  subtree(nodeId: string): string[];
}

export function knownPrincipal(kind: string, scope: string | null): boolean {
  return kind === "device" && scope === null;
}

export const policy = {
  authorize(principal: Principal, _op: Op, _effect: OpEffect): boolean {
    return knownPrincipal(principal.kind, principal.scope);
  },

  can(principal: Principal, _capability: Capability): boolean {
    return knownPrincipal(principal.kind, principal.scope);
  },

  deletes(_principal: Principal, _spaceId: string, _trashedFiles: number): boolean {
    return true;
  },
};
