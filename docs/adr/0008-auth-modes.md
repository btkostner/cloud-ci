# 0008: GitHub OAuth for humans; OIDC and tokens for machines

- Status: Accepted
- Date: 2026-10-01

## Context

Humans need a dashboard login; machines uploading from GitHub Actions or other CI should need
no stored secret. An earlier draft of this ADR also offered Cloudflare Access/Zero Trust as a
human auth mode for companies with an existing IdP; see Alternatives considered.

## Decision

- Humans authenticate with GitHub OAuth (the GitHub App user-to-server flow). Roles are derived
  from the user's GitHub permission on each repo — viewer, operator, admin — evaluated per repo
  and cached with a short TTL since it requires a GitHub API call.
- Machines: GitHub Actions OIDC tokens (audience = deployment URL), hashed scoped API tokens,
  and coordinator-minted per-job tokens for the agent.
- Deployers who want the asset hostname (artifacts/reports) behind Cloudflare Zero Trust can put
  it there themselves; that is a deployment choice out of band of cloud-ci auth, not a cloud-ci
  auth mode.

Details in [auth](../design/auth.md).

## Consequences

- One human auth path to test and maintain.
- GitHub-derived roles require a GitHub API call on the hot path; mitigated by a short-TTL cache.
- Deployers without a GitHub org for their reviewers (e.g. a Zero-Trust-only company) must add
  those people as GitHub collaborators to grant dashboard access.

## Alternatives considered

- **Cloudflare Access as a cloud-ci auth mode.** Verifying the Access JWT and mapping groups to
  roles would let companies reuse their existing IdP without adding GitHub collaborators.
  Rejected: it adds a second human auth path to test and maintain, duplicates what GitHub
  permissions already express, and is off-brand for a tool whose roles are already "who can
  touch this GitHub repo". Deployers who still want IdP-gated access can front the dashboard
  with Access themselves at the Cloudflare account level, same as the asset hostname note above.

## What would reverse this

Demand from deployers whose reviewers are not GitHub users at all (no seats, no collaborator
access), making GitHub OAuth a hard requirement rather than a convenience.
