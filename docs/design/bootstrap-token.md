Status: **DRAFT** - design only, nothing here is built. Owner questions are listed, not decided.

# Bootstrap-token issuance and exchange

Closes the roadmap gap: "Bootstrap exists as a data shape in `executor.rs` but nothing mints a
real one-time token or wires it to the `cloud-ci agent` token-exchange flow." Builds on
[ADR 0010](../adr/0010-pluggable-executors.md), [auth](./auth.md) and
[ADR 0007](../adr/0007-one-upload-path.md). `lib.rs` (under `packages/cloud-ci-worker/src/`) is
cited by function name, which does not drift as the file grows. Every other citation is
`file:line` at commit `f600e4d` (paths under `packages/`); those files were unchanged between
`f600e4d` and `7b9864f`, but they will drift, so re-check before implementing.

## 1. Summary

An executor boots a machine; that machine must obtain a run-bound credential without the Worker
ever sending a long-lived secret into user-data, env or logs. This doc designs:

1. a **bootstrap token**: single-use, short-lived, bound to one `(repo, run, node)`, minted only
   by the run's `RunCoordinator`;
2. a new **exchange** call the agent makes on the public ingest path, trading it for the
   ordinary run-bound ingest token that `authenticate_ingest_bearer` already accepts;
3. how it is delivered to the container, and how it is retried, healed and revoked.

Non-goals: the agent pull-loop and job-spec serving, non-Containers executors, any code.

## 2. What exists today

| Fact | Where |
| --- | --- |
| `Bootstrap { token: String, deployment_url: String }` is plain data; "Nothing in this crate mints a real one yet" | `cloud-ci-worker/src/executor.rs:128-132`, scope note `:42-48` |
| `Executor::start(&self, job: &JobSpec, bootstrap: &Bootstrap)` is the only consumer of the type; `ContainersExecutor::start` ignores it | `executor.rs:183-208`, `:235+` |
| `RunCoordinator` bypasses the trait on purpose: no real `Bootstrap` exists to pass | `executor.rs:57-71`; `coordinator/mod.rs:2961-2980` (`start_node_container` calls `node_container::start_container`) |
| `start_container` posts `{run_do_name,node_id,image,command}` to the `NodeContainer` DO; `handle_start` sets image only, calls `container.start(Some(options))`, then `exec()`s the command in the background | `node_container.rs:105-112`, `:140-170`, `:214-250` |
| `exec_in_container` calls `container.exec(&cmd, None)`: no env option is ever passed | `node_container.rs:356-362` |
| Duplicate `startNode` is absorbed by `resolve_start_node`; `AlreadyStarted` never calls `start_node_container` again. The node row is inserted **before** the container call | `coordinator/mod.rs:2716-2790`, `:2739`, `insert_node` `:5263` |
| `node` rows live in the `RunCoordinator`'s own SQLite and are projected to D1 `nodes` | `coordinator/mod.rs:4350` (`ensure_schema`), `:4467`; `migrations/0010_nodes.sql` |
| The only mint of a run-bound credential is `BeginRun`: authenticate (OIDC or scoped API token) then `ingest_token::mint` | `lib.rs` `handle_begin_run` (its `ingest_token::mint` call) |
| Ingest token = `<b64url(claims)>.<b64url(HMAC-SHA256)>`, claims `{typ:"ingest", scope:["ingest:write"], repo_id, run_id, exp}`, TTL 3600 s, key = `INGEST_TOKEN_SECRET` used directly | `ingest_token.rs:28-38`, `:57-80`, `:100-135`; key `lib.rs` `ingest_token_secret` |
| `verify` rejects a bad MAC, `typ != "ingest"`, missing `ingest:write`, expired | `ingest_token.rs:100-135` (`:122`, `:127`) |
| Every run-scoped RPC: `authenticate_ingest_bearer` then `resolve_run_identity` (D1 `runs` row) then `require_matching_identity` (both `repo_id` and `run_id` must match, else `PermissionDenied`) | `lib.rs` `authenticate_ingest_bearer`, `resolve_run_identity`, `require_matching_identity`; callers: `handle_start_job`, `handle_register_shard_group`, `handle_create_upload`, `handle_complete_upload`, `handle_submit_report`, `handle_submit_resource_samples`, `handle_complete_shard` |
| Raw part `PUT` re-verifies and compares claim `repo_id`/`run_id` to the upload's owner | `lib.rs` `handle_upload_part` |
| Scoped API tokens: random `cc_tok_` value, only `sha256` stored, admin-only issuance behind session + role check | `token_issuance.rs:97`, `:313`, `:333`; `lib.rs` `handle_issue_token`; `api_tokens.rs:49`; `migrations/0005_api_tokens.sql` |
| Proto: `BeginRunResponse.ingest_token = 3`; no exchange RPC exists | `cloud-ci-proto/proto/cloud_ci/ingest/v1/ingest.proto:10-20`, `:69-74` |
| Agent today: reads `CLOUD_CI_JOB_TOKEN` (fallback `CLOUD_CI_TOKEN`/OIDC) and uses it directly as the bearer for `SubmitResourceSamples`; server from `CLOUD_CI_SERVER_URL`; it does no exchange and pulls no job spec | `cloud-ci-cli/src/agent.rs:12-22`, `:98-110`, `:123-129`, `:151`; `identity.rs:145-148` |

## 3. Invariants this touches

- **Only a run's `RunCoordinator` writes run state** (`AGENTS.md`). Bootstrap state (minted,
  redeemed, revoked) is run state. It is written only inside the `RunCoordinator`; the stateless
  Worker handler for the exchange call never writes it. It forwards a redeem *request* to the
  coordinator DO (`RunCoordinatorStore`, the same route `BeginRun` and `StartJob` use, built in
  `lib.rs` `handle_begin_run` and `handle_start_job`) and the coordinator decides.
- **Inputs enqueue, coordinators decide.** The exchange handler does not trust the token's
  payload to mutate anything. It verifies the MAC (cheap, stateless, rejects garbage before any
  DO is woken) and then the coordinator re-reads its own row to decide redeem-or-reject. A
  duplicated or reordered exchange therefore resolves against state, not against the payload.
- **The agent uses the public ingest path** (ADR 0007). Exchange is a new RPC on the same
  `IngestService` and the same URL the agent already uses, callable by any machine with a
  bootstrap token. No private DO channel, no Containers-only shortcut.
- **The proto contract is backward compatible.** The change is purely additive: one new rpc and
  new messages in `ingest.proto`; no field renumbered or removed, `BeginRunResponse` untouched.
  `buf breaking` against `main` must pass. Old CLIs never call it.
- **D1 migrations are forward-only and live-safe.** The recommended option needs no D1
  migration (state lives in the coordinator's SQLite via `CREATE TABLE IF NOT EXISTS`, the
  existing pattern at `coordinator/mod.rs:4350`). See section 8 for the D1 variant.
- **No `unwrap`/`expect`/`panic` in Rust packages**, no secrets in logs, and Cloudflare facts
  dated and sourced (section 7).

## 4. Threat model

**Assets.** A run-bound ingest token authorizes `ingest:write` on every run-scoped RPC for one
run for up to one hour (`ingest_token.rs:28`). That is the thing an attacker wants.

**Who may mint.** Only the run's `RunCoordinator`, and only as part of dispatching a node it
already admitted (`handle_start_node`). No public endpoint mints a bootstrap token, and there is
no admin path to one. A mint function that any code in the crate may call is exactly the
standing-privilege risk `token_issuance.rs:6-17` calls out for `cc_tok_` tokens; the mint
function is therefore private to the coordinator module and takes the signing key as a
parameter (matching `ingest_token::mint`'s posture, `ingest_token.rs:51-62`).

**What a bootstrap token authorizes.** Exactly one thing: one successful exchange for the ingest
token of its own `(repo_id, run_id)`, for its own `node_id`, once, before `exp`. It authorizes
no other RPC. Presented as a bearer to any other RPC it must be rejected as it is today by
`verify` (`typ` mismatch, claim-shape mismatch). One run, one repo, one node or shard, one use.

**What the exchange yields, and the scope gap.** Today's only run-bound credential is the
run-wide ingest token. Exchange returns that token (so no existing check changes), which means a
redeemed bootstrap token confers run-wide `ingest:write`, not node-wide. The narrower
`typ=job` token auth.md describes (`auth.md:347`) is not implemented. Owner question OQ1.

| Threat | Window | Mitigation |
| --- | --- | --- |
| Token read from user-data/env/process list before use | Mint to first exchange (target: seconds) | Short `exp`; single use, so a stolen-after-use token is dead; never printed, never in argv |
| Replay of an already-redeemed token | Unbounded without state | Coordinator-held `redeemed_at`; replay returns `PermissionDenied`/`FailedPrecondition` and is logged by `jti` |
| Thief redeems first, real agent fails | Race at boot | Failure is loud (agent exits non-zero), coordinator's boot-deadline reaper notices no `StartJob` and the node is retried with a new token (section 9). The thief still only gets this run's ingest token |
| Token for run A used against run B or another repo | n/a | `run_id`/`repo_id`/`node_id` are inside the HMAC payload and re-checked against the coordinator row; the ingest token returned carries the *row's* ids, never the request's |
| Forged token | n/a | HMAC with a key distinct from the ingest key (section 5) |
| Bootstrap token replayed as an ingest token | n/a | `ingest_token::verify` rejects `typ != "ingest"` and a payload without `scope`; plus key domain separation |
| Job code reads the token | During job | Agent redeems on startup and must not export the bootstrap or ingest token to child processes; a redeemed bootstrap token is inert |
| Ingest token minted by exchange outlives cancel | Up to 1 h | Not revocable: `ingest_token` is stateless (`ingest_token.rs`). Cancel closes the run, and run-scoped writes to a closed run are already refused by the coordinator; bearer validity is not extended. OQ6 |
| Token leaks into logs | Forever | See "Logging" |

**Lifetime.** Proposed default 5 minutes from mint (OQ2). It must cover container pull, start
and agent boot, not job runtime. Expiry is checked against the coordinator clock at redeem time,
never trusted from the payload alone.

**Logging.** Log run id, node id, `jti`, attempt/generation, outcome and reason code. Never log
the token, any substring, its hash, the MAC, the signing key, or the returned ingest token. The
`exec()` env map and the `NodeContainer` `/start` body must not be logged or echoed in error
strings: `start_container` currently formats the response body into errors
(`node_container.rs:240-249`), which is safe only while the request body is not echoed. Error
paths must keep it that way once the body carries the token. Debug formatting of `Bootstrap`
must redact `token` (the derived `Debug` at `executor.rs:128` would print it; a manual impl is
required, see section 12).

## 5. Token format and storage

### Option A: HMAC-signed, plus a coordinator-held single-use row (recommended)

```
<b64url(payload)>.<b64url(HMAC-SHA256(key_boot, payload))>
payload = {v:1, typ:"bootstrap", repo_id, run_id, node_id, gen, jti, exp}
```

- Same stateless shape as `ingest_token.rs`, so the worker can reject malformed or forged tokens
  and route to the right DO (the `run_id` is in the payload) with no D1 read.
- `key_boot` is domain-separated from the ingest key. auth.md specifies
  `HKDF(CLOUD_CI_MASTER_KEY, info="cloud-ci/<typ>/v1")` (`auth.md:352-363`); the code uses
  `INGEST_TOKEN_SECRET` directly (`lib.rs` `ingest_token_secret`). Which root to use is OQ3 and a prerequisite.
- `jti = sha256(run_id || node_id || gen)` truncated and encoded, i.e. **deterministic**, and
  `exp` is fixed at first mint and stored. Minting the same `(run_id,node_id,gen)` again
  therefore reproduces the **identical token bytes** (the property
  `ingest_token.rs:162-167`'s `same_inputs_mint_identical_tokens` already tests). The plaintext
  never needs to be stored to redeliver it.
- Coordinator SQLite table (new, `CREATE TABLE IF NOT EXISTS`, forward-only like the rest of
  `ensure_schema`):

  ```
  bootstrap_token(jti PK, node_id, gen, expires_at, redeemed_at NULL, revoked_at NULL,
                  created_at)   UNIQUE (node_id, gen)
  ```

- Only `jti`, expiry and state are stored. No secret at rest.

### Option B: random opaque token, `sha256` stored (like `cc_tok_`)

`cc_boot_<32 random bytes>`; store the hash. Instant, simple revocation and a leaked-DB export is
inert. But the exchange handler cannot route to the run's DO from an opaque value; it needs a
global D1 table `bootstrap_tokens(token_hash PK, run_id, node_id, ...)` (migration
`0022_*.sql`) read by the Worker, which then must write redemption back to a table the
coordinator owns, pushing a second writer toward run state (violates the first invariant) or
requiring a prefix scheme (`cc_boot_<run_id>.<random>`), which is Option A with extra storage.
Random plaintext also cannot be re-derived, so a crash between mint and delivery loses it
(it must be stored in plaintext or re-minted with a new `gen`).

### Option C: stateless only (no single-use row)

Simplest, but "single-use" is then unenforceable: any replay inside `exp` works. Rejected: it
contradicts ADR 0010 ("Agent bootstrap tokens are single-use").

### Idempotency and one-use enforcement

The coordinator executes redeem as one SQLite statement:
`UPDATE bootstrap_token SET redeemed_at=? WHERE jti=? AND redeemed_at IS NULL AND revoked_at IS
NULL AND expires_at>?`, then reads whether a row changed. DO storage is single-writer and
serialized, so the first of two racing redeems wins and the other sees zero rows. No D1
transaction is needed; none is available across the DO boundary anyway.

## 6. The exchange flow

```mermaid
sequenceDiagram
    participant RC as RunCoordinator
    participant Ex as Executor / NodeContainer
    participant Ag as cloud-ci agent
    participant W as Worker (ingest RPC)
    RC->>RC: insert node row, mint/record bootstrap (gen)
    RC->>Ex: start(job, Bootstrap{token,url})
    Ex->>Ag: env CLOUD_CI_BOOTSTRAP_TOKEN, CLOUD_CI_SERVER_URL
    Ag->>W: ExchangeBootstrapToken(bearer = bootstrap token)
    W->>W: verify MAC, typ, exp (stateless)
    W->>RC: redeem(jti) via RunCoordinatorStore
    RC->>RC: atomic redeemed_at; run not closed; row matches
    RC-->>W: ok (repo_id, run_id from the row)
    W->>W: ingest_token::mint(secret, repo_id, run_id, now)
    W-->>Ag: ingest_token
    Ag->>W: StartJob / uploads (Bearer ingest token)
```

New RPC (proposed shape only, name is OQ4): `ExchangeBootstrapToken` takes no meaningful request
fields (the token is the bearer credential, like `BeginRun` it is the one call made before a
run-scoped token exists, see the doc comment on `lib.rs` `handle_begin_run`) and returns `{ingest_token}`; `run_id` is returned
for the agent's convenience. Authentication order mirrors `handle_begin_run`: the credential is
mandatory (no header is `Unauthenticated` and wakes no DO), resolve the signing key and fail
loudly if unset (`handle_begin_run` calls `ingest_token_secret` before touching the Durable Object), verify, then call the coordinator.

**Interaction with the existing bearer checks: none are weakened.** The returned ingest token is
produced by the same `ingest_token::mint` that `BeginRun` uses and is presented to the same
`authenticate_ingest_bearer`, `resolve_run_identity` and
`require_matching_identity` (all in `lib.rs`). Specifically:

- `ingest_token::verify` is not edited. Its `typ == "ingest"`, scope and `exp` checks stay.
- `require_matching_identity` is not edited; the exchange-minted token binds the **row's**
  `repo_id`/`run_id`, so it passes and fails exactly as a `BeginRun`-minted one.
- No code path accepts a bootstrap token where an ingest token is expected, and none accepts the
  reverse for exchange.
- The only new trust decision is "this bootstrap token is live", which is a state read in the
  one component allowed to decide it.

Closed runs: the coordinator refuses to redeem for a run that is terminal or cancelled (it
already owns that status). That keeps "revocation on close" a state check, not a token feature.

Agent side (described, not designed in detail here): when `CLOUD_CI_BOOTSTRAP_TOKEN` is set the
agent exchanges once at startup and keeps the ingest token in memory only; when it is not set,
today's `CLOUD_CI_JOB_TOKEN`/`CLOUD_CI_TOKEN`/OIDC chain (`agent.rs:98-110`) is unchanged, so
third-party-CI behavior does not move. Token expiry mid-run is a prerequisite (section 12).

## 7. Delivery into the container

Cloudflare Containers `exec()` runs a new process that "receives only the variables in `env`,
plus `PATH`. It does not inherit other variables from `envVars` or the image"
(developers.cloudflare.com/containers/guides/execute-commands/, page "Last updated Sep 30,
2026", read 2026-10-03). Therefore variables given to `start()` (or `envVars`) do **not**
reach an `exec()`'d process. The bootstrap token and URL must be passed in `exec()`'s own `env`
option, and `start()` must not be relied on to carry them.

The Rust binding used here is the patched `worker` fork pinned in `Cargo.toml:38-39`
(`rev df96700`, ADR 0011). Today `exec_in_container` passes `None` for options
(`node_container.rs:360-362`). Whether the fork's `exec` options expose `env` is
`[unverified]`: the fork source was not available in this session. This is a hard prerequisite
(section 12). If it does not, the fallback is a one-line `env VAR=value ...` argv wrapper,
which puts the token in argv and `/proc/<pid>/cmdline` and is therefore a worse exposure than
the env map; it should be chosen only deliberately (OQ7).

Flow change (description, no code): `start_container` (`node_container.rs:214`) and the
`NodeContainer` `StartRequest` (`:105`) gain the token and URL fields; `handle_start` forwards
them to `exec()`'s env; `RunCoordinator::start_node_container` (`mod.rs:2961`) becomes a call
through `Executor::start` once it has a real `Bootstrap` to pass (this is exactly what the
`executor.rs:57-71` note is waiting for). Executors other than Containers carry the same two
values by their own mechanism (user-data, Lambda payload), which ADR 0010 already says.

The request body that carries the token is DO-to-DO inside one Workers deployment and is never
logged. It must not be persisted by the `NodeContainer` DO.

## 8. Write ownership, migration, failure and healing

**Owner.** `RunCoordinator` mints, records, redeems and revokes. The exchange handler is a
verifier and a forwarder. `NodeContainer` and executors only carry the token; they hold no
state about it.

**Migration.** Recommended: none for D1. New coordinator-SQLite table via
`CREATE TABLE IF NOT EXISTS` in `ensure_schema` (`mod.rs:4350`), which is forward-only and safe
on a live deployment by construction (a new table cannot break an old DO instance). If the owner
wants dashboards of bootstrap events in D1, that is a separate projection and a forward-only
migration `0022_bootstrap_tokens.sql` written by the coordinator's projection path like
`project_node_to_d1`, not by the handler. The number is a placeholder: `main` now ends at
`0021_ai_insight_idempotency.sql` (merged), so `0022` is the next free number today, but another
branch may take it first, so the real number is the next free one at the time this migration is
written.

### Retry and idempotency semantics

| Situation | Behavior |
| --- | --- |
| **Duplicate dispatch** (`startNode` redelivered) | `resolve_start_node` returns `AlreadyStarted` (`mod.rs:2739`); no new mint, no second container start. State, not payload, decides |
| **Token minted twice** (same `(run_id,node_id,gen)`) | `INSERT OR IGNORE` on `UNIQUE(node_id,gen)`; `jti` and `exp` come from the stored row, so the re-derived token is byte-identical. One token exists per generation, never two |
| **Replayed exchange** | First redeem wins (atomic `UPDATE`); later ones get zero rows, rejected `FailedPrecondition`/`PermissionDenied` (never `OK`), logged by `jti` |
| **Lost exchange response** (agent never got the ingest token) | Strict single-use: token is burned; agent exits non-zero; boot-deadline reaper starts a new generation. Alternative (bounded same-caller replay) is OQ5. |
| **Crash between mint and delivery** | Row exists `redeemed_at NULL`. The node row is already `running` (inserted before the container call, `mod.rs:2761`), so a redelivered `startNode` is `AlreadyStarted` and would **not** redeliver: a pre-existing hazard that bootstrap makes visible (nothing now ever runs the agent). The heal is the reaper below, not redelivery |
| **Expiry** | Rejected at redeem by the coordinator clock; the reaper treats an expired, unredeemed token on a `running` node as a failed boot |
| **Revocation on run close or cancel** | `handle_cancel_run` (`mod.rs:2897`) and terminal transitions set `revoked_at` on every unredeemed bootstrap row (same transaction as the status change) and the container is stopped as today (`stop_node_container`). Redeem also checks run state, so a race is closed from both sides |
| **Crash after redeem, before agent used the ingest token** | Ingest token is simply lost with the agent; same as an agent crash: reaper/heartbeat handling, new generation (new `gen`, new `jti`), old row is already `redeemed_at` and cannot be reused |

**Healing.** The coordinator already has one DO alarm slot shared by timeout and overflow flush
(`mod.rs:944-1040`). A boot deadline per node (`created_at + exp + grace`) must share that slot,
not add a second. On fire, for a node `running` with an unredeemed or expired bootstrap row and
no `StartJob` seen: revoke the row, stop the container (idempotent), increment `gen`, mint, and
redispatch, bounded by an attempt cap so a persistently bad image fails the node rather than
loops (`failed` through the existing `update_node_status` path). The cap value is OQ8.

## 9. Test plan

**Pure logic, plain `cargo test`** (no `worker` dependency, like `ingest_token.rs`):

- mint/verify round trip; deterministic mint (same inputs, identical bytes); different `gen`,
  node or run gives a different `jti`;
- reject: bad MAC, wrong key, wrong `typ`, malformed payload, expired, not-yet-valid
  (`exp <= now`), truncated segments, non-base64;
- cross-type: a bootstrap token fails `ingest_token::verify`; an ingest token fails bootstrap
  verify (both directions);
- the redeem decision function (`row`, `now`, `run_state` to `Redeem | AlreadyRedeemed | Revoked
  | Expired | RunClosed | WrongNode`) exhaustively, as a pure function in `coordinator::logic`;
- `Bootstrap` `Debug` output never contains the token.

**Auth negative cases** (the explicit list the task asks for, each as a pure test of the decision
plus, where the handler is thin, a live check): token reuse; token for a different run; token
for a different repo (`repo_id` in payload vs row); token for a different node; expired;
revoked; run already terminal; no `Authorization` header; non-bootstrap bearer (an ingest token
or a `cc_tok_`) presented to exchange; bootstrap token presented to `StartJob` and to the part
`PUT`.

**FakeExecutor** (`executor.rs`) gains the ability to record the `Bootstrap` it was given so a
test can assert the coordinator minted one per started node and never twice for a duplicate
`startNode`.

**Needs a live run** (`mise run //packages/cloud-ci-worker:dev`, per the crate's own convention
for DO and D1 code): the single-use `UPDATE` under a genuine race; coordinator table creation on
an existing DO; `exec()` env delivery (the token is visible to the agent process and not to an
unrelated `exec`); the cancel race; the boot-deadline reaper under an artificially short `exp`;
the full agent exchange followed by a `StartJob` using the returned token.

## 10. Options and recommendation

| Option | Pros | Cons |
| --- | --- | --- |
| **A. HMAC + coordinator row** (token routes by payload, state in DO) | One writer; no D1 migration; deterministic re-mint tolerates crash-before-delivery; matches `ingest_token.rs` | Needs a new key or HKDF; revocation is by state, not by token |
| B. Random token, hash in D1 | Simple, leak-safe DB | Global table the Worker reads and the coordinator must write, or prefix routing; plaintext cannot be re-derived |
| C. Stateless HMAC only | Trivial | Not single-use; contradicts ADR 0010 |
| Scope: return the run-wide ingest token (matches code today) vs a new `typ=job` token | Ingest: zero verifier changes, no weakening | Run-wide authority from one node's credential |

**Recommendation: Option A, exchange returns the existing ingest token.** It satisfies every
invariant listed in section 3 with the smallest change to verified code, adds no D1 migration,
and makes mint idempotent. The `typ=job` upgrade (OQ1) can follow without changing the bootstrap
design.

## 11. OWNER QUESTIONS (not decided here)

- **OQ1. Scope of what exchange returns.** Run-wide ingest token (1 h, today's format), or a new
  node-scoped `typ=job` token as `auth.md:347` describes (needs verifier work in
  `ingest_token.rs` and every `authenticate_ingest_bearer` caller)?
- **OQ2. Bootstrap TTL.** 5 minutes proposed. Longer for slow-image or cold-start executors?
- **OQ3. Signing key.** Reuse `INGEST_TOKEN_SECRET` with a different `typ`, introduce
  `CLOUD_CI_MASTER_KEY` + HKDF as `auth.md:352-363` says (which the code does not do), or a new
  dedicated secret? Affects deployer setup and rotation.
- **OQ4. RPC name and wire shape**, and whether exchange is a Connect RPC or a plain HTTP route.
- **OQ5. Strict one-use vs bounded replay.** Allow a repeat of a redeem within a short window
  (or from the same source) to survive a lost response, accepting a larger theft window?
- **OQ6. Revoking already-minted ingest tokens** on cancel or close (needs a stateful
  revocation check on every RPC, today stateless) or accept that closed-run writes are refused
  by the coordinator while the bearer remains valid up to 1 h?
- **OQ7. If the patched `worker` fork cannot set `exec()` env:** extend the fork (ADR 0011
  patch), or accept argv delivery, or deliver through a file/stdin channel?
- **OQ8. Redispatch cap and boot-deadline grace** for the healing reaper, and what a node shows
  the user when it exhausts them.
- **OQ9. Should bootstrap events be projected to D1** for audit/dashboards, which adds a
  forward-only migration?

## 12. Unresolved implementation prerequisites

Technical items that must be settled before or during implementation, separate from the owner
questions:

1. **`exec()` env in Rust.** Confirm or add `env` to the patched fork's `exec` options
   (`node_container.rs:356-362` passes `None`). Until then the token cannot be delivered
   safely. `[unverified]`
2. **Key derivation mismatch.** `auth.md:352-363` specifies HKDF from `CLOUD_CI_MASTER_KEY`;
   `ingest_token.rs`/`lib.rs` `ingest_token_secret` use `INGEST_TOKEN_SECRET` raw, and `.dev.vars.example` only
   configures the latter. A bootstrap key needs a decision and a dev/prod config path.
3. **Ingest TTL vs run length.** Ingest token TTL is a fixed 3600 s (`ingest_token.rs:28`) while
   `BeginRun` timeouts are configurable (default 30 min, `byo-ci.md`). A node running longer than
   the remaining ingest lifetime has no refresh path; exchange does not solve this.
4. **Alarm slot.** The boot-deadline reaper must integrate with the single DO alarm
   (`mod.rs:944-1040`) without breaking `release_alarm_if_idle`.
5. **Node-row-before-container ordering** (`mod.rs:2761-2775`) leaves a `running` node with no
   container after a crash; the reaper design above fixes it only if it keys off the bootstrap
   row. Needs an explicit decision on whether to also change that ordering.
6. **`Bootstrap` `Debug`.** Derived `Debug` (`executor.rs:128`) leaks the token; replace with a
   redacting implementation as part of adding a real mint.
7. **Agent behavior.** Agent has no exchange step, holds one static bearer, and must not export
   either token to child processes; `agent.rs:12-22` documents the agent does not pull a job spec,
   so job-spec serving (Dynamic Pipelines) is a separate dependency of the full pull model.
8. **Proto additions.** New rpc and messages in `ingest.proto`, regenerate bindings
   (`mise run //packages/cloud-ci-proto:generate`), run `buf breaking` against `main`.
9. **Redeem signature** between Worker and `RunCoordinator`: a new store method on
   `RunCoordinatorStore` and a coordinator route; must return the row's ids, never echo the
   request's.
10. **`RunCoordinator` rewire through `Executor`.** `start_node_container` still calls
    `node_container::start_container` directly; moving to the trait is the follow-on this design
    unblocks, and `ContainersExecutor::start` must start consuming `bootstrap`.

## 13. Where existing docs and code disagree

- `auth.md:352-363` (HKDF from `CLOUD_CI_MASTER_KEY`, payload with `v`/`sub`/`jti`) vs
  `ingest_token.rs:30-38` and `lib.rs` `ingest_token_secret` (raw `INGEST_TOKEN_SECRET`, no `v`/`sub`/`jti`).
- `auth.md:347` and ADR 0010 describe a `typ=job` token minted by the coordinator and exchanged
  for a bootstrap token; no `typ=job` exists in code, and `verify` accepts only `typ=ingest`.
- `auth.md:337` lists `UploadArtifact` and `FinishRun` as `ingest:write` RPCs; neither exists in
  `ingest.proto:10-20`.
- `auth.md` ("minutes-to-hours lived" tokens) vs the fixed 1 h ingest TTL and configurable run
  timeouts.
- ADR 0010 lists user-data as the boot mechanism for the agent; for Containers the real path is
  `exec()` and `exec()` does not inherit `start()` env, so the ADR table row is incomplete.
