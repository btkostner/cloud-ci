import { describe, expect, it } from "vitest";
import { ShardGroupRegisterRpcError } from "../src/errors.js";
import { RpcShardGroupRegistrar } from "../src/rpc-shard-group-registrar.js";
import type { ShardPlanFetcher, ShardPlanFetchInit } from "../src/types.js";

/** In-process fake `ShardPlanFetcher` — explicitly NOT a real
 * `CONTAINER_WORKER` service binding reaching a deployed `cloud-ci-worker`.
 * Proves the request/response wire shape `RpcShardGroupRegistrar` builds
 * and parses, not real network behavior — every assertion in this file is
 * unit-only, same posture as `rpc-shard-planner.test.ts`. */
class FakeFetcher implements ShardPlanFetcher {
  readonly calls: { url: string; init: ShardPlanFetchInit }[] = [];
  private readonly handler: (
    url: string,
    init: ShardPlanFetchInit,
  ) => { status: number; body: unknown } | Promise<{ status: number; body: unknown }>;

  constructor(
    handler: (
      url: string,
      init: ShardPlanFetchInit,
    ) => { status: number; body: unknown } | Promise<{ status: number; body: unknown }>,
  ) {
    this.handler = handler;
  }

  async fetch(url: string, init: ShardPlanFetchInit) {
    this.calls.push({ url, init });
    const { status, body } = await this.handler(url, init);
    return {
      ok: status >= 200 && status < 300,
      status,
      json: async () => body,
    };
  }
}

/** Awaits `promise`, asserting it rejects with a `ShardGroupRegisterRpcError`
 * and returning it narrowed — same helper shape
 * `rpc-shard-planner.test.ts`'s own `rejects` uses. */
async function rejects(promise: Promise<unknown>): Promise<ShardGroupRegisterRpcError> {
  try {
    await promise;
  } catch (err) {
    if (err instanceof ShardGroupRegisterRpcError) {
      return err;
    }
    throw err;
  }
  throw new Error("expected promise to reject with ShardGroupRegisterRpcError");
}

describe("RpcShardGroupRegistrar", () => {
  it("POSTs the real Connect JSON request shape to RegisterShardGroup", async () => {
    const fetcher = new FakeFetcher(() => ({
      status: 200,
      body: { jobName: "e2e" },
    }));
    const registrar = new RpcShardGroupRegistrar({ fetcher, runId: "run_abc", token: "tok_abc" });

    const result = await registrar.register({
      jobName: "e2e",
      expectedTotal: 4,
      failFast: true,
      mergeOnFailure: "always",
    });

    expect(fetcher.calls).toHaveLength(1);
    const call = fetcher.calls[0];
    expect(call).toBeDefined();
    expect(call?.url).toBe(
      "http://cloud-ci.internal/cloud_ci.ingest.v1.IngestService/RegisterShardGroup",
    );
    expect(call?.init.method).toBe("POST");
    expect(call?.init.headers["content-type"]).toBe("application/json");
    expect(call?.init.headers.authorization).toBe("Bearer tok_abc");
    expect(JSON.parse(call?.init.body ?? "")).toEqual({
      runId: "run_abc",
      jobName: "e2e",
      expectedTotal: 4,
      failFast: true,
      mergeOnFailure: "always",
    });
    expect(result).toEqual({ jobName: "e2e" });
  });

  it("calling register() twice with identical fields is idempotent — two real requests, same result both times", async () => {
    // `RegisterShardGroup` itself is server-side idempotent (a redelivered
    // call for an already-registered job_name is a clean no-op per
    // coordinator::mod's own doc comment); this client has no local cache
    // or dedupe layer of its own, so a retry is simply a second real POST
    // that resolves to the same `ShardGroupRegisterResult` the first did.
    const fetcher = new FakeFetcher(() => ({
      status: 200,
      body: { jobName: "e2e" },
    }));
    const registrar = new RpcShardGroupRegistrar({ fetcher, runId: "run_abc", token: "tok_abc" });
    const request = {
      jobName: "e2e",
      expectedTotal: 2,
      failFast: false,
      mergeOnFailure: "if_any_passed" as const,
    };

    const first = await registrar.register(request);
    const second = await registrar.register(request);

    expect(fetcher.calls).toHaveLength(2);
    expect(first).toEqual({ jobName: "e2e" });
    expect(second).toEqual({ jobName: "e2e" });
  });

  it("maps a Connect error body to ShardGroupRegisterRpcError with its code, message, and HTTP status", async () => {
    const fetcher = new FakeFetcher(() => ({
      status: 403,
      body: { code: "permission_denied", message: "ingest token does not match this run" },
    }));
    const registrar = new RpcShardGroupRegistrar({ fetcher, runId: "run_abc", token: "tok_abc" });

    const err = await rejects(
      registrar.register({
        jobName: "e2e",
        expectedTotal: 1,
        failFast: false,
        mergeOnFailure: "if_any_passed",
      }),
    );

    expect(err.code).toBe("permission_denied");
    expect(err.httpStatus).toBe(403);
    expect(err.message).toContain("ingest token does not match this run");
  });

  it("throws malformed_response for a 200 body missing jobName", async () => {
    const fetcher = new FakeFetcher(() => ({
      status: 200,
      body: {},
    }));
    const registrar = new RpcShardGroupRegistrar({ fetcher, runId: "run_abc", token: "tok_abc" });

    const err = await rejects(
      registrar.register({
        jobName: "e2e",
        expectedTotal: 1,
        failFast: false,
        mergeOnFailure: "if_any_passed",
      }),
    );

    expect(err.code).toBe("malformed_response");
  });

  it("throws malformed_response for a 200 body with a non-string jobName", async () => {
    const fetcher = new FakeFetcher(() => ({
      status: 200,
      body: { jobName: 42 },
    }));
    const registrar = new RpcShardGroupRegistrar({ fetcher, runId: "run_abc", token: "tok_abc" });

    const err = await rejects(
      registrar.register({
        jobName: "e2e",
        expectedTotal: 1,
        failFast: false,
        mergeOnFailure: "if_any_passed",
      }),
    );

    expect(err.code).toBe("malformed_response");
  });

  it("throws malformed_response when the response jobName does not match the request's own jobName", async () => {
    const fetcher = new FakeFetcher(() => ({
      status: 200,
      body: { jobName: "a-different-job" },
    }));
    const registrar = new RpcShardGroupRegistrar({ fetcher, runId: "run_abc", token: "tok_abc" });

    const err = await rejects(
      registrar.register({
        jobName: "e2e",
        expectedTotal: 1,
        failFast: false,
        mergeOnFailure: "if_any_passed",
      }),
    );

    expect(err.code).toBe("malformed_response");
    expect(err.message).toContain("e2e");
    expect(err.message).toContain("a-different-job");
  });

  it("throws a transport ShardGroupRegisterRpcError when the Fetcher itself rejects", async () => {
    const fetcher: ShardPlanFetcher = {
      fetch: async () => {
        throw new Error("service binding unavailable");
      },
    };
    const registrar = new RpcShardGroupRegistrar({ fetcher, runId: "run_abc", token: "tok_abc" });

    const err = await rejects(
      registrar.register({
        jobName: "e2e",
        expectedTotal: 1,
        failFast: false,
        mergeOnFailure: "if_any_passed",
      }),
    );

    expect(err.code).toBe("transport");
    expect(err.message).toContain("service binding unavailable");
  });

  it("falls back to a transport ShardGroupRegisterRpcError when the error body isn't Connect-shaped", async () => {
    const fetcher = new FakeFetcher(() => ({
      status: 502,
      body: "<html>Bad Gateway</html>",
    }));
    const registrar = new RpcShardGroupRegistrar({ fetcher, runId: "run_abc", token: "tok_abc" });

    const err = await rejects(
      registrar.register({
        jobName: "e2e",
        expectedTotal: 1,
        failFast: false,
        mergeOnFailure: "if_any_passed",
      }),
    );

    expect(err.code).toBe("transport");
    expect(err.httpStatus).toBe(502);
  });

  it("rejects a non-positive expectedTotal before any fetch", async () => {
    const fetcher = new FakeFetcher(() => ({ status: 200, body: { jobName: "e2e" } }));
    const registrar = new RpcShardGroupRegistrar({ fetcher, runId: "run_abc", token: "tok_abc" });

    await expect(
      registrar.register({
        jobName: "e2e",
        expectedTotal: 0,
        failFast: false,
        mergeOnFailure: "if_any_passed",
      }),
    ).rejects.toThrow(ShardGroupRegisterRpcError);
    expect(fetcher.calls).toHaveLength(0);
  });

  it("rejects a non-integer expectedTotal before any fetch", async () => {
    const fetcher = new FakeFetcher(() => ({ status: 200, body: { jobName: "e2e" } }));
    const registrar = new RpcShardGroupRegistrar({ fetcher, runId: "run_abc", token: "tok_abc" });

    await expect(
      registrar.register({
        jobName: "e2e",
        expectedTotal: 1.5,
        failFast: false,
        mergeOnFailure: "if_any_passed",
      }),
    ).rejects.toThrow(ShardGroupRegisterRpcError);
    expect(fetcher.calls).toHaveLength(0);
  });

  it("rejects an empty jobName before any fetch", async () => {
    const fetcher = new FakeFetcher(() => ({ status: 200, body: { jobName: "e2e" } }));
    const registrar = new RpcShardGroupRegistrar({ fetcher, runId: "run_abc", token: "tok_abc" });

    await expect(
      registrar.register({
        jobName: "",
        expectedTotal: 1,
        failFast: false,
        mergeOnFailure: "if_any_passed",
      }),
    ).rejects.toThrow(ShardGroupRegisterRpcError);
    expect(fetcher.calls).toHaveLength(0);
  });

  it("sends failFast and mergeOnFailure through unchanged for every documented mergeOnFailure value", async () => {
    for (const mergeOnFailure of ["if_any_passed", "always", "never"] as const) {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: { jobName: "e2e" } }));
      const registrar = new RpcShardGroupRegistrar({
        fetcher,
        runId: "run_abc",
        token: "tok_abc",
      });

      await registrar.register({
        jobName: "e2e",
        expectedTotal: 1,
        failFast: false,
        mergeOnFailure,
      });

      const body = JSON.parse(fetcher.calls[0]?.init.body ?? "{}");
      expect(body.mergeOnFailure).toBe(mergeOnFailure);
    }
  });

  it("rejects an empty runId in the constructor, before any register() call", () => {
    const fetcher = new FakeFetcher(() => ({ status: 200, body: { jobName: "e2e" } }));
    expect(() => new RpcShardGroupRegistrar({ fetcher, runId: "", token: "tok" })).toThrowError(
      ShardGroupRegisterRpcError,
    );
  });

  it("rejects a baseUrl that is not https and not the internal default", () => {
    const fetcher = new FakeFetcher(() => ({ status: 200, body: { jobName: "e2e" } }));
    expect(
      () =>
        new RpcShardGroupRegistrar({
          fetcher,
          runId: "run_abc",
          token: "tok",
          baseUrl: "http://evil.example.com",
        }),
    ).toThrowError(ShardGroupRegisterRpcError);
  });

  it("rejects a token callback that resolves to an empty string, never calling fetch", async () => {
    const fetcher = new FakeFetcher(() => ({ status: 200, body: { jobName: "e2e" } }));
    const registrar = new RpcShardGroupRegistrar({
      fetcher,
      runId: "run_abc",
      token: () => "",
    });

    await expect(
      registrar.register({
        jobName: "e2e",
        expectedTotal: 1,
        failFast: false,
        mergeOnFailure: "if_any_passed",
      }),
    ).rejects.toThrow(ShardGroupRegisterRpcError);
    expect(fetcher.calls).toHaveLength(0);
  });

  it("scrubs the bearer token out of an echoed transport error message", async () => {
    const fetcher: ShardPlanFetcher = {
      fetch: async () => {
        throw new Error("upstream said: Bearer super-secret-token-value failed");
      },
    };
    const registrar = new RpcShardGroupRegistrar({
      fetcher,
      runId: "run_abc",
      token: "super-secret-token-value",
    });

    const err = await rejects(
      registrar.register({
        jobName: "e2e",
        expectedTotal: 1,
        failFast: false,
        mergeOnFailure: "if_any_passed",
      }),
    );

    expect(err.message).not.toContain("super-secret-token-value");
    expect(err.message).toContain("[redacted]");
  });
});
