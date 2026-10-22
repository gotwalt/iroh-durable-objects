// Unit tests of the parts without I/O, in Node. The object itself is tested against stock
// peers in Miniflare (examples/ts-deployment/test) and by the golden fixtures.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    environment: "node",
  },
});
