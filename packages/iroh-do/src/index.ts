// iroh-do: iroh endpoints as TypeScript Durable Objects (design note 013).
//
// `IrohDurableObject` is the base class of an app's object; `irohRouter` is the Worker's
// `fetch`; `IrohRegistry` records the minted endpoints and, in a deployment with several
// apps, which app each one runs. The Connection and stream API follows `@number0/iroh` 1.1.0,
// with the differences listed in README.md.
import type { EndpointAddrConstructor, EndpointIdStatic } from "./types.js";
import { GlueEndpointAddr, GlueEndpointId } from "./wasm.js";

export type { IrohVars } from "./env.js";
export { NodeBuildError, type NodeLostCause, NodeLostError, NodeTrappedError, NotMintedError } from "./errors.js";
export { type Level, log } from "./log.js";
export {
  type DestroyResult,
  type EndpointStatus,
  type HostStatus,
  IrohDurableObject,
  type JsonScalar,
  type MintResult,
  type ProtocolHandler,
  type Protocols,
  type StatusValue,
  type Timings,
} from "./object.js";
export {
  IrohRegistry,
  LIST_LIMIT,
  MAX_LABEL,
  REGISTRY_NAME,
  type RegistryEntry,
  type RegistryPage,
} from "./registry.js";
export {
  type AppRoute,
  DEFAULT_PKARR_RELAY,
  type IrohRouter,
  irohRouter,
  type RouterEnv,
  type RouterOptions,
} from "./router.js";
export type { BiStream, Connection, Endpoint, RecvStream, SendStream } from "./types.js";

/** Parses and builds endpoint ids. */
export const EndpointId: EndpointIdStatic = GlueEndpointId as unknown as EndpointIdStatic;
export type EndpointId = import("./types.js").EndpointId;
/** An endpoint's id and relay URL. */
export const EndpointAddr: EndpointAddrConstructor = GlueEndpointAddr as unknown as EndpointAddrConstructor;
export type EndpointAddr = import("./types.js").EndpointAddr;
