# 0003: Single-tenant, deploy-to-your-own-account

- Status: Proposed
- Date: 2026-09-30

## Context

Users are "people and companies" who want CI on Cloudflare. A hosted multi-tenant SaaS would
need billing, tenant isolation for arbitrary code execution, and abuse handling — and would
put customer source code and secrets in our account.

## Decision

Every deployment is one Worker plus its bindings in the deployer's own Cloudflare account. One
deployment can serve **multiple GitHub orgs** (multiple GitHub App installations) belonging to
one company; data is keyed by installation/org id + repo id, and the dashboard scopes access by
the signed-in user's permission per repo. Setup is `wrangler deploy` (or a Deploy-to-Cloudflare
button) followed by a GitHub App manifest flow that creates the App owned by the deployer, then
installing that App on each org.

## Consequences

- No tenant id in the data model; isolation is the Cloudflare account boundary. Multiple orgs
  under one deployment share that boundary — they are not isolated from each other the way
  separate deployments are, only scoped by installation/org id + repo id in queries and by
  per-repo permission in the dashboard.
- Source code, logs, secrets, and AI prompts never leave the deployer's account.
- Upgrades are the deployer's responsibility: D1 migrations must be forward-only and safe to
  run against a live deployment, and the proto contract must stay backward compatible with
  older CLIs. A version that changes how `NodeContainer` addresses a node's real container
  (`coordinator::node_physical_address`, introduced 2026-10-03) requires draining every
  `NodeContainer` actor from the previous deployment first — not just non-terminal nodes: a
  node already marked terminal can still have a running physical container (`run_and_report`
  reports an exit code, it never calls `destroy()` itself), so the old actor can be left
  running and unreachable by the new addressing scheme either way.
- Container and Workers AI usage bill to the deployer.

## What would reverse this

Demand for a hosted offering. Adding tenancy later means a tenant key on every D1 table and
R2 prefix — painful but mechanical, which is why identifiers are already namespaced by repo.
