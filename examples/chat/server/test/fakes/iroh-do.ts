// What the tests resolve `iroh-do` to (vitest.config.ts). The chat classes under test import
// only `EndpointId` from it as a value; the real module loads the wasm library and
// `cloudflare:workers` at import.
export { EndpointId } from "./iroh";
