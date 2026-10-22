// The bindings of the test Worker (vitest.config.ts).
declare namespace Cloudflare {
  interface Env {
    TEST_OBJECT: DurableObjectNamespace<import("./worker").TestObject>;
  }
}
