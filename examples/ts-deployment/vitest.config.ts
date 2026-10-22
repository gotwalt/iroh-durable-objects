// End-to-end tests in Node against the bundle in dist/ (`npm run test:e2e` builds it first).
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: { include: ["test/**/*.test.ts"], environment: "node", testTimeout: 180_000, hookTimeout: 120_000 },
});
