// The chat class alone behind the single-class router, for local runs of this package. The
// deployment is examples/ts-deployment.
import { irohRouter } from "iroh-do";

export { Chat } from "./chat";

export default { fetch: irohRouter({ binding: "CHAT" }) };
