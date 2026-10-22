// End-to-end tests in Node: the test fixture (test/fixture, test-panic build) in Miniflare,
// driven by the stock test peer (crates/iroh-do-test-peer) as a child process. Each file
// starts its own Miniflare on a port the OS picks, so files run in parallel.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["test/e2e/**/*.test.ts"],
    environment: "node",
    testTimeout: 120_000,
    hookTimeout: 120_000,
  },
});
