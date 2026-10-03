import {
  assertUint,
  decodeConnectError,
  normalizeBaseUrl,
  resolveBearerToken,
  scrub,
  show,
} from "./connect-rpc-support.js";
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
    this.baseUrl = normalizeBaseUrl(deps.baseUrl, DEFAULT_BASE_URL, invalid);
    const repoId = deps.repoId;
    if (typeof repoId === "bigint") {
      if (repoId < 0n || repoId > 2n ** 64n - 1n) {
        throw invalid(`repoId must be in 0..2^64-1, got ${String(repoId)}`);
      }
    } else {
      assertUint("repoId", repoId, Number.MAX_SAFE_INTEGER, invalid);
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

    const token = await resolveBearerToken(this.deps.token, invalid);
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

function invalid(message: string, cause?: unknown): ShardPlanRpcError {
  return new ShardPlanRpcError("invalid_argument", message, undefined, cause);
}

function malformed(message: string): ShardPlanRpcError {
  return new ShardPlanRpcError("malformed_response", message);
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
    assertUint("count", count, UINT32_MAX, invalid);
    return { fixed: count };
  }
  assertUint("count.min", count.min, UINT32_MAX, invalid);
  assertUint("count.max", count.max, UINT32_MAX, invalid);
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
