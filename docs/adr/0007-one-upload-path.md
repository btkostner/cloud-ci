# 0007: One upload path for managed and external runs

- Status: Proposed
- Date: 2026-09-30

## Context

BYO CI is a headline feature, and the easiest way for it to rot is for our own runners to use
a private, richer path.

## Decision

`cloud-ci agent` (inside our containers) and `cloud-ci upload` (in anyone's CI) use the same
client code and the same ingest RPCs. The only difference is the credential: a job token
minted by the coordinator versus GitHub OIDC or an API token.

## Consequences

- Every managed run exercises BYO CI end to end.
- Ingest must be efficient enough for the hot path (log streaming), not just end-of-job uploads.

## What would reverse this

A managed-only capability that is fundamentally impossible to offer externally (none known).
