// Real `.cloud-ci/pipelines/*.ts`-shaped script written through
// `@cloud-ci/pipeline-sdk`'s public `workflow()`/`ci.check`/`ci.container`
// API instead of hand-rolled `WorkflowEntrypoint` subclass JS — proves the
// README's "workflow() adapter shape" glue (`getEntrypoint()` with no
// argument, ../../src/index.ts) actually loads and runs an SDK-built
// script end to end. The two existing fixtures (`pipeline-script.js`,
// `pipeline-script-dag.js`) are untouched and keep proving the host
// supports plain, non-SDK scripts too (the loader only duck-types
// `WorkflowRunner`, it has no SDK-only contract).
//
// `POST /scripts` loads its `script` field verbatim as a single ES module
// with no bundler/transpiler (see ../../README.md's "Explicit non-goals":
// "TypeScript transpilation of the loaded script" is out of scope) — it
// cannot resolve a bare `import "@cloud-ci/pipeline-sdk"` specifier at
// runtime, since the Worker Loader only knows about the one module string
// handed to it. So this file is the source of truth, authored exactly like
// a real pipeline script, and `build-sdk-fixture.mjs` bundles it with
// esbuild into the committed, self-contained `pipeline-script-sdk.js` that
// `POST /scripts` actually loads — bundling happens at authoring/build
// time, outside the host, which still never transpiles or resolves
// imports for a loaded script itself.
//
// `workflow()`'s `run(event, step)` is a plain object method called over
// `getEntrypoint()` RPC (see ../../src/index.ts), not a `WorkerEntrypoint`
// subclass constructor — there is no `this.env`. The real binding-access
// mechanism for a plain-object entrypoint is `cloudflare:workers`'s
// exported live `env` accessor
// (developers.cloudflare.com/workers/runtime-apis/bindings/#cloudflareworkers-env,
// checked 2026-10-02): a module-scope import, resolved per-call by the
// runtime rather than snapshotted once, so it stays correct across an
// isolate recycle the same way `this.env` would inside a class — this is
// what lets `workflow(opts, { executor })`'s real `ContainerExecutor` be
// built once at module scope below instead of needing special per-call env
// plumbing the SDK's `WorkflowRunner`/`ContainerExecutor` types don't
// carry.
//
// Shape: two sequential `ci.container` steps with a real dependency —
// `step-b`'s command embeds `step-a`'s own recorded result
// (`stepA.startedAt`), so the dependency is structural, not just
// source-order adjacency. Used for the real end-to-end step-durability
// proof in ../../README.md's "SDK fixture step-durability verification"
// section: `step.do("container:step-a", ...)` here is this package's
// real, non-fake `ContainerExecutor` calling back into the Rust
// `cloud-ci-worker`'s `ContainerProbe` over `CONTAINER_WORKER` (the same
// proven chain `pipeline-script.js` uses) — not the SDK's own unit-test
// `FakeContainerExecutor`. The bare `await new Promise(setTimeout, ...)`
// between the two `ci.container` calls happens in the script's plain `run`
// function body, *outside* any `step.do`, deliberately: it gives a real,
// easily-hittable window to force a `wrangler dev` reload between the two
// steps, and — because nothing durable protects that bare wait — a reload
// during it genuinely restarts `run(ci)` from the top. Confirming
// `ci.container("step-a", ...)`'s second, post-restart call returns the
// *exact same* already-recorded `startedAt` without re-invoking the
// injected `ContainerExecutor` (i.e. without starting a second real
// container, which would record a new, later `startedAt`) is exactly
// `step.do`'s real, documented persistence contract — the same claim this
// package's "Isolate-recycle verification" section already proved for
// `pipeline-script.js`'s `step1-plan`, now proved again through the SDK's
// own `ci.container` call instead of a raw `step.do`.
import { env } from "cloudflare:workers";
import { workflow } from "@cloud-ci/pipeline-sdk";

/** Real (not fake) `ContainerExecutor`: calls back into the Rust
 * `cloud-ci-worker`'s `ContainerProbe` Durable Object over the
 * `CONTAINER_WORKER` service binding — the same proven chain
 * `pipeline-script.js`'s `step2-container-exec` uses, just wrapped behind
 * the SDK's `ContainerExecutor` interface instead of called inline. */
const executor = {
  async start({ id, run }) {
    const startedAt = Date.now();
    const res = await env.CONTAINER_WORKER.fetch(
      `http://cloud-ci.internal/internal/container-probe/exec?probe_id=${id}`,
      { method: "POST", body: JSON.stringify({ cmd: ["sh", "-c", run] }) },
    );
    if (!res.ok) {
      throw new Error(`${id} container exec failed: ${res.status} ${await res.text()}`);
    }
    const result = await res.json();
    return {
      ok: result.exit_code === 0,
      exitCode: result.exit_code,
      startedAt,
      finishedAt: Date.now(),
    };
  },
};

export default workflow(
  {
    on: { push: true },
    async run(ci) {
      const check = ci.check("sdk-fixture", { required: true });

      // `startedAt` is this run's fingerprint: if `step.do` ever failed to
      // cache `step-a`'s result and re-invoked `executor.start()` after the
      // bare wait below restarts `run(ci)`, the second call would record a
      // *new*, later `startedAt` instead of the exact value from before the
      // restart — the real, directly-observable (via `GET /instances/:id`)
      // signal the "SDK fixture step-durability verification" proof reads.
      const stepA = await ci.container("step-a", {
        runner: "node",
        run: "echo node-a",
        check,
      });

      // Bare, non-durable wait between the two steps — see module doc
      // comment above for exactly why this is deliberate: it gives a real
      // window to force a `wrangler dev` reload between the two
      // `ci.container` calls.
      await new Promise((resolve) => setTimeout(resolve, 20000));

      await ci.container("step-b", {
        runner: "node",
        run: `echo depends-on-${stepA.startedAt}`,
        check,
      });

      check.seal({ conclusion: "success" });
    },
  },
  { executor },
);
