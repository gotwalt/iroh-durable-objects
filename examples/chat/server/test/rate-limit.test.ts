import { expect, it } from "vitest";
import { RateLimiter } from "../src/rate-limit";

it("admits `limit` requests per sliding window, without counting refusals", () => {
  const limiter = new RateLimiter(3, 1000);
  expect([0, 100, 200, 300].map((t) => limiter.allow("a", t))).toEqual([true, true, true, false]);
  // Other keys have their own window.
  expect(limiter.allow("b", 300)).toBe(true);
  // The request at 0 leaves the window at 1000; the refusal at 300 never entered it.
  expect(limiter.allow("a", 999)).toBe(false);
  expect(limiter.allow("a", 1000)).toBe(true);
  expect(limiter.allow("a", 1001)).toBe(false);
});
