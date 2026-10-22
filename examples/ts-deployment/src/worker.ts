// The deployment `iroh-ts` (design note 013): one Worker script serving the chat and the echo
// app, each its own Durable Object class, and the registry that records which app each
// endpoint runs. The router finds an endpoint's class through the registry.
import { IrohRegistry, irohRouter } from "iroh-do";

export { Chat } from "iroh-do-chat-server";
export { Echo } from "iroh-do-echo-server";

/** The endpoint list and `E → app`; class name `Registry`, binding `REGISTRY`. */
export class Registry extends IrohRegistry {}

export default {
  fetch: irohRouter({
    // The first app is the default for a mint that names none, as in the Rust deployment.
    apps: {
      chat: { binding: "CHAT", joinHint: (id) => `iroh-do-chat ${id}` },
      echo: { binding: "ENDPOINTS", joinHint: (id) => `iroh-do-echo-client ${id}` },
    },
    registry: "REGISTRY",
  }),
};
