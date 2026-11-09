// SPDX-License-Identifier: AGPL-3.0-or-later

import { DurableObject } from "cloudflare:workers";

/** One per space: tree, versions, chunks and the commit log. Not implemented yet. */
export class SpaceDO extends DurableObject<Env> {}
