// SPDX-License-Identifier: AGPL-3.0-or-later

export { AccountDO } from "./account-do";
export { SpaceDO } from "./space-do";

export default {
  async fetch(_request, _env, _ctx): Promise<Response> {
    return new Response("Not Found", { status: 404 });
  },
} satisfies ExportedHandler<Env>;
