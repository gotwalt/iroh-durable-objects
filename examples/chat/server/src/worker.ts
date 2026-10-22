// The chat class alone behind the single-class router, for local runs of this package. The
// deployment is examples/ts-deployment.
import { irohRouter } from "iroh-do";
import type { ChatEnv } from "./chat";

export { Chat } from "./chat";

/**
 * Compiles only if the bindings of wrangler.toml, as `wrangler types` generates them into
 * `Env`, provide what the class reads (ChatEnv).
 */
export type EnvProvidesChatEnv = Env extends ChatEnv ? true : never;
true satisfies EnvProvidesChatEnv;

export default { fetch: irohRouter({ binding: "CHAT" }) } satisfies ExportedHandler<Env>;
