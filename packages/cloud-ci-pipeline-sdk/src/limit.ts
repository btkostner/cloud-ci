import { InvalidConcurrencyError } from "./errors.js";

/**
 * Backs `ci.limit(n, thunks)`. `docs/design/dynamic-pipelines.md`'s
 * "Graph helpers" table gives this its entire specification in one row:
 * "Concurrency limiter that is replay-safe (ordering is by call, not
 * completion); the per-call half of concurrency control — repo-wide caps
 * come from settings.yml" (`:380`). No worked example calls `ci.limit`
 * directly — `turbo.execute`'s own sketch (the design doc's "Discovery and
 * triggers" section, `:157`) is the only call site shown:
 * `await ci.limit(opts.concurrency, graph.ids().map((id) => () => runNode(id)))`.
 * That one call site fixes the signature this implements: `n` concurrent
 * slots, `thunks` an array of zero-arg functions each returning a promise,
 * return value an array of their results in `thunks`' own order (mirroring
 * `Promise.all`'s result-ordering contract, since `ci.limit` is a bounded
 * drop-in for `Promise.all(thunks.map((t) => t()))`).
 *
 * **"Ordering is by call, not completion" — what that rules out, and the
 * reading this package takes:** a concurrency limiter has two places
 * ordering could leak in: (1) which thunk starts next when a slot frees up,
 * and (2) the array position of the return value. Both are pinned to
 * `thunks`' own array order here, never to which in-flight thunk happens to
 * settle first in real time:
 *
 * - A free slot always claims the lowest-index thunk that has not yet
 *   started — `next` below is a single shared counter incremented
 *   synchronously (no `Promise.race` over in-flight promises to decide
 *   "what's ready"), so which thunk starts next is a pure function of
 *   `thunks`' array order, not of which worker's prior thunk happened to
 *   resolve first or how long each thunk actually took.
 * - Results are written into `results[i]` by each thunk's own fixed index,
 *   so the returned array is always in call order regardless of
 *   completion order.
 *
 * This matters for the same reason "Determinism" (`docs/design/
 * dynamic-pipelines.md`) requires every `step.do`/`step.waitForEvent` call
 * to happen in the same order on replay: if a limiter picked its next
 * thunk by racing in-flight promises, a replay whose containers happen to
 * finish in a different wall-clock order than the original run could start
 * a different *set* of thunks before hitting its concurrency cap than the
 * original run did — a real, if narrow, path to the "replayed start for an
 * id it has never seen" divergence the coordinator rejects. Index-order
 * claiming closes that off: slot N always goes to thunk N once thunks
 * 0..N-1 have *started* (not finished), independent of completion timing.
 *
 * This is in-process scheduling only — it does not itself wrap anything in
 * `step.do`. Each thunk is expected to make its own durable calls (e.g. a
 * `ci.container` call), the same way `Promise.all`-based fan-out already
 * would; `ci.limit` only bounds how many of those run concurrently.
 */
export async function limit<T>(n: number, thunks: readonly (() => Promise<T>)[]): Promise<T[]> {
  if (!Number.isInteger(n) || n < 1) {
    throw new InvalidConcurrencyError(n);
  }

  const results = new Array<T>(thunks.length);
  let next = 0;

  async function worker(): Promise<void> {
    for (;;) {
      const i = next;
      const thunk = thunks[i];
      if (!thunk) {
        return;
      }
      next += 1;
      results[i] = await thunk();
    }
  }

  const workerCount = Math.min(n, thunks.length);
  await Promise.all(Array.from({ length: workerCount }, () => worker()));

  return results;
}
