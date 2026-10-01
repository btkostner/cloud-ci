# 0008: Access or GitHub OAuth for humans; OIDC for machines

- Status: Proposed
- Date: 2026-09-30

## Context

Companies on Cloudflare Zero Trust want their existing IdP and policies; individuals want
"log in with GitHub". Machines uploading from GitHub Actions should need no stored secret.

## Decision

- Human auth mode is chosen at deploy time: Cloudflare Access (verify the Access JWT, map
  groups to roles), GitHub OAuth (roles derived from the user's GitHub permission on each
  repo), or both.
- Roles: viewer, operator, admin. Permission is evaluated per repo.
- Machines: GitHub Actions OIDC tokens (audience = deployment URL), hashed scoped API tokens,
  and coordinator-minted per-job tokens.

Details in [auth](../design/auth.md).

## Consequences

- Two human auth paths to test and maintain.
- GitHub-derived roles require GitHub API calls on the hot path; they are cached with a short TTL.

## What would reverse this

Access becoming available without a Zero Trust setup burden for individuals, at which point
GitHub OAuth could be dropped.
