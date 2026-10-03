import { describe, expect, it } from "vitest";
import { InvalidConcurrencyError } from "../src/errors.js";
import { limit } from "../src/limit.js";

/** Flushes pending microtasks so an already-settled promise's `.then`
 * chain (inside `limit`'s worker loop) has had a chance to run, without
 * binding the test to any real wall-clock duration. */
async function flushMicrotasks(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

describe("ci.limit", () => {
  it("claims the next pending thunk by array index when a slot frees, regardless of which earlier thunk settles first", async () => {
    const started: number[] = [];
    const gate0 = Promise.withResolvers<string>();
    const gate1 = Promise.withResolvers<string>();
    const gate2 = Promise.withResolvers<string>();

    const thunks = [
      async () => {
        started.push(0);
        return gate0.promise;
      },
      async () => {
        started.push(1);
        return gate1.promise;
      },
      async () => {
        started.push(2);
        return gate2.promise;
      },
    ];

    const resultPromise = limit(2, thunks);

    // With concurrency 2, thunks 0 and 1 claim the two slots synchronously;
    // thunk 2 stays pending.
    expect(started).toEqual([0, 1]);

    // Resolve thunk 1 before thunk 0 — a completion-ordered limiter could
    // let whichever slot freed first pick an out-of-order pending item, but
    // there is only one pending item (2), so this specifically proves the
    // freed slot claims it immediately, in array order, without waiting for
    // thunk 0 (still unresolved) to finish.
    gate1.resolve("one");
    await flushMicrotasks();
    expect(started).toEqual([0, 1, 2]);

    gate2.resolve("two");
    gate0.resolve("zero");
    const results = await resultPromise;

    // Results land at their own call-order index, not completion order
    // (thunk 1 settled first, thunk 0 settled last).
    expect(results).toEqual(["zero", "one", "two"]);
  });

  it("never starts more than n thunks concurrently", async () => {
    const gate0 = Promise.withResolvers<number>();
    const gate1 = Promise.withResolvers<number>();
    const gate2 = Promise.withResolvers<number>();
    const gate3 = Promise.withResolvers<number>();
    const gate4 = Promise.withResolvers<number>();
    const gate5 = Promise.withResolvers<number>();
    const gates = [gate0, gate1, gate2, gate3, gate4, gate5];

    const started: number[] = [];
    const thunks = gates.map((gate, i) => async () => {
      started.push(i);
      return gate.promise;
    });

    const resultPromise = limit(3, thunks);

    expect(started).toEqual([0, 1, 2]);

    gate0.resolve(0);
    await flushMicrotasks();
    expect(started).toEqual([0, 1, 2, 3]);

    gate1.resolve(1);
    gate2.resolve(2);
    await flushMicrotasks();
    expect([...started].sort((a, b) => a - b)).toEqual([0, 1, 2, 3, 4, 5]);

    gate3.resolve(3);
    gate4.resolve(4);
    gate5.resolve(5);
    await resultPromise;
  });

  it("runs all thunks sequentially when n is 1", async () => {
    const order: number[] = [];
    const gate0 = Promise.withResolvers<void>();
    const gate1 = Promise.withResolvers<void>();
    const gate2 = Promise.withResolvers<void>();
    const thunks = [gate0, gate1, gate2].map((gate, i) => async () => {
      order.push(i);
      return gate.promise;
    });

    const resultPromise = limit(1, thunks);
    expect(order).toEqual([0]);

    gate0.resolve();
    await flushMicrotasks();
    expect(order).toEqual([0, 1]);

    gate1.resolve();
    await flushMicrotasks();
    expect(order).toEqual([0, 1, 2]);

    gate2.resolve();
    await resultPromise;
  });

  it("returns an empty array for an empty thunks array", async () => {
    await expect(limit(4, [])).resolves.toEqual([]);
  });

  it("throws InvalidConcurrencyError for n < 1", async () => {
    await expect(limit(0, [async () => 1])).rejects.toThrow(InvalidConcurrencyError);
  });

  it("throws InvalidConcurrencyError for a non-integer n", async () => {
    await expect(limit(1.5, [async () => 1])).rejects.toThrow(InvalidConcurrencyError);
  });
});
