// Reading the echo class's vars, apart from the class so that it can be tested in Node.

/**
 * A var holding a positive whole number of milliseconds; undefined if unset or empty. Throws
 * on anything else, so that a typo in a deployment fails the node build instead of running
 * with the default.
 */
export function milliseconds(value: string | undefined, name: string): number | undefined {
  if (value === undefined || value === "") return undefined;
  const ms = Number(value);
  if (!Number.isSafeInteger(ms) || ms <= 0) throw new RangeError(`${name} must be a positive integer, got ${value}`);
  return ms;
}
