// The echo class alone behind the single-class router, for this package's tests and local
// runs. The deployment `iroh-ts` is examples/ts-deployment, which serves it beside the chat.
import { irohRouter } from "iroh-do";

export { Echo } from "./echo";

export default { fetch: irohRouter({ binding: "ENDPOINTS" }) };
