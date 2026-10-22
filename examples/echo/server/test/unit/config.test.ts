import { describe, expect, it } from "vitest";
import { milliseconds } from "../../src/config.js";

describe("milliseconds", () => {
  it("reads positive whole numbers, and unset or empty as the default", () => {
    expect(milliseconds("1500", "T")).toBe(1500);
    expect(milliseconds(undefined, "T")).toBeUndefined();
    expect(milliseconds("", "T")).toBeUndefined();
  });

  it("refuses anything else instead of passing NaN on", () => {
    for (const v of ["abc", "0", "-5", "1.5", "1e400"]) {
      expect(() => milliseconds(v, "T_IDLE_MS")).toThrow(/T_IDLE_MS must be a positive integer/);
    }
  });
});
