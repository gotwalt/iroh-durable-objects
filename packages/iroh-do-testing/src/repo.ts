// Where this repository's native test binaries are, for its own e2e tests. Kept apart from
// the general helpers in index.ts, which take paths as arguments.
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/** The repository root: this file is packages/iroh-do-testing/{src,dist}/repo.*. */
export const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");

/** A cargo binary of the workspace, debug profile; throws with the build command if missing. */
export function cargoBin(name: string, pkg = name): string {
  const target = process.env.CARGO_TARGET_DIR ?? join(repoRoot, "target");
  const bin = join(target, "debug", name);
  if (!existsSync(bin)) throw new Error(`${bin} is missing; build it with \`cargo build -p ${pkg}\``);
  return bin;
}

/**
 * The wrapper native binaries run through (AGENTS.md): it signs them so that the macOS
 * firewall does not prompt. Empty elsewhere.
 */
export const nativeWrapper: string[] =
  process.platform === "darwin" ? [join(repoRoot, "scripts", "macos-sign-run.sh")] : [];
