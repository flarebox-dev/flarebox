// SPDX-License-Identifier: AGPL-3.0-or-later

import { DurableObject } from "cloudflare:workers";

/** One per deployment: users, devices, tokens, spaces and share links. Not implemented yet. */
export class AccountDO extends DurableObject<Env> {}
