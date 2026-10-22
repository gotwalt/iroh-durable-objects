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
  /** When keys whose window has emptied were last removed. */
  #sweptAt = Number.NEGATIVE_INFINITY;

  constructor(limit: number, windowMs: number) {
    this.#limit = limit;
    this.#windowMs = windowMs;
  }

  /** Records a request of `key` at `nowMs` if it is within the limit; returns whether it was. */
  allow(key: string, nowMs: number): boolean {
    this.#sweep(nowMs);
    const recent = (this.#admitted.get(key) ?? []).filter((t) => nowMs - t < this.#windowMs);
    if (recent.length >= this.#limit) {
      this.#admitted.set(key, recent);
      return false;
    }
    recent.push(nowMs);
    this.#admitted.set(key, recent);
    return true;
  }

  /** The keys with a request inside the window. */
  get size(): number {
    return this.#admitted.size;
  }

  /**
   * Removes the keys with no request inside the window, at most once per window, so that the
   * map holds the peers of about the last two windows rather than every peer ever seen.
   */
  #sweep(nowMs: number): void {
    if (nowMs - this.#sweptAt < this.#windowMs) return;
    this.#sweptAt = nowMs;
    for (const [key, times] of this.#admitted) {
      const newest = times[times.length - 1];
      if (newest === undefined || nowMs - newest >= this.#windowMs) this.#admitted.delete(key);
    }
  }
}
