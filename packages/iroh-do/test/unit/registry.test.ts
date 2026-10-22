// Pure functions of the registry and the list page, and the registry object's checks.
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { htmlEscape, labelError, listPage, mintBody, REGISTRY_NAME, utc } from "../../src/registry.js";
import { newId } from "./ids.js";

const ID = "ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u";

describe("mintBody", () => {
  it("accepts an empty body, app and label, and trims the label", () => {
    expect(mintBody("")).toEqual({ app: null, label: "" });
    expect(mintBody('{"app":"chat","label":"  room  "}')).toEqual({ app: "chat", label: "room" });
    expect(mintBody('{"app":null}')).toEqual({ app: null, label: "" });
  });

  it("refuses what is not an object of strings", () => {
    expect(mintBody("label=x")).toHaveProperty("error");
    expect(mintBody("[1]")).toEqual({ error: 'body must be JSON {"app":…, "label":…}: not an object' });
    expect(mintBody('{"app":1}')).toHaveProperty("error");
  });
});

describe("labelError", () => {
  it("counts characters, not UTF-16 units, and refuses the Cc category", () => {
    expect(labelError("é".repeat(64))).toBeNull();
    expect(labelError("😀".repeat(64))).toBeNull();
    expect(labelError("é".repeat(65))).toBe("label longer than 64 characters");
    expect(labelError("a\u0085b")).toBe("label contains control characters");
    expect(labelError("a\tb")).toBe("label contains control characters");
  });
});

describe("list page", () => {
  it("escapes labels, apps and hints, and links the next page", () => {
    expect(htmlEscape(`<a href="x">'&'</a>`)).toBe("&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;");
    expect(utc(0)).toBe("1970-01-01 00:00 UTC");
    const page = listPage([{ id: ID, app: "<app>", label: "", mintedAt: 0 }], (_a, id) => `x ${id}`, "/?before=5");
    expect(page).toContain("&lt;app&gt;");
    expect(page).toContain('<a href="/?before=5">Older endpoints</a>');
  });
});

describe("IrohRegistry", () => {
  const registry = () => env.REGISTRY.get(env.REGISTRY.idFromName(REGISTRY_NAME));

  it("refuses malformed entries", async () => {
    const r = registry();
    expect(await r.register({ id: "nope", app: "a", label: "", mintedAt: 1 })).toBe(false);
    expect(await r.register({ id: ID, app: "", label: "", mintedAt: 1 })).toBe(false);
    expect(await r.register({ id: ID, app: "a", label: "x".repeat(65), mintedAt: 1 })).toBe(false);
    expect(await r.register({ id: ID, app: "a", label: "", mintedAt: Number.NaN })).toBe(false);
    expect(await r.lookup(ID)).toBeNull();
  });

  it("pages newest first", async () => {
    const r = registry();
    const ids = [newId(), newId(), newId()];
    for (const [i, id] of ids.entries()) {
      expect(await r.register({ id, app: "a", label: "", mintedAt: 1000 + i })).toBe(true);
    }
    const p1 = await r.list({ limit: 2, before: 1003 });
    expect(p1.entries.map((e) => e.mintedAt)).toEqual([1002, 1001]);
    expect(p1.next).toBe(1001);
    const p2 = await r.list({ limit: 2, before: 1001 });
    expect(p2.entries.map((e) => e.mintedAt)).toContain(1000);
    expect(await r.lookup(ids[0] ?? "")).toBe("a");
  });
});
