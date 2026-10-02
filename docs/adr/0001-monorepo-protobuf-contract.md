# 0001: Monorepo with a protobuf contract

- Status: Accepted
- Date: 2026-09-30

## Context

cloud-ci has at least four programs that must agree on a wire format: the Worker (Rust/wasm),
the `cloud-ci` CLI and in-container agent (native Rust), the dashboard (TypeScript), and every
third-party CI that uploads with an older CLI version. Old CLIs in the field will keep talking
to new deployments, so the contract must evolve compatibly and be checkable.

## Decision

- One repository; one directory per package under `packages/`; no root Cargo or pnpm
  workspace. Packages depend on each other by relative path.
- `cloud-ci-proto` holds `.proto` files and `buf` config. One `buf generate` writes bindings into
  `generated/` (gitignored, wiped every run) inside `cloud-ci-proto-rust` and
  `cloud-ci-proto-typescript`, each with exactly one hand-written entrypoint.
- `mise` orchestrates tasks (`build`, `check`, `fix`, `test`, `dev`) per package with a root
  that only composes; `hk` composes lint/format hooks with `subprojects`.
- CI runs `buf breaking` against `main`.
- RPC shapes: unary for commands; server-streaming only for live log/run watching. No client or
  bidirectional streaming.

## Consequences

- Each language keeps its native toolchain; no Bazel-style second dependency graph.
- Adding a proto package means touching each binding's entrypoint — a few lines, once.
- Workers lack a long-lived HTTP/2 server, so Connect is implemented over the Worker's `fetch`
  handler rather than a Tower server stack ([ADR 0002](./0002-rust-cloudflare-worker.md)).

## What would reverse this

A second consumer that cannot use protobuf at all (e.g. a public REST API demanded by users),
or codegen for wasm32 proving unworkable — at which point the contract would move to
hand-written serde types plus JSON Schema.
