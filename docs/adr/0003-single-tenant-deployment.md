# 0003: Single-tenant, deploy-to-your-own-account

- Status: Proposed
- Date: 2026-09-30

## Context

Users are "people and companies" who want CI on Cloudflare. A hosted multi-tenant SaaS would
need billing, tenant isolation for arbitrary code execution, and abuse handling — and would
put customer source code and secrets in our account.

## Decision

Every deployment is one Worker plus its bindings in the deployer's own Cloudflare account,
serving one GitHub App installation target (an org or user). Setup is `wrangler deploy` (or a
Deploy-to-Cloudflare button) followed by a GitHub App manifest flow that creates the App
owned by the deployer.

## Consequences

- No tenant id in the data model; isolation is the Cloudflare account boundary.
- Source code, logs, secrets, and AI prompts never leave the deployer's account.
- Upgrades are the deployer's responsibility: D1 migrations must be forward-only and safe to
  run against a live deployment, and the proto contract must stay backward compatible with
  older CLIs.
- Container and Workers AI usage bill to the deployer.

## What would reverse this

Demand for a hosted offering. Adding tenancy later means a tenant key on every D1 table and
R2 prefix — painful but mechanical, which is why identifiers are already namespaced by repo.
