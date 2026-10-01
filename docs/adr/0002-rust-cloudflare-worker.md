# 0002: Rust on Cloudflare Workers

- Status: Proposed
- Date: 2026-09-30

## Context

The product is defined as a Rust Cloudflare Worker. The Worker parses large untrusted inputs
(JUnit XML, coverage files, Playwright JSON), merges them, and computes analytics — work where
Rust's performance and type safety pay off, and where the same parsing code must also run in
the native CLI.

## Decision

- `cloud-ci-worker` uses `workers-rs` (`worker` crate 0.8.x, features `d1`, `queue`, `http`)
  compiled to `wasm32-unknown-unknown` via `worker-build`.
- Report parsers, merge logic, the splitter, and the rightsizer live in a plain Rust library
  (planned `cloud-ci-core`, no `worker` dependency) shared by the Worker and the CLI, so they are
  unit-testable natively.
- Connect-protocol unary RPCs are routed by hand in the Worker's fetch handler (`POST
  /<package>.<Service>/<Method>`, `application/proto` or `application/json`), using
  buffa-generated message types. The `connectrpc` crate's server feature is hyper/tokio-based
  and is not expected to run on Workers [unverified]; its message and client types are usable
  from the native CLI.
- Lints: `unsafe_code = "forbid"`; clippy `unwrap_used`, `expect_used`, `panic`, `todo` warned.

## Consequences

- Any capability workers-rs lacks requires JavaScript glue. The known candidate is container
  lifecycle ([ADR 0005](./0005-containers-for-execution.md)).
- wasm bundle size counts against Worker script limits; heavy dependencies (e.g. a full XML
  DOM) must be avoided in favor of streaming parsers.

## What would reverse this

workers-rs falling far enough behind the platform that more than a thin shim is JavaScript, or
wasm cold-start/size limits making the Worker unshippable.
