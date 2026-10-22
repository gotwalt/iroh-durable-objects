// Two projects:
//
// - `unit`, in Node: the parts without I/O (protocol, framing, events, rate limit).
// - `workers`, in workerd through @cloudflare/vitest-plugin: ChatStorage on a Durable
//   Object's real SQLite, and Room, Ctl and DialBack on that storage with the fakes of
//   test/fakes/iroh.ts in place of the iroh-do endpoint, connections and streams. `iroh-do`
//   resolves to test/fakes/iroh-do.ts, since the real module loads the wasm library.
//
// The object against stock peers is tested in examples/ts-deployment/test and by the golden
// fixtures (`npm run test:fixtures`).
import { fileURLToPath } from "node:url";
import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

const path = (relative: string) => fileURLToPath(new URL(relative, import.meta.url));

export default defineConfig({
  test: {
    projects: [
      {
        test: { name: "unit", include: ["test/*.test.ts"], environment: "node" },
      },
      {
        plugins: [
          cloudflareTest({
            main: path("test/workers/worker.ts"),
            miniflare: {
              compatibilityDate: "2026-09-15",
              durableObjects: { TEST_OBJECT: { className: "TestObject", useSQLite: true } },
            },
          }),
        ],
        resolve: { alias: { "iroh-do": path("test/fakes/iroh-do.ts") } },
        test: { name: "workers", include: ["test/workers/*.test.ts"] },
      },
    ],
  },
});
