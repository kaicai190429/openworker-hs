/**
 * A tiny JSON-Schema builder with the same call shape as the `typebox` constructors the OpenClaw
 * plugin used, so the ported tool definitions stay unchanged and the SDK takes no new dependency.
 */
type Schema = Record<string, unknown>;
const OPTIONAL = Symbol("optional");
type Marked = Schema & { [OPTIONAL]?: true };

export const Type = {
  String: (opts: Schema = {}): Schema => ({ type: "string", ...opts }),
  Number: (opts: Schema = {}): Schema => ({ type: "number", ...opts }),
  Boolean: (opts: Schema = {}): Schema => ({ type: "boolean", ...opts }),
  Optional: (schema: Schema): Schema => {
    const copy: Marked = { ...schema };
    copy[OPTIONAL] = true;
    return copy;
  },
  Unsafe: <T = unknown>(schema: Schema): Schema & { __t?: T } => ({ ...schema }),
  Object: (props: Record<string, Schema>, opts: Schema = {}): Schema => {
    const required: string[] = [];
    const properties: Record<string, Schema> = {};
    for (const [key, value] of Object.entries(props)) {
      if (!(value as Marked)[OPTIONAL]) required.push(key);
      properties[key] = { ...value };
    }
    return { type: "object", properties, ...(required.length ? { required } : {}), additionalProperties: false, ...opts };
  },
};

export function optionalStringEnum<T extends readonly string[]>(values: T, opts: Schema = {}): Schema {
  return Type.Optional({ type: "string", enum: [...values], ...opts });
}
