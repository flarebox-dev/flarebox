// SPDX-License-Identifier: AGPL-3.0-or-later

import { DurableObject } from "cloudflare:workers";
import { accountLogic } from "./account/account";
import { migrate } from "./account/schema";
import type { Database, Sql } from "./host";
import { ShareLinks } from "./share/links";

export type { Outcome } from "./share/links";

export class AccountDO extends accountLogic(DurableObject<Env>) {
  protected readonly db: Database;
  protected readonly sql: Sql;
  protected readonly shares: ShareLinks;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.db = ctx.storage;
    this.sql = ctx.storage.sql;
    this.db.transactionSync(() => migrate(this.sql));
    this.shares = new ShareLinks(this.sql, {
      clock: () => this.clock(),
      serverSecret: async () => (await this.serverKey()).secret,
      spaceClosed: (spaceId) => this.spaceClosed(spaceId),
    });
  }

  protected get state(): DurableObjectState {
    return this.ctx;
  }

  protected spaceStub(spaceId: string) {
    return this.env.SPACE.get(this.env.SPACE.idFromName(spaceId));
  }
}
