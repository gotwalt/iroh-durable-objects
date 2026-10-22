// Unit tests: test/unit inside workerd (the Workers Vitest integration), test/node in Node.
import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    projects: [
      {
        plugins: [cloudflareTest({ wrangler: { configPath: "./test/unit/wrangler.toml" } })],
        test: { name: "workers", include: ["test/unit/**/*.test.ts"] },
      },
      {
        // --expose-gc: the leak test collects garbage itself.
        test: { name: "node", include: ["test/node/**/*.test.ts"], environment: "node", pool: "forks", execArgv: ["--expose-gc"] },
      },
    ],
  },
});
