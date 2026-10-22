// A sliding-window request limit per key.
//
// The windows are in memory. The object hibernates only after t_idle + t_linger without
// connections, which is longer than the room's window, so hibernation cannot cut a running
// window short; an eviction can, which PROTOCOL.md allows.

export class RateLimiter {
  readonly #limit: number;
  readonly #windowMs: number;
  /** Times of the admitted requests still inside the window, oldest first, by key. */
  readonly #admitted = new Map<string, number[]>();

  constructor(limit: number, windowMs: number) {
    this.#limit = limit;
    this.#windowMs = windowMs;
  }

  /** Records a request of `key` at `nowMs` if it is within the limit; returns whether it was. */
  allow(key: string, nowMs: number): boolean {
    const recent = (this.#admitted.get(key) ?? []).filter((t) => nowMs - t < this.#windowMs);
    if (recent.length >= this.#limit) {
      this.#admitted.set(key, recent);
      return false;
    }
    recent.push(nowMs);
    this.#admitted.set(key, recent);
    return true;
  }
}
