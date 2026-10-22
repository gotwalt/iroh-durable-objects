// Errors the base class throws, so that callers can tell them apart without parsing messages.

/** Why a node stopped being usable. */
export type NodeLostCause = "wasm_reset" | "destroy";

/**
 * The reason of `IrohDurableObject.nodeSignal` when it aborts: the node, and every
 * `Endpoint`, `Connection` and stream obtained from it, is no longer usable.
 */
export class NodeLostError extends Error {
  override readonly name = "NodeLostError";
  constructor(readonly nodeCause: NodeLostCause) {
    super(nodeCause === "wasm_reset" ? "node lost: the wasm instance was reset" : "node lost: endpoint destroyed");
  }
}

/** Thrown by `withNode` when the body trapped the wasm instance; the node is already rebuilt. */
export class NodeTrappedError extends Error {
  override readonly name = "NodeTrappedError";
  constructor(cause: unknown) {
    super("the wasm instance trapped; the node was rebuilt", { cause });
  }
}

/** Building the node failed (the app's `protocols()` threw, or the options were invalid). */
export class NodeBuildError extends Error {
  override readonly name = "NodeBuildError";
  constructor(cause: unknown) {
    super(`node build failed: ${String(cause)}`, { cause });
  }
}

/** The object holds no endpoint: it was never minted, or it was destroyed. */
export class NotMintedError extends Error {
  override readonly name = "NotMintedError";
  constructor() {
    super("endpoint not minted");
  }
}
