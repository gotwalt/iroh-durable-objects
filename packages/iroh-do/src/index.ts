// iroh-do: iroh endpoints as TypeScript Durable Objects (design note 013).
//
// `IrohDurableObject` is the base class of an app's object; `irohRouter` is the Worker's
// `fetch`. The Connection and stream classes come from the wasm library
// (crates/iroh-do-wasm) and follow `@number0/iroh` 1.1.0, with the differences listed in
// README.md.
export {
  IrohDurableObject,
  type DestroyResult,
  type MintResult,
  type ProtocolHandler,
  type Protocols,
  type Status,
  type Timings,
} from "./object";
export { DEFAULT_PKARR_RELAY, irohRouter, type RouterEnv, type RouterOptions } from "./router";
export { BiStream, Connection, Endpoint, EndpointAddr, EndpointId, RecvStream, SendStream } from "./wasm";
