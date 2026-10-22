// The Worker the storage-backed tests run in (vitest.config.ts, project `workers`). Its one
// Durable Object class has no behavior: tests take its SQLite storage with
// `runInDurableObject` and run the chat classes on it.
import { DurableObject } from "cloudflare:workers";

export class TestObject extends DurableObject {}

export default {
  fetch: () => new Response("tests only", { status: 404 }),
} satisfies ExportedHandler;
