import { describe, expect, it } from "vitest";
import { ShardPlanRpcError } from "../src/errors.js";
import { MAX_SHARD_COUNT as MAX_SHARD_COUNT_FROM_INDEX } from "../src/index.js";
import { MAX_SHARD_COUNT, RpcShardPlanner } from "../src/rpc-shard-planner.js";
import type { ShardPlanFetcher, ShardPlanFetchInit } from "../src/types.js";

/** In-process fake `ShardPlanFetcher` — explicitly NOT a real
 * `CONTAINER_WORKER` service binding reaching a deployed `cloud-ci-worker`
 * (see `RpcShardPlanner`'s doc comment). Proves the request/response wire
 * shape `RpcShardPlanner` builds and parses, not real network behavior —
 * every assertion in this file is unit-only. */
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

/** Fake whose `.json()` itself rejects, with a caller-chosen error message
 * — used both for the plain "unparsable JSON" tests and for proving that
 * message gets scrubbed. */
class JsonRejectingFetcher implements ShardPlanFetcher {
  constructor(
    private readonly status: number,
    private readonly errorMessage = "Unexpected token < in JSON at position 0",
  ) {}

  async fetch() {
    return {
      ok: this.status >= 200 && this.status < 300,
      status: this.status,
      json: async () => {
        throw new SyntaxError(this.errorMessage);
      },
    };
  }
}

/** Awaits `promise`, asserting it rejects with a `ShardPlanRpcError` and
 * returning it narrowed — used across every error-mapping test below so
 * each one can assert on `code`/`httpStatus`/`message` without an inline
 * `as ShardPlanRpcError` cast on a caught `unknown`. */
async function rejects(promise: Promise<unknown>): Promise<ShardPlanRpcError> {
  try {
    await promise;
  } catch (err) {
    if (err instanceof ShardPlanRpcError) {
      return err;
    }
    throw err;
  }
  throw new Error("expected promise to reject with ShardPlanRpcError");
}

describe("RpcShardPlanner", () => {
  it("POSTs the real Connect JSON request shape to ResolveShardPlan with a fixed count", async () => {
    const fetcher = new FakeFetcher(() => ({
      status: 200,
      body: { shardCount: 2, shards: [{ files: ["a.ts"] }, { files: ["b.ts"] }] },
    }));
    const planner = new RpcShardPlanner({ fetcher, repoId: 42, token: "tok_abc" });

    const plan = await planner.resolve({
      filePaths: ["a.ts", "b.ts"],
      strategy: "file",
      count: 2,
    });

    expect(fetcher.calls).toHaveLength(1);
    const call = fetcher.calls[0];
    expect(call).toBeDefined();
    expect(call?.url).toBe(
      "http://cloud-ci.internal/cloud_ci.ingest.v1.IngestService/ResolveShardPlan",
    );
    expect(call?.init.method).toBe("POST");
    expect(call?.init.headers["content-type"]).toBe("application/json");
    expect(call?.init.headers.authorization).toBe("Bearer tok_abc");
    // `repoId` is a proto `uint64`, encoded as a quoted decimal string on
    // the wire (`buffa`'s generated `json_helpers::uint64`), never a bare
    // number — distinct from `shardCount`/`min`/`max`, which are `uint32`
    // and stay bare numbers.
    expect(JSON.parse(call?.init.body ?? "")).toEqual({
      repoId: "42",
      filePaths: ["a.ts", "b.ts"],
      strategy: "SPLIT_STRATEGY_FILE",
      count: { fixed: 2 },
    });
    expect(plan).toEqual({ shardCount: 2, files: [["a.ts"], ["b.ts"]] });
  });

  it("encodes a {min,max,target} auto-sizing count as a Duration string and SPLIT_STRATEGY_TIMING", async () => {
    const fetcher = new FakeFetcher(() => ({
      status: 200,
      body: { shardCount: 1, shards: [{ files: [] }] },
    }));
    const planner = new RpcShardPlanner({ fetcher, repoId: 7, token: "tok" });

    await planner.resolve({
      filePaths: [],
      strategy: "timing",
      count: { min: 2, max: 16, target: "8m" },
    });

    const call = fetcher.calls[0];
    expect(JSON.parse(call?.init.body ?? "")).toEqual({
      repoId: "7",
      filePaths: [],
      strategy: "SPLIT_STRATEGY_TIMING",
      count: { range: { min: 2, max: 16, target: "480s" } },
    });
  });

  it("decodes a {} shard entry as an empty file list (the server omits an empty files vec)", async () => {
    const fetcher = new FakeFetcher(() => ({
      status: 200,
      body: { shardCount: 2, shards: [{ files: ["a.ts"] }, {}] },
    }));
    const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

    const plan = await planner.resolve({ filePaths: ["a.ts"], strategy: "file", count: 2 });

    expect(plan).toEqual({ shardCount: 2, files: [["a.ts"], []] });
  });

  it("accepts shardCount at exactly MAX_SHARD_COUNT (64)", async () => {
    const fetcher = new FakeFetcher(() => ({
      status: 200,
      body: {
        shardCount: MAX_SHARD_COUNT,
        shards: Array.from({ length: MAX_SHARD_COUNT }, () => ({ files: [] })),
      },
    }));
    const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

    const plan = await planner.resolve({
      filePaths: [],
      strategy: "file",
      count: MAX_SHARD_COUNT,
    });

    expect(plan.shardCount).toBe(MAX_SHARD_COUNT);
    expect(plan.files).toHaveLength(MAX_SHARD_COUNT);
  });

  it("pins MAX_SHARD_COUNT at 64, mirroring cloud-ci-core/src/split.rs's MAX_SHARDS", () => {
    expect(MAX_SHARD_COUNT).toBe(64);
  });

  it("re-exports MAX_SHARD_COUNT from the package root", () => {
    expect(MAX_SHARD_COUNT_FROM_INDEX).toBe(MAX_SHARD_COUNT);
  });

  it("calls a function token fresh on every resolve()", async () => {
    let calls = 0;
    const fetcher = new FakeFetcher(() => ({
      status: 200,
      body: { shardCount: 1, shards: [{ files: [] }] },
    }));
    const planner = new RpcShardPlanner({
      fetcher,
      repoId: 1,
      token: () => {
        calls += 1;
        return `tok_${calls}`;
      },
    });

    await planner.resolve({ filePaths: [], strategy: "file", count: 1 });
    await planner.resolve({ filePaths: [], strategy: "file", count: 1 });

    expect(fetcher.calls[0]?.init.headers.authorization).toBe("Bearer tok_1");
    expect(fetcher.calls[1]?.init.headers.authorization).toBe("Bearer tok_2");
  });

  describe("token callback failures", () => {
    it("rewraps a throwing token callback as invalid_argument, with no raw error escaping", async () => {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
      const thrown = new Error("vault lookup failed");
      const planner = new RpcShardPlanner({
        fetcher,
        repoId: 1,
        token: () => {
          throw thrown;
        },
      });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("invalid_argument");
      expect(err.message).toContain("vault lookup failed");
      expect(fetcher.calls).toHaveLength(0);
      // U4: the original error is preserved as `.cause`, not dropped —
      // a caller's error-reporting tooling that walks `.cause` still
      // sees the real failure (and its stack), not just the rewrap.
      expect(err.cause).toBe(thrown);
    });

    it("rewraps a rejecting token callback as invalid_argument, with no raw error escaping", async () => {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
      const thrown = new Error("token endpoint timed out");
      const planner = new RpcShardPlanner({
        fetcher,
        repoId: 1,
        token: async () => {
          throw thrown;
        },
      });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("invalid_argument");
      expect(err.message).toContain("token endpoint timed out");
      expect(fetcher.calls).toHaveLength(0);
      expect(err.cause).toBe(thrown);
    });

    it("rejects an empty string returned by a token callback, with no fetch", async () => {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: () => "" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("invalid_argument");
      expect(err.message).toContain("non-empty string");
      expect(fetcher.calls).toHaveLength(0);
      // V5: an empty-token rejection is this call's own client-side
      // validation failure, not a rewrap of anything — it must not carry
      // a `cause` key at all (as opposed to the throwing/rejecting
      // callback tests above, which do).
      expect("cause" in err).toBe(false);
    });

    it("rejects an empty string token given directly (not via a callback), with no fetch", async () => {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("invalid_argument");
      expect(err.message).toContain("non-empty string");
      expect(fetcher.calls).toHaveLength(0);
      expect("cause" in err).toBe(false);
    });

    it("rejects a whitespace-only string returned by a token callback, with no fetch", async () => {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: () => "   " });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("invalid_argument");
      expect(err.message).toContain("whitespace-only");
      expect(fetcher.calls).toHaveLength(0);
    });

    it("rejects a whitespace-only token given directly, with no fetch", async () => {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "\t \n" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("invalid_argument");
      expect(err.message).toContain("whitespace-only");
      expect(fetcher.calls).toHaveLength(0);
    });

    it.each(["a\nb", "a\rb", "a\tb", "a\u0000b", "a\u007fb"])(
      "rejects a token containing a control character (%j), with no fetch",
      async (token) => {
        const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
        const planner = new RpcShardPlanner({ fetcher, repoId: 1, token });

        const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

        expect(err.code).toBe("invalid_argument");
        expect(err.message).toContain("control characters");
        expect(fetcher.calls).toHaveLength(0);
      },
    );

    /** Builds a `planner`/`fetcher` pair with `token` bypassing its
     * declared type — used only to prove `resolve()`'s own runtime
     * validation (not TypeScript) is what rejects a value the type system
     * would normally forbid (V4: a non-string, non-function `token`; V5: a
     * callback resolving to something other than a string). */
    function plannerWithToken(token: unknown): { planner: RpcShardPlanner; fetcher: FakeFetcher } {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
      const deps = {
        fetcher,
        repoId: 1,
        token,
      } as unknown as ConstructorParameters<typeof RpcShardPlanner>[0];
      return { planner: new RpcShardPlanner(deps), fetcher };
    }

    it.each([undefined, 42])(
      "rejects a token that is neither a string nor a function (%j), with no fetch and no cause",
      async (token) => {
        const { planner, fetcher } = plannerWithToken(token);

        const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

        expect(err.code).toBe("invalid_argument");
        expect(err.message).toBe(
          `ResolveShardPlan failed (invalid_argument): token must be a string or a function, ` +
            `got ${typeof token}`,
        );
        expect(fetcher.calls).toHaveLength(0);
        expect("cause" in err).toBe(false);
      },
    );

    it.each([undefined, null, 42])(
      "rejects a token callback that resolves to %j with a ShardPlanRpcError, not a raw TypeError",
      async (returned) => {
        const { planner, fetcher } = plannerWithToken(() => returned);

        const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

        expect(err.code).toBe("invalid_argument");
        expect(err.message).toContain(typeof returned);
        expect(fetcher.calls).toHaveLength(0);
      },
    );
  });

  describe("response validation (malformed_response)", () => {
    it("rejects a response missing the shards array even when shardCount is valid", async () => {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: { shardCount: 4 } }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 4 }));

      expect(err.code).toBe("malformed_response");
    });

    it("rejects a non-array shards field", async () => {
      const fetcher = new FakeFetcher(() => ({
        status: 200,
        body: { shardCount: 2, shards: "x" },
      }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 2 }));

      expect(err.code).toBe("malformed_response");
    });

    it("rejects a shardCount/shards.length mismatch with the exact message", async () => {
      const fetcher = new FakeFetcher(() => ({
        status: 200,
        body: { shardCount: 3, shards: [{ files: [] }, { files: [] }] },
      }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 3 }));

      expect(err.code).toBe("malformed_response");
      expect(err.message).toBe(
        'ResolveShardPlan failed (malformed_response): "shardCount" (3) does not match ' +
          '"shards".length (2) in ResolveShardPlanResponse: ' +
          '{"shardCount":3,"shards":[{"files":[]},{"files":[]}]}',
      );
    });

    it.each([0, MAX_SHARD_COUNT + 1, 1.5])(
      `rejects shardCount %s as outside the valid 1..${MAX_SHARD_COUNT} integer range`,
      async (shardCount) => {
        const fetcher = new FakeFetcher(() => ({
          status: 200,
          body: { shardCount, shards: [] },
        }));
        const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

        const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

        expect(err.code).toBe("malformed_response");
      },
    );

    it("rejects a shard entry whose files field is not a string array", async () => {
      const fetcher = new FakeFetcher(() => ({
        status: 200,
        body: { shardCount: 1, shards: [{ files: [123] }] },
      }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("malformed_response");
    });

    it("maps unparsable JSON on a 200 to malformed_response", async () => {
      const planner = new RpcShardPlanner({
        fetcher: new JsonRejectingFetcher(200),
        repoId: 1,
        token: "tok",
      });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("malformed_response");
    });
  });

  describe("error mapping", () => {
    it("maps a Connect error body to ShardPlanRpcError with its code, message, and HTTP status", async () => {
      const fetcher = new FakeFetcher(() => ({
        status: 401,
        body: {
          code: "unauthenticated",
          message:
            "ResolveShardPlan requires a credential: GitHub Actions OIDC JWT or a scoped API token",
        },
      }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "irrelevant" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("unauthenticated");
      expect(err.httpStatus).toBe(401);
      expect(err.message).toContain("GitHub Actions OIDC JWT");
    });

    it("falls back to a transport ShardPlanRpcError when the error body isn't Connect-shaped", async () => {
      const fetcher = new FakeFetcher(() => ({
        status: 502,
        body: "<html>Bad Gateway</html>",
      }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("transport");
      expect(err.httpStatus).toBe(502);
    });

    it("maps unparsable JSON on a non-2xx to a transport error carrying the HTTP status", async () => {
      const planner = new RpcShardPlanner({
        fetcher: new JsonRejectingFetcher(500),
        repoId: 1,
        token: "tok",
      });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("transport");
      expect(err.httpStatus).toBe(500);
    });

    it("throws a transport ShardPlanRpcError when the Fetcher itself rejects", async () => {
      const fetcher: ShardPlanFetcher = {
        fetch: async () => {
          throw new Error("service binding unavailable");
        },
      };
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("transport");
      expect(err.message).toContain("service binding unavailable");
    });
  });

  describe("bearer credential safety", () => {
    const secretToken = "super-secret-token-value";

    it("never echoes the bearer token in a Fetcher-thrown error message", async () => {
      const fetcher: ShardPlanFetcher = {
        fetch: async () => {
          throw new Error(`connect ECONNREFUSED while sending Bearer ${secretToken}`);
        },
      };
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: secretToken });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.message).not.toContain(secretToken);
      expect(err.message).toContain("[redacted]");
    });

    it("never echoes the bearer token in the JSON-parse-failure message", async () => {
      const planner = new RpcShardPlanner({
        fetcher: new JsonRejectingFetcher(200, `body contained raw ${secretToken} somehow`),
        repoId: 1,
        token: secretToken,
      });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.message).not.toContain(secretToken);
      expect(err.message).toContain("[redacted]");
    });

    it("never echoes the bearer token in decodeResponse's echoed malformed body", async () => {
      const fetcher = new FakeFetcher(() => ({
        status: 200,
        body: { marker: secretToken },
      }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: secretToken });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).toBe("malformed_response");
      expect(err.message).not.toContain(secretToken);
      expect(err.message).toContain("[redacted]");
    });

    it("never echoes the bearer token in a server Connect error message", async () => {
      const fetcher = new FakeFetcher(() => ({
        status: 400,
        body: {
          code: "invalid_argument",
          message: `rejected token ${secretToken}: malformed`,
        },
      }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: secretToken });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.message).not.toContain(secretToken);
    });

    it("never echoes the bearer token in a server Connect error code", async () => {
      const fetcher = new FakeFetcher(() => ({
        status: 400,
        body: { code: secretToken, message: "ok" },
      }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: secretToken });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.code).not.toContain(secretToken);
      expect(err.code).toBe("[redacted]");
    });

    it("never echoes the bearer token when falling back on a non-Connect body", async () => {
      const fetcher = new FakeFetcher(() => ({
        status: 502,
        body: `proxy error for Bearer ${secretToken}`,
      }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: secretToken });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.message).not.toContain(secretToken);
    });

    it("scrubs a function token the same way as a string token", async () => {
      const fetcher: ShardPlanFetcher = {
        fetch: async () => {
          throw new Error(`auth failed for ${secretToken}`);
        },
      };
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: () => secretToken });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.message).not.toContain(secretToken);
      expect(err.message).toContain("[redacted]");
    });

    it("leaves a token under 8 characters unredacted, so it can't mangle ordinary server text", async () => {
      const fetcher = new FakeFetcher(() => ({
        status: 502,
        body: "proxy rejected the token field",
      }));
      // "token" (5 chars) is deliberately both the credential AND a
      // substring of the server's own text below — this is what actually
      // exercises `scrub`'s length guard: a token that IS present in the
      // echoed text but is too short to redact. (An unrelated token that
      // never occurs in the body, like the one this test used to use,
      // would pass whether or not the length guard existed at all.)
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "token" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.message).toContain("the token field");
    });

    it("truncates an echoed non-Connect error body at exactly MAX_ECHO_CHARS (200) characters", async () => {
      const longBody = "x".repeat(5000);
      const fetcher = new FakeFetcher(() => ({ status: 502, body: longBody }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1 }));

      expect(err.message).toBe(
        `ResolveShardPlan failed (transport): non-Connect error response: ` +
          `${longBody.slice(0, 200)}…(truncated)`,
      );
    });
  });

  describe("client-side validation (throws before any fetch)", () => {
    it("rejects a non-integer fixed count", async () => {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

      const err = await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1.5 }));

      expect(err.code).toBe("invalid_argument");
      expect(fetcher.calls).toHaveLength(0);
    });

    it("rejects count.min > count.max", async () => {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

      const err = await rejects(
        planner.resolve({
          filePaths: [],
          strategy: "timing",
          count: { min: 10, max: 2, target: "5m" },
        }),
      );

      expect(err.code).toBe("invalid_argument");
      expect(fetcher.calls).toHaveLength(0);
    });

    it("rejects a count.target string with an unrecognized unit instead of silently defaulting", async () => {
      const fetcher = new FakeFetcher(() => ({
        status: 200,
        body: { shardCount: 1, shards: [{ files: [] }] },
      }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

      const err = await rejects(
        planner.resolve({
          filePaths: [],
          strategy: "timing",
          count: { min: 1, max: 4, target: "5x" },
        }),
      );

      expect(err.code).toBe("invalid_argument");
      expect(fetcher.calls).toHaveLength(0);
    });

    it.each(["30s", "1h", "315576000000s"])(
      "accepts the duration %s (within Duration's magnitude limit)",
      async (target) => {
        const fetcher = new FakeFetcher(() => ({
          status: 200,
          body: { shardCount: 1, shards: [{ files: [] }] },
        }));
        const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

        await planner.resolve({
          filePaths: [],
          strategy: "timing",
          count: { min: 1, max: 4, target },
        });

        expect(fetcher.calls).toHaveLength(1);
      },
    );

    it("rejects a count.target duration exactly one second past Duration's maximum magnitude", async () => {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok" });

      const err = await rejects(
        planner.resolve({
          filePaths: [],
          strategy: "timing",
          count: { min: 1, max: 4, target: "315576000001s" },
        }),
      );

      expect(err.code).toBe("invalid_argument");
      expect(fetcher.calls).toHaveLength(0);
    });

    it("never calls the token function when request validation fails", async () => {
      const fetcher = new FakeFetcher(() => ({ status: 200, body: {} }));
      let tokenCalls = 0;
      const planner = new RpcShardPlanner({
        fetcher,
        repoId: 1,
        token: () => {
          tokenCalls += 1;
          return "tok";
        },
      });

      await rejects(planner.resolve({ filePaths: [], strategy: "file", count: 1.5 }));

      expect(tokenCalls).toBe(0);
      expect(fetcher.calls).toHaveLength(0);
    });
  });

  describe("constructor validation", () => {
    const fetcher: ShardPlanFetcher = {
      fetch: async () => {
        throw new Error("must not be called");
      },
    };

    it("rejects a negative repoId", () => {
      expect(() => new RpcShardPlanner({ fetcher, repoId: -1, token: "tok" })).toThrowError(
        ShardPlanRpcError,
      );
    });

    it("accepts a bigint repoId at exactly the uint64 upper bound (2^64-1)", () => {
      expect(
        () => new RpcShardPlanner({ fetcher, repoId: 2n ** 64n - 1n, token: "tok" }),
      ).not.toThrow();
    });

    it("rejects a bigint repoId one past the uint64 upper bound (2^64)", () => {
      expect(() => new RpcShardPlanner({ fetcher, repoId: 2n ** 64n, token: "tok" })).toThrowError(
        ShardPlanRpcError,
      );
    });

    it("rejects a repoId bigint outside uint64 range (negative)", () => {
      expect(() => new RpcShardPlanner({ fetcher, repoId: -1n, token: "tok" })).toThrowError(
        ShardPlanRpcError,
      );
    });

    it("rejects a plain http:// baseUrl override (would send the bearer in cleartext)", () => {
      expect(
        () =>
          new RpcShardPlanner({
            fetcher,
            repoId: 1,
            token: "tok",
            baseUrl: "http://evil.example.com",
          }),
      ).toThrowError(ShardPlanRpcError);
    });

    it("rejects a baseUrl containing userinfo, without echoing the username or password", () => {
      let caught: unknown;
      try {
        new RpcShardPlanner({
          fetcher,
          repoId: 1,
          token: "tok",
          baseUrl: "https://alice:s3cr3t-password@cloud-ci-worker.example.workers.dev",
        });
      } catch (err) {
        caught = err;
      }

      expect(caught).toBeInstanceOf(ShardPlanRpcError);
      const message = caught instanceof ShardPlanRpcError ? caught.message : "";
      expect(message).not.toContain("alice");
      expect(message).not.toContain("s3cr3t-password");
      expect(message).toContain("cloud-ci-worker.example.workers.dev");
    });

    // Defense-in-depth: `new URL()` never even runs on a string with
    // embedded whitespace (`normalizeBaseUrl` rejects it first), so the
    // userinfo-stripping regex — not `url.origin` — is what has to keep
    // the password out of *this* branch's message.
    it("never echoes userinfo when a baseUrl has both userinfo and whitespace", () => {
      let caught: unknown;
      try {
        new RpcShardPlanner({
          fetcher,
          repoId: 1,
          token: "tok",
          baseUrl: "https://alice:s3cr3t-password@cloud-ci-worker.example.workers.dev/ evil",
        });
      } catch (err) {
        caught = err;
      }

      expect(caught).toBeInstanceOf(ShardPlanRpcError);
      const message = caught instanceof ShardPlanRpcError ? caught.message : "";
      expect(message).not.toContain("alice");
      expect(message).not.toContain("s3cr3t-password");
    });

    it("rejects a baseUrl containing a space", () => {
      expect(
        () =>
          new RpcShardPlanner({
            fetcher,
            repoId: 1,
            token: "tok",
            baseUrl: "https://cloud-ci-worker.example.workers.dev/ evil",
          }),
      ).toThrowError(ShardPlanRpcError);
    });

    // `new URL()` silently strips embedded tab/newline characters rather
    // than rejecting them (the exact gap `normalizeBaseUrl`'s own doc
    // comment names) — this proves the explicit pre-parse whitespace check
    // catches what the space-only test above can't.
    it.each([
      "https://cloud-ci-worker\t.example.workers.dev",
      "https://cloud-ci-worker\n.example.workers.dev",
    ])("rejects a baseUrl containing a tab or newline (%j)", (baseUrl) => {
      expect(() => new RpcShardPlanner({ fetcher, repoId: 1, token: "tok", baseUrl })).toThrowError(
        ShardPlanRpcError,
      );
    });

    it("rejects a baseUrl with a non-root path (would silently route a path-prefixed gateway to the wrong endpoint)", () => {
      expect(
        () =>
          new RpcShardPlanner({
            fetcher,
            repoId: 1,
            token: "tok",
            baseUrl: "https://cloud-ci-worker.example.workers.dev/prefix",
          }),
      ).toThrowError(ShardPlanRpcError);
    });

    it("rejects a baseUrl with a query string", () => {
      expect(
        () =>
          new RpcShardPlanner({
            fetcher,
            repoId: 1,
            token: "tok",
            baseUrl: "https://cloud-ci-worker.example.workers.dev?x=1",
          }),
      ).toThrowError(ShardPlanRpcError);
    });

    it("rejects a baseUrl with a bare trailing '?' (an empty query string)", () => {
      // `url.search` is `""` for a bare trailing `?` (the WHATWG URL spec
      // drops an empty query), so this is checked separately from the
      // query-string case above — it must not slip through as "no query".
      expect(
        () =>
          new RpcShardPlanner({
            fetcher,
            repoId: 1,
            token: "tok",
            baseUrl: "https://cloud-ci-worker.example.workers.dev?",
          }),
      ).toThrowError(ShardPlanRpcError);
    });

    it.each(["http://cloud-ci.internal#frag", "https://cloud-ci-worker.example.workers.dev#frag"])(
      "rejects a baseUrl with a fragment (%j)",
      (baseUrl) => {
        expect(
          () => new RpcShardPlanner({ fetcher, repoId: 1, token: "tok", baseUrl }),
        ).toThrowError(ShardPlanRpcError);
      },
    );

    it('names the scheme, not the literal string "null", when an opaque-origin scheme is rejected', () => {
      let caught: unknown;
      try {
        new RpcShardPlanner({ fetcher, repoId: 1, token: "tok", baseUrl: "javascript:alert(1)" });
      } catch (err) {
        caught = err;
      }

      expect(caught).toBeInstanceOf(ShardPlanRpcError);
      const message = caught instanceof ShardPlanRpcError ? caught.message : "";
      expect(message).toContain("javascript:");
      expect(message).not.toContain('"null"');
    });
  });

  describe("baseUrl normalization (asserts the actual resolved request URL)", () => {
    /** Resolves through `planner` and returns the exact URL
     * `ShardPlanFetcher.fetch` was called with — used by every case below
     * so "accepts"/"strips"/"lowercases" claims are proven against the
     * real request, not just a non-throwing constructor call. */
    async function requestedUrl(baseUrl?: string): Promise<string> {
      const fetcher = new FakeFetcher(() => ({
        status: 200,
        body: { shardCount: 1, shards: [{ files: [] }] },
      }));
      const planner = new RpcShardPlanner({ fetcher, repoId: 1, token: "tok", baseUrl });
      await planner.resolve({ filePaths: [], strategy: "file", count: 1 });
      return fetcher.calls[0]?.url ?? "";
    }

    const rpcPath = "/cloud_ci.ingest.v1.IngestService/ResolveShardPlan";

    it("uses the default internal origin when no baseUrl is given", async () => {
      expect(await requestedUrl(undefined)).toBe(`http://cloud-ci.internal${rpcPath}`);
    });

    // U1 regression coverage: the default-origin shortcut used to be an
    // exact string compare against the raw input, so a trailing slash (or
    // two) on the otherwise-identical default was wrongly rejected as "not
    // https:". It must compare the *parsed* origin instead.
    it("accepts the documented default origin with one trailing slash", async () => {
      expect(await requestedUrl("http://cloud-ci.internal/")).toBe(
        `http://cloud-ci.internal${rpcPath}`,
      );
    });

    it("accepts the documented default origin with two trailing slashes", async () => {
      expect(await requestedUrl("http://cloud-ci.internal//")).toBe(
        `http://cloud-ci.internal${rpcPath}`,
      );
    });

    it("strips a trailing slash on an https:// override", async () => {
      expect(await requestedUrl("https://cloud-ci-worker.example.workers.dev/")).toBe(
        `https://cloud-ci-worker.example.workers.dev${rpcPath}`,
      );
    });

    it("lowercases an uppercase HTTPS:// scheme", async () => {
      expect(await requestedUrl("HTTPS://cloud-ci-worker.example.workers.dev")).toBe(
        `https://cloud-ci-worker.example.workers.dev${rpcPath}`,
      );
    });
  });
});
