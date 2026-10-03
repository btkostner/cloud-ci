/**
 * Shared hardening primitives for this package's hand-written Connect
 * unary-JSON RPC clients (`RpcShardPlanner` in `rpc-shard-planner.ts`,
 * `RpcShardGroupRegistrar` in `rpc-shard-group-registrar.ts`). Factored
 * out so both clients enforce the exact same scrubbing/validation/decode
 * discipline from one place — a future fix here (e.g. a new token-leak
 * vector) lands for every client at once, instead of risking two copies
 * silently drifting apart. Every function here is error-class-agnostic:
 * each client passes in its own `invalid`/`malformed`-style error
 * constructor (`ShardPlanRpcError` vs `ShardGroupRegisterRpcError`), so
 * a caller catching a specific error class still only ever sees that
 * class, never a shared generic one.
 *
 * See `rpc-shard-planner.ts`'s own doc comment for the wire-format
 * background (Connect unary-JSON, `cloud-ci-worker/src/connect.rs`) this
 * all exists to speak correctly and safely.
 */

export const MAX_ECHO_CHARS = 200;
/** Below this length, redacting every occurrence of `token` in echoed text
 * risks mangling ordinary words (e.g. a server message that happens to
 * contain the literal substring "token"). Only bearer credentials long
 * enough to be unambiguous get redacted — see `scrub`'s own doc comment. */
export const MIN_REDACTABLE_TOKEN_LENGTH = 8;

/** Redacts every occurrence of the bearer `token` in text that may echo
 * network input (a transport error, a server body), then caps the result
 * at `MAX_ECHO_CHARS`. This is a best-effort exact-string match, not a
 * guarantee: a token under `MIN_REDACTABLE_TOKEN_LENGTH` characters is
 * skipped (redacting a short string risks mangling an unrelated word like
 * "token" in ordinary server text), and a token containing characters a
 * JSON encoder re-escapes (e.g. a literal `"` or `\`) can fail to match
 * the echoed, re-serialized text even when longer. Redaction runs before
 * truncation so a cut can never leave a partial token visible. */
export function scrub(text: string, token: string): string {
  const redacted =
    token.length >= MIN_REDACTABLE_TOKEN_LENGTH ? text.split(token).join("[redacted]") : text;
  return redacted.length > MAX_ECHO_CHARS
    ? `${redacted.slice(0, MAX_ECHO_CHARS)}…(truncated)`
    : redacted;
}

export function show(value: unknown, token: string): string {
  return scrub(typeof value === "string" ? value : String(JSON.stringify(value)), token);
}

export function assertUint(
  name: string,
  value: number,
  max: number,
  invalid: (message: string) => Error,
): void {
  if (!Number.isInteger(value) || value < 0 || value > max) {
    throw invalid(`${name} must be an integer in 0..${max}, got ${String(value)}`);
  }
}

/** Validates and normalizes `baseUrl` using `new URL()` (not a regex) so
 * scheme/userinfo/host parsing matches the platform's own URL grammar.
 * Accepts only the exact `defaultBaseUrl`, or an `https:` URL with no
 * embedded userinfo, path, query, or fragment — a plain `http://`
 * override would send the bearer credential in cleartext, userinfo in
 * the URL is a second, easily-overlooked place a credential could leak,
 * and a non-root path/query/fragment is silently dropped below (each
 * client always appends its own fixed `IngestService` RPC path to this
 * result) — a caller routing through a path-prefixed gateway would
 * otherwise get a wrong-endpoint 404 with no warning, so this rejects
 * rather than discarding any of them. Rejects any whitespace outright,
 * since the WHATWG `URL` parser silently strips some whitespace variants
 * rather than rejecting them. Compares the *parsed* origin (not the raw
 * string) against `defaultBaseUrl`, so `"http://cloud-ci.internal/"` and
 * `"http://cloud-ci.internal//"` are still exactly the default — only a
 * raw string that parses to a *different* origin needs `https:`. */
export function normalizeBaseUrl(
  raw: string | undefined,
  defaultBaseUrl: string,
  invalid: (message: string) => Error,
): string {
  if (raw === undefined) {
    return defaultBaseUrl;
  }
  if (/\s/.test(raw)) {
    throw invalid(
      `baseUrl must not contain whitespace, got "${scrub(redactUserinfoInUrl(raw), "")}"`,
    );
  }
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    throw invalid(`baseUrl "${scrub(redactUserinfoInUrl(raw), "")}" is not a valid URL`);
  }
  // From here on every message below describes `url`, never the raw
  // input — `describeOrigin`/`url.protocol` never include userinfo, a
  // path, or a query string by construction, so they are always safe to
  // echo into an exception a caller might log (U3: a rejected
  // `user:pass@host` URL must not write that password into its own
  // rejection message).
  const isDefaultOrigin = url.origin === defaultBaseUrl;
  if (!isDefaultOrigin && url.protocol.toLowerCase() !== "https:") {
    throw invalid(
      `baseUrl "${describeOrigin(url)}" must use https:, or be the internal service-binding ` +
        `default ${defaultBaseUrl}`,
    );
  }
  if (url.username !== "" || url.password !== "") {
    throw invalid(`baseUrl "${describeOrigin(url)}" must not contain a username or password`);
  }
  // Any number of leading slashes with nothing else ("/", "//", ...) still
  // counts as "no path", so the documented default's trailing-slash forms
  // keep working through this same check. A bare trailing "?" parses to
  // `url.search === ""` (the WHATWG URL spec drops an empty query), so
  // that case is checked against `raw` directly rather than `url.search`.
  const hasPath = url.pathname.replace(/\/+/g, "") !== "";
  const hasBareQuestionMark = raw.includes("?");
  if (hasPath || url.search !== "" || hasBareQuestionMark || url.hash !== "") {
    throw invalid(
      `baseUrl "${describeOrigin(url)}" must be a bare origin with no path, query string, or ` +
        `fragment`,
    );
  }
  return url.origin;
}

/** `url.origin` is the literal string `"null"` for opaque-origin schemes
 * (`javascript:`, a bare `scheme:opaque` string with no `//` authority,
 * ...) — printing that into a rejection message ("baseUrl "null" must
 * use https:...") names nothing useful. Falls back to `url.protocol`
 * (e.g. `"javascript:"`), which is always present and, like `origin`,
 * never includes userinfo. */
export function describeOrigin(url: URL): string {
  return url.origin === "null" ? url.protocol : url.origin;
}

/** Strips a `user:pass@`/`user@` userinfo prefix from `raw` before it is
 * echoed into an exception message. Defense-in-depth for the two
 * `normalizeBaseUrl` branches above that run *before* `new URL()` has
 * successfully parsed `raw` (whitespace, and an unparsable URL), where
 * `url.origin`'s stronger guarantee isn't available yet — a plain regex
 * rather than `new URL()`, since this runs on strings that may be exactly
 * what `new URL()` itself refuses to parse. */
export function redactUserinfoInUrl(raw: string): string {
  return raw.replace(/^([a-zA-Z][a-zA-Z0-9+.-]*:\/\/)[^/?#@]*@/, "$1");
}

/** Connect error body (`cloud-ci-worker/src/connect.rs`'s `ErrorBody`):
 * `{ code, message }`. Falls back to a `"transport"` code with the raw
 * body when the error body itself doesn't match that shape (e.g. a
 * non-Connect-aware proxy's HTML error page) — narrowed via `in`/`typeof`,
 * never cast, since this is unvalidated network input. */
export function decodeConnectError(
  body: unknown,
  token: string,
): { code: string; message: string } {
  if (
    typeof body === "object" &&
    body !== null &&
    "code" in body &&
    typeof body.code === "string" &&
    "message" in body &&
    typeof body.message === "string"
  ) {
    return { code: scrub(body.code, token), message: scrub(body.message, token) };
  }
  return { code: "transport", message: `non-Connect error response: ${show(body, token)}` };
}

/** Resolves a `token: string | (() => string | Promise<string>)` dep into
 * a validated, non-empty, control-character-free bearer credential — the
 * exact validation `RpcShardPlanner.resolve()` originally ran inline,
 * extracted so both clients fail the same way on the same bad input.
 * Narrowed through `unknown` at every step: the declared type promises
 * `string | () => string | Promise<string>`, but this is caller-supplied
 * data/code and untyped JS (or a bug) can hand over anything. A throwing
 * or rejecting token callback is caught and rewrapped via `invalid` — no
 * raw error from caller code ever escapes a client's declared error type
 * — with the original preserved as `Error.cause` (via `invalid`'s own
 * `cause` parameter) so a caller's error-reporting tooling that already
 * walks `.cause` still sees the real failure and its stack, not just the
 * rewrap. */
export async function resolveBearerToken(
  tokenSource: string | (() => string | Promise<string>),
  invalid: (message: string, cause?: unknown) => Error,
): Promise<string> {
  const source: unknown = tokenSource;
  let resolved: unknown;
  if (typeof source === "string") {
    resolved = source;
  } else if (typeof source === "function") {
    try {
      resolved = await source();
    } catch (err) {
      throw invalid(
        scrub(`token callback failed: ${err instanceof Error ? err.message : String(err)}`, ""),
        err,
      );
    }
  } else {
    throw invalid(`token must be a string or a function, got ${typeof source}`);
  }
  // Sending `Bearer undefined`, `Bearer ` or a header with embedded
  // control characters is a confusing failure far from this call site,
  // so `fetch` never runs with a token that is not a usable credential.
  // None of these messages echo the token value itself.
  if (typeof resolved !== "string") {
    throw invalid(`token callback must resolve to a non-empty string, got ${typeof resolved}`);
  }
  if (resolved.length === 0) {
    throw invalid("token must be a non-empty string, got an empty string");
  }
  if (resolved.trim().length === 0) {
    throw invalid("token must be a non-empty string, got a whitespace-only string");
  }
  const hasControlChar = [...resolved].some((ch) => {
    const code = ch.charCodeAt(0);
    return code < 0x20 || code === 0x7f;
  });
  if (hasControlChar) {
    throw invalid("token must not contain control characters");
  }
  return resolved;
}
