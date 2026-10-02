/** Pure request-shape helpers, kept separate from `src/index.ts` so they
 * are unit-testable without a Workers runtime (no bindings, no `Request`
 * parsing beyond what's passed in). */

export interface ScriptRequestBody {
  script: string;
  params?: unknown;
}

export type ValidateScriptRequestBodyResult =
  | { ok: true; value: ScriptRequestBody }
  | { ok: false; error: string };

/** Validates a parsed `POST /scripts` JSON body. The only required field
 * is a non-empty `script` string; `params` passes through unvalidated —
 * this round's test script is the only consumer and reads whatever shape
 * it wants from it. */
export function validateScriptRequestBody(body: unknown): ValidateScriptRequestBodyResult {
  if (typeof body !== "object" || body === null) {
    return { ok: false, error: "body must be a JSON object" };
  }
  const script = (body as Record<string, unknown>).script;
  if (typeof script !== "string" || script.length === 0) {
    return { ok: false, error: "script must be a non-empty string" };
  }
  return { ok: true, value: { script, params: (body as Record<string, unknown>).params } };
}

/** Extracts the `:id` segment from `/instances/:id`, or `null` for any
 * other path shape. */
export function parseInstanceIdPath(pathname: string): string | null {
  const match = pathname.match(/^\/instances\/([^/]+)$/);
  return match ? (match[1] as string) : null;
}
