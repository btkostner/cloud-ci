import { ShardPlanRpcError } from "./errors.js";
import type {
  ShardCountOption,
  ShardPlan,
  ShardPlanFetcher,
  ShardPlanner,
  ShardPlanRequest,
  SplitStrategy,
} from "./types.js";

/**
 * Real `ShardPlanner` — reaches `cloud-ci-worker`'s `ResolveShardPlan` RPC
 * (`cloud_ci.ingest.v1.IngestService/ResolveShardPlan`,
 * `packages/cloud-ci-worker/src/shard_plan.rs`) over an injected
 * `ShardPlanFetcher` (a `CONTAINER_WORKER`-style service binding — see
 * `ShardPlanFetcher`'s doc comment in `types.ts`). Speaks the Connect
 * unary-JSON wire format `cloud-ci-worker/src/connect.rs`'s `Codec::Json`
 * and `negotiate()` implement: `POST` to
 * `/cloud_ci.ingest.v1.IngestService/ResolveShardPlan`,
 * `content-type: application/json`, a plain proto3-JSON request body, a
 * plain proto3-JSON response body on success, and a Connect error body
 * (`{ code, message }`, always JSON regardless of request codec — see
 * `ConnectError::body`) on failure.
 *
 * The exact proto3-JSON field encodings below (camelCase names, `uint64`
 * as a quoted decimal string, `uint32` as a bare number, enums as their
 * proto name string, the `ShardCountSpec` oneof flattened to a one-key
 * `{ fixed }`/`{ range }` object, `google.protobuf.Duration` as a quoted
 * `"<seconds>s"` string) come from `packages/cloud-ci-proto-rust/generated`
 * (`buffa`'s generated `serde` impls for `ResolveShardPlanRequest`/
 * `ResolveShardPlanResponse`/`ShardCountSpec`), not from guessing a generic
 * protobuf-JSON convention — `cloud-ci-proto`'s `buf.gen.yaml` only
 * generates a Rust client (`local: protoc-gen-buffa`, `out:
 * ../cloud-ci-proto-rust/generated`); there is no TypeScript generation
 * target, so this hand-written client mirrors that generated Rust shape
 * exactly instead of inventing an undocumented one.
 *
 * `ResolveShardPlan` requires a credential (`handle_resolve_shard_plan`'s
 * doc comment and `verify_scoped_api_token_begin_run_credential`'s own
 * `ingest:write`-scope check): a GitHub Actions OIDC JWT, or a scoped API
 * token with the `ingest:write` scope and this `repoId` in its allowlist.
 * The run's `BeginRun` ingest token is NOT accepted — this call reuses the
 * `BeginRun` credential check, not the run-scoped post-`BeginRun` flow (see
 * `handle_resolve_shard_plan`'s doc comment: "mirrors
 * `handle_get_test_timings`'s own credential model ... rather than the
 * run-scoped ingest token every post-`BeginRun` call uses"). `token` is
 * called fresh on every `resolve()` so a caller can rotate a short-lived
 * token between calls.
 *
 * The "fallback when no history exists" parallelization.md documents
 * (median imputation, degrading to `min` with no history at all) is
 * entirely server-side (`shard_plan::resolve_plan`'s `durations_ms`
 * handling) — this client never special-cases it, it just decodes
 * whatever `ResolveShardPlanResponse` the server computed.
 */
export class RpcShardPlanner implements ShardPlanner {
  private readonly baseUrl: string;

  constructor(
    private readonly deps: {
      readonly fetcher: ShardPlanFetcher;
      /** GitHub's numeric repository id (proto `uint64`). */
      readonly repoId: number | bigint;
      /** Credential sent as `Authorization: Bearer` — see this class's own
       * doc comment for exactly which credentials `ResolveShardPlan`
       * accepts. */
      readonly token: string | (() => string | Promise<string>);
      /** Origin the request URL is built against. Ignored by a real
       * service binding's routing (it dispatches directly into the bound
       * Worker — see `pipeline-script.js`'s own
       * `http://cloud-ci.internal/...` convention in
       * `cloud-ci-dynamic-workflows-host`), but still required to build a
       * well-formed `Request`/URL. Defaults to that same placeholder
       * origin; any override must be `https://` — a plain `http://`
       * override would send the bearer credential in cleartext. */
      readonly baseUrl?: string;
    },
  ) {
    this.baseUrl = normalizeBaseUrl(deps.baseUrl);
    const repoId = deps.repoId;
    if (typeof repoId === "bigint") {
      if (repoId < 0n || repoId > 2n ** 64n - 1n) {
        throw invalid(`repoId must be in 0..2^64-1, got ${String(repoId)}`);
      }
    } else {
      assertUint("repoId", repoId, Number.MAX_SAFE_INTEGER);
    }
  }

  async resolve(request: ShardPlanRequest): Promise<ShardPlan> {
    // Validate and encode first: a bad argument must throw before the
    // token function or `fetch` run, so a client-side validation error is
    // never caught by the transport `try`/`catch` below and rewrapped as a
    // `"transport"` error.
    const requestBody = JSON.stringify({
      repoId: String(this.deps.repoId),
      filePaths: request.filePaths,
      strategy: encodeStrategy(request.strategy),
      count: encodeCount(request.count),
    });

    // Narrowed through `unknown` at every step: the declared type promises
    // `string | () => string | Promise<string>`, but this is caller
    // supplied data/code and untyped JS (or a bug) can hand over anything.
    const tokenSource: unknown = this.deps.token;
    let resolvedToken: unknown;
    if (typeof tokenSource === "string") {
      resolvedToken = tokenSource;
    } else if (typeof tokenSource === "function") {
      try {
        resolvedToken = await tokenSource();
      } catch (err) {
        // No raw error escapes this class's declared `ShardPlanRpcError`
        // contract — a throwing or rejecting token callback surfaces the
        // same way every other client-side validation failure does. The
        // original error is preserved as `cause` so a caller's own
        // error-reporting tooling (which typically walks `.cause`) still
        // sees the token callback's real stack, not just this rewrap's.
        throw invalid(
          scrub(`token callback failed: ${err instanceof Error ? err.message : String(err)}`, ""),
          err,
        );
      }
    } else {
      throw invalid(`token must be a string or a function, got ${typeof tokenSource}`);
    }
    // Sending `Bearer undefined`, `Bearer ` or a header with embedded
    // control characters is a confusing failure far from this call site,
    // so `fetch` never runs with a token that is not a usable credential.
    // None of these messages echo the token value itself.
    if (typeof resolvedToken !== "string") {
      throw invalid(
        `token callback must resolve to a non-empty string, got ${typeof resolvedToken}`,
      );
    }
    if (resolvedToken.length === 0) {
      throw invalid("token must be a non-empty string, got an empty string");
    }
    if (resolvedToken.trim().length === 0) {
      throw invalid("token must be a non-empty string, got a whitespace-only string");
    }
    const hasControlChar = [...resolvedToken].some((ch) => {
      const code = ch.charCodeAt(0);
      return code < 0x20 || code === 0x7f;
    });
    if (hasControlChar) {
      throw invalid("token must not contain control characters");
    }
    const token: string = resolvedToken;
    const url = `${this.baseUrl}/cloud_ci.ingest.v1.IngestService/ResolveShardPlan`;

    let res: { readonly ok: boolean; readonly status: number; json(): Promise<unknown> };
    try {
      res = await this.deps.fetcher.fetch(url, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: `Bearer ${token}`,
        },
        body: requestBody,
      });
    } catch (err) {
      throw new ShardPlanRpcError(
        "transport",
        scrub(err instanceof Error ? err.message : String(err), token),
      );
    }

    let body: unknown;
    try {
      body = await res.json();
    } catch (err) {
      throw new ShardPlanRpcError(
        res.ok ? "malformed_response" : "transport",
        scrub(
          `failed to parse response body as JSON: ${err instanceof Error ? err.message : String(err)}`,
          token,
        ),
        res.status,
      );
    }

    if (!res.ok) {
      const { code, message } = decodeConnectError(body, token);
      throw new ShardPlanRpcError(code, message, res.status);
    }

    return decodeResponse(body, token);
  }
}

/** Platform bound on `ci.shard`'s resolved shard count, mirroring
 * `cloud-ci-core/src/split.rs`'s `MAX_SHARDS` exactly (parallelization.md's
 * `count` column: "int 1-64" — "Clamp shard count to the documented `1..64`
 * range regardless of how it was resolved"). Exported so a future change to
 * either side's bound is a conscious, pinned decision, not a silent drift
 * between the Rust and TypeScript copies of the same constant. */
export const MAX_SHARD_COUNT = 64;

const UINT32_MAX = 4_294_967_295;
/** `google.protobuf.Duration`'s documented maximum magnitude (10 000 years). */
const MAX_DURATION_SECS = 315_576_000_000;
const DEFAULT_BASE_URL = "http://cloud-ci.internal";
const MAX_ECHO_CHARS = 200;
/** Below this length, redacting every occurrence of `token` in echoed text
 * risks mangling ordinary words (e.g. a server message that happens to
 * contain the literal substring "token"). Only bearer credentials long
 * enough to be unambiguous get redacted — see `scrub`'s own doc comment. */
const MIN_REDACTABLE_TOKEN_LENGTH = 8;

function invalid(message: string, cause?: unknown): ShardPlanRpcError {
  return new ShardPlanRpcError("invalid_argument", message, undefined, cause);
}

function malformed(message: string): ShardPlanRpcError {
  return new ShardPlanRpcError("malformed_response", message);
}

/** Redacts every occurrence of the bearer `token` in text that may echo
 * network input (a transport error, a server body), then caps the result
 * at `MAX_ECHO_CHARS`. This is a best-effort exact-string match, not a
 * guarantee: a token under `MIN_REDACTABLE_TOKEN_LENGTH` characters is
 * skipped (redacting a short string risks mangling an unrelated word like
 * "token" in ordinary server text), and a token containing characters a
 * JSON encoder re-escapes (e.g. a literal `"` or `\`) can fail to match
 * the echoed, re-serialized text even when longer. Redaction runs before
 * truncation so a cut can never leave a partial token visible. */
function scrub(text: string, token: string): string {
  const redacted =
    token.length >= MIN_REDACTABLE_TOKEN_LENGTH ? text.split(token).join("[redacted]") : text;
  return redacted.length > MAX_ECHO_CHARS
    ? `${redacted.slice(0, MAX_ECHO_CHARS)}…(truncated)`
    : redacted;
}

function show(value: unknown, token: string): string {
  return scrub(typeof value === "string" ? value : String(JSON.stringify(value)), token);
}

function assertUint(name: string, value: number, max: number): void {
  if (!Number.isInteger(value) || value < 0 || value > max) {
    throw invalid(`${name} must be an integer in 0..${max}, got ${String(value)}`);
  }
}

/** Validates and normalizes `baseUrl` using `new URL()` (not a regex) so
 * scheme/userinfo/host parsing matches the platform's own URL grammar.
 * Accepts only the exact documented internal service-binding default
 * origin, or an `https:` URL with no embedded userinfo, path, query, or
 * fragment — a plain `http://` override would send the bearer credential
 * in cleartext, userinfo in the URL is a second, easily-overlooked place
 * a credential could leak, and a non-root path/query/fragment is
 * silently dropped below (`resolve()` always appends the fixed
 * `IngestService` RPC path to this result) — a caller routing through a
 * path-prefixed gateway would otherwise get a wrong-endpoint 404 with no
 * warning, so this rejects rather than discarding any of them. Rejects
 * any whitespace outright, since the WHATWG `URL` parser silently strips
 * some whitespace variants rather than rejecting them. Compares the
 * *parsed* origin (not the raw string) against the documented default,
 * so `"http://cloud-ci.internal/"` and `"http://cloud-ci.internal//"`
 * are still exactly the default — only a raw string that parses to a
 * *different* origin needs `https:`. */
function normalizeBaseUrl(raw: string | undefined): string {
  if (raw === undefined) {
    return DEFAULT_BASE_URL;
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
  const isDefaultOrigin = url.origin === DEFAULT_BASE_URL;
  if (!isDefaultOrigin && url.protocol.toLowerCase() !== "https:") {
    throw invalid(
      `baseUrl "${describeOrigin(url)}" must use https:, or be the internal service-binding ` +
        `default ${DEFAULT_BASE_URL}`,
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
function describeOrigin(url: URL): string {
  return url.origin === "null" ? url.protocol : url.origin;
}

/** Strips a `user:pass@`/`user@` userinfo prefix from `raw` before it is
 * echoed into an exception message. Defense-in-depth for the two
 * `normalizeBaseUrl` branches above that run *before* `new URL()` has
 * successfully parsed `raw` (whitespace, and an unparsable URL), where
 * `url.origin`'s stronger guarantee isn't available yet — a plain regex
 * rather than `new URL()`, since this runs on strings that may be exactly
 * what `new URL()` itself refuses to parse. */
function redactUserinfoInUrl(raw: string): string {
  return raw.replace(/^([a-zA-Z][a-zA-Z0-9+.-]*:\/\/)[^/?#@]*@/, "$1");
}

function encodeStrategy(strategy: SplitStrategy): string {
  switch (strategy) {
    case "timing":
      return "SPLIT_STRATEGY_TIMING";
    case "file":
      return "SPLIT_STRATEGY_FILE";
    case "count":
      return "SPLIT_STRATEGY_COUNT";
  }
}

/** `ShardCountOption` → `ShardCountSpec`'s flattened-oneof proto3-JSON
 * shape: `{ fixed: <uint32> }` or `{ range: { min, max, target } }`,
 * `target` as a `google.protobuf.Duration` `"<seconds>s"` string. Numeric
 * fields are validated here so a bad value fails before any network call. */
function encodeCount(count: ShardCountOption): unknown {
  if (typeof count === "number") {
    assertUint("count", count, UINT32_MAX);
    return { fixed: count };
  }
  assertUint("count.min", count.min, UINT32_MAX);
  assertUint("count.max", count.max, UINT32_MAX);
  if (count.min > count.max) {
    throw invalid(`count.min (${count.min}) must be <= count.max (${count.max})`);
  }
  return {
    range: {
      min: count.min,
      max: count.max,
      target: `${parseDurationToSeconds(count.target)}s`,
    },
  };
}

/** Parses the `"5m"`/`"30s"`/`"1h"`-style duration strings
 * `ShardCountRange.target`'s doc comment (`types.ts`) documents into whole
 * seconds for `google.protobuf.Duration`'s wire format. Only the three
 * suffixes that doc comment names are accepted — anything else throws
 * rather than silently defaulting to a unit — and a non-finite or
 * out-of-range result is rejected too. */
function parseDurationToSeconds(target: string): number {
  const match = /^(\d+)(s|m|h)$/.exec(target.trim());
  const amountStr = match?.[1];
  const unit = match?.[2];
  if (amountStr === undefined || unit === undefined) {
    throw invalid(
      `count.target "${target}" is not a valid duration — expected a plain integer followed ` +
        `by "s", "m", or "h" (e.g. "30s", "5m", "1h")`,
    );
  }
  const amount = Number(amountStr);
  let seconds = amount * 3600;
  if (unit === "s") {
    seconds = amount;
  } else if (unit === "m") {
    seconds = amount * 60;
  }
  if (!Number.isFinite(seconds) || seconds > MAX_DURATION_SECS) {
    throw invalid(`count.target "${target}" is out of range (max ${MAX_DURATION_SECS}s)`);
  }
  return seconds;
}

/** `ResolveShardPlanResponse`'s proto3-JSON shape → `ShardPlan`. Narrows
 * the untrusted parsed-JSON `body` field by field (`in`/`typeof`) rather
 * than casting it, since this is unvalidated network input. Strict by
 * design: a missing `shards` array, a non-integer or out-of-`1..MAX_SHARD_COUNT`
 * `shardCount`, or a `shardCount`/`shards.length` mismatch are all
 * `malformed_response` rather than a silently truncated plan — a
 * `{"shardCount":4}` body with no `shards` must never resolve as "4
 * shards, 0 files assigned". */
function decodeResponse(body: unknown, token: string): ShardPlan {
  if (typeof body !== "object" || body === null) {
    throw malformed(`expected a ResolveShardPlanResponse JSON object, got ${show(body, token)}`);
  }
  if (
    !("shardCount" in body) ||
    typeof body.shardCount !== "number" ||
    !Number.isInteger(body.shardCount) ||
    body.shardCount < 1 ||
    body.shardCount > MAX_SHARD_COUNT
  ) {
    throw malformed(
      `"shardCount" must be an integer in 1..${MAX_SHARD_COUNT} in ResolveShardPlanResponse: ` +
        `${show(body, token)}`,
    );
  }
  const shardCount = body.shardCount;
  if (!("shards" in body) || !Array.isArray(body.shards)) {
    throw malformed(
      `"shards" is missing or not an array in ResolveShardPlanResponse: ${show(body, token)}`,
    );
  }
  if (body.shards.length !== shardCount) {
    throw malformed(
      `"shardCount" (${shardCount}) does not match "shards".length (${body.shards.length}) in ` +
        `ResolveShardPlanResponse: ${show(body, token)}`,
    );
  }
  const files = body.shards.map((shard) => decodeShardFiles(shard, body, token));
  return { shardCount, files };
}

/** A `{}` entry is a valid `ShardFiles` — the server omits an empty
 * `files` vec rather than sending `"files":[]` (`buffa`'s
 * `skip_serializing_if = "is_empty_vec"`). */
function decodeShardFiles(shard: unknown, response: unknown, token: string): readonly string[] {
  if (typeof shard !== "object" || shard === null) {
    throw malformed(
      `malformed ShardFiles entry in ResolveShardPlanResponse: ${show(response, token)}`,
    );
  }
  if (!("files" in shard) || shard.files === undefined) {
    return [];
  }
  if (!isStringArray(shard.files)) {
    throw malformed(
      `malformed ShardFiles entry in ResolveShardPlanResponse: ${show(response, token)}`,
    );
  }
  return shard.files;
}

/** Type guard (preserves narrowing for `decodeShardFiles`'s caller) —
 * exempt from the tiny-function inlining convention for that reason. */
function isStringArray(v: unknown): v is string[] {
  return Array.isArray(v) && v.every((e) => typeof e === "string");
}

/** Connect error body (`cloud-ci-worker/src/connect.rs`'s `ErrorBody`):
 * `{ code, message }`. Falls back to a `"transport"` code with the raw
 * body when the error body itself doesn't match that shape (e.g. a
 * non-Connect-aware proxy's HTML error page) — narrowed via `in`/`typeof`,
 * never cast, since this is unvalidated network input. */
function decodeConnectError(body: unknown, token: string): { code: string; message: string } {
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
