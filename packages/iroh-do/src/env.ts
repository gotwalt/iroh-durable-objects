// Reading the bindings the library uses from an app's `env`, whose type the library does not
// know: each read checks the value's type at run time instead of casting `env`.

/** The vars the library reads. All optional; see README.md, "Bindings". */
export interface IrohVars {
  /** tracing directives for the wasm side's log (design note 010). */
  LOG_LEVEL?: string;
  /** Reported in the status, first 12 characters. */
  BUNDLE_SHA256?: string;
}

/** `env[name]` if it is a string, else undefined. */
export function stringVar(env: unknown, name: string): string | undefined {
  if (typeof env !== "object" || env === null) return undefined;
  const v: unknown = Reflect.get(env, name);
  return typeof v === "string" ? v : undefined;
}

/** `env[name]` if it is a Durable Object namespace, else undefined. */
export function namespaceVar<T extends Rpc.DurableObjectBranded | undefined = undefined>(
  env: unknown,
  name: string,
): DurableObjectNamespace<T> | undefined {
  if (typeof env !== "object" || env === null) return undefined;
  const v: unknown = Reflect.get(env, name);
  if (typeof v !== "object" || v === null) return undefined;
  return typeof Reflect.get(v, "idFromName") === "function" && typeof Reflect.get(v, "get") === "function"
    ? (v as DurableObjectNamespace<T>)
    : undefined;
}
