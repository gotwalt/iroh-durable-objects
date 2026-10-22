import { generateSecretKey, publicKeyOf } from "../../src/wasm.js";

/** A new, valid EndpointId in z-base-32. */
export function newId(): string {
  const key = publicKeyOf(generateSecretKey());
  const id = key.toZ32();
  key.free();
  return id;
}
