import {
  assertUint,
  decodeConnectError,
  normalizeBaseUrl,
  resolveBearerToken,
  scrub,
  show,
} from "./connect-rpc-support.js";
import { ShardGroupRegisterRpcError } from "./errors.js";
import type {
  ShardGroupRegisterRequest,
  ShardGroupRegisterResult,
  ShardGroupRegistrar,
  ShardPlanFetcher,
} from "./types.js";

/**
 * Real `ShardGroupRegistrar` — reaches `cloud-ci-worker`'s
 * `RegisterShardGroup` RPC (`cloud_ci.ingest.v1.IngestService/RegisterShardGroup`,
 * `packages/cloud-ci-worker/src/lib.rs`'s `handle_register_shard_group`)
 * over an injected `ShardPlanFetcher` (a `CONTAINER_WORKER`-style service
 * binding — see `RpcShardPlanner`'s own doc comment, `rpc-shard-planner.ts`,
 * for the full wire-format background this class speaks identically:
 * Connect unary-JSON, `POST` to
 * `/cloud_ci.ingest.v1.IngestService/RegisterShardGroup`,
 * `content-type: application/json`, a plain proto3-JSON request/response
 * body, and a Connect error body (`{ code, message }`) on failure).
 *
 * `RegisterShardGroup` authenticates and authorizes exactly like every
 * other run-bound ingest call (`StartJob`, `SubmitReport`, ...,
 * `handle_register_shard_group`'s own doc comment, docs/design/auth.md's
 * Machine auth section): the bearer must be the run's own ingest token
 * (minted by `BeginRun`), cross-checked against `runId`'s real owning run
 * server-side. This is a *different* credential than
 * `RpcShardPlanner`'s own `repoId`/`token` (a repo-scoped GitHub Actions
 * OIDC JWT or scoped API token, reusing the `BeginRun` credential model) —
 * `runId`/`token` here must be the run-scoped ingest token, not a
 * repo-scoped one. Neither `workflow.ts` nor any host package in this
 * repo constructs and injects a real instance of this class yet (there is
 * no run-scoped-ingest-token plumbing anywhere in this SDK or
 * `cloud-ci-dynamic-workflows-host` today) — same documented scope
 * boundary `RpcShardPlanner`'s own doc comment calls out for its own
 * credential ("wiring this call to the real `RunCoordinator`/
 * ingest-token flow ... is explicit follow-up"). A caller that has that
 * token (a future managed-run host) constructs this class directly, the
 * same way it would construct `RpcShardPlanner`.
 */
export class RpcShardGroupRegistrar implements ShardGroupRegistrar {
  private readonly baseUrl: string;

  constructor(
    private readonly deps: {
      readonly fetcher: ShardPlanFetcher;
      /** The managed run's id (`BeginRunResponse.run_id`) — resolves which
       * run's `RunCoordinator` owns the group being registered. Not part
       * of the per-call `register()` request; see `ShardGroupRegisterRequest`'s
       * own doc comment for why. */
      readonly runId: string;
      /** The run's own ingest token (`BeginRunResponse.ingest_token`),
       * sent as `Authorization: Bearer` — see this class's own doc
       * comment for exactly which credential `RegisterShardGroup`
       * requires and why it differs from `RpcShardPlanner`'s. */
      readonly token: string | (() => string | Promise<string>);
      /** Origin the request URL is built against — same convention and
       * same default as `RpcShardPlanner`'s own `baseUrl` dep; see that
       * class's doc comment. */
      readonly baseUrl?: string;
    },
  ) {
    this.baseUrl = normalizeBaseUrl(deps.baseUrl, DEFAULT_BASE_URL, invalid);
    if (deps.runId.length === 0) {
      throw invalid("runId must be a non-empty string, got an empty string");
    }
  }

  async register(request: ShardGroupRegisterRequest): Promise<ShardGroupRegisterResult> {
    // Validate and encode first: a bad argument must throw before the
    // token function or `fetch` run, matching `RpcShardPlanner.resolve()`'s
    // own ordering (a client-side validation error must never be caught
    // by the transport `try`/`catch` below and rewrapped as `"transport"`).
    if (request.jobName.length === 0) {
      throw invalid("jobName must be a non-empty string, got an empty string");
    }
    assertUint("expectedTotal", request.expectedTotal, UINT32_MAX, invalid);
    if (request.expectedTotal < 1) {
      throw invalid(`expectedTotal must be at least 1, got ${request.expectedTotal}`);
    }
    const requestBody = JSON.stringify({
      runId: this.deps.runId,
      jobName: request.jobName,
      expectedTotal: request.expectedTotal,
      failFast: request.failFast,
      mergeOnFailure: request.mergeOnFailure,
    });

    const token = await resolveBearerToken(this.deps.token, invalid);
    const url = `${this.baseUrl}/cloud_ci.ingest.v1.IngestService/RegisterShardGroup`;

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
      throw new ShardGroupRegisterRpcError(
        "transport",
        scrub(err instanceof Error ? err.message : String(err), token),
      );
    }

    let body: unknown;
    try {
      body = await res.json();
    } catch (err) {
      throw new ShardGroupRegisterRpcError(
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
      throw new ShardGroupRegisterRpcError(code, message, res.status);
    }

    return decodeResponse(body, token);
  }
}

const UINT32_MAX = 4_294_967_295;
const DEFAULT_BASE_URL = "http://cloud-ci.internal";

function invalid(message: string, cause?: unknown): ShardGroupRegisterRpcError {
  return new ShardGroupRegisterRpcError("invalid_argument", message, undefined, cause);
}

function malformed(message: string): ShardGroupRegisterRpcError {
  return new ShardGroupRegisterRpcError("malformed_response", message);
}

/** `RegisterShardGroupResponse`'s proto3-JSON shape → `ShardGroupRegisterResult`.
 * Narrows the untrusted parsed-JSON `body` field by field (`in`/`typeof`)
 * rather than casting it, same fail-closed discipline as
 * `rpc-shard-planner.ts`'s `decodeResponse` — a missing or non-string
 * `jobName` is `malformed_response`, never silently coerced to `""`. */
function decodeResponse(body: unknown, token: string): ShardGroupRegisterResult {
  if (typeof body !== "object" || body === null) {
    throw malformed(`expected a RegisterShardGroupResponse JSON object, got ${show(body, token)}`);
  }
  if (!("jobName" in body) || typeof body.jobName !== "string" || body.jobName.length === 0) {
    throw malformed(
      `"jobName" must be a non-empty string in RegisterShardGroupResponse: ${show(body, token)}`,
    );
  }
  return { jobName: body.jobName };
}
