# Deployment, upgrades, and setup

Status: Proposed. See [ADR 0003](../adr/0003-single-tenant-deployment.md) for the decision this
doc details.

## Summary

A deployer provisions one Worker plus its bindings into their own Cloudflare account
([ADR 0003](../adr/0003-single-tenant-deployment.md)), registers a GitHub App against it with
`cloud-ci setup github-app`, and later applies schema and code upgrades on their own schedule.
This doc covers three things: what actually happens when a live deployment is upgraded, what
Cloudflare's Deploy-to-Cloudflare button can and cannot do for this repo today, and the concrete
steps a deployer runs today to go from nothing to a working deployment, built entirely on the
`cloud-ci setup` subcommands that already exist.

## Upgrading a live deployment

Schema evolution happens through three independent mechanisms. None is wired to the others —
applying one does not trigger or require the others, and nothing in this repo enforces an order
between them.

**1. D1 SQL migrations** — `packages/cloud-ci-worker/migrations/0001_runs_and_jobs.sql` through
`0020_runs_settings_sha.sql`. 0001 is the only full `CREATE TABLE` file; every later file is a
numbered, forward-only `ALTER TABLE ... ADD COLUMN` (e.g. 0020 in full: `ALTER TABLE runs ADD
COLUMN settings_sha TEXT;`). These tables are explicitly documented, repeatedly, as read-only D1
*projections* — the authoritative state lives in each Durable Object's own SQLite (next
paragraph). `wrangler.toml`'s `migrations_dir = "migrations"` is Wrangler's own convention for
`wrangler d1 migrations apply`; no wrapper exists anywhere in this repo (no mise task, no CI
step), so a deployer runs it themselves: `wrangler d1 migrations apply cloud-ci --remote` from
`packages/cloud-ci-worker/`, using the **database name** (`cloud-ci`), not the binding (`DB`) —
Cloudflare's own migrations reference recommends the name specifically because a binding name can
change in code while the database name cannot (developers.cloudflare.com/d1/reference/migrations/,
checked 2026-10-03). There is no down-migration mechanism; a mistaken column stays forever.

**2. Durable Object class migrations** — five `[[migrations]]` tag blocks (`v1`–`v5`) in
`packages/cloud-ci-worker/wrangler.toml`, each adding exactly one class to `new_sqlite_classes`
(`RunCoordinator`, `PullRequestState`, `RepoState`, `ContainerProbe`, `NodeContainer`). This is
Cloudflare's own DO migration mechanism, applied automatically by `wrangler deploy` — every
comment on these blocks states they must never be rewritten or reordered once shipped.

**3. In-code guarded `ALTER`** — `ensure_schema(&SqlStorage)` in `src/coordinator/mod.rs`,
`src/repo_state/mod.rs`, and `src/pull_request_state/mod.rs`, called at the top of every
fetch/alarm handler (per-request, not once at deploy time). Each does `CREATE TABLE IF NOT
EXISTS` for every table that DO owns, then, for columns added after a table's original version,
checks `pragma_table_info` for the column's presence before running `ALTER TABLE ... ADD COLUMN`
— never a blind `ALTER` with errors swallowed as "probably a duplicate column," which would also
mask a real failure (disk full, corrupted schema) until a later, confusing error on an unrelated
query. Two retrofits exist today: `run.settings_sha` and `node.physical_address`
(`coordinator/mod.rs`). Because this runs per request, a Durable Object instance that hasn't been
touched since a code deploy can still be running the old schema while other instances have
already self-upgraded — there is no coordinated "upgrade every instance now" step.

### No enforced ordering, no rollback

[ADR 0003](../adr/0003-single-tenant-deployment.md#consequences)'s own worked example is the one
place this risk is named concretely, not generically: a change to how `NodeContainer` addresses a
node's real container requires manually draining every `NodeContainer` actor from the previous
deployment first — including already-"terminal" nodes, since `run_and_report` reports an exit
code but never calls `destroy()` itself, so an old actor's container can be left running and
unreachable by a new addressing scheme either way. There is no automated drain step in code for
this; it is operator-performed and only documented in prose.

### Practical upgrade order (synthesized from the above, not a fourth mechanism)

1. Check every new file under `packages/cloud-ci-worker/migrations/` since the last upgrade —
   every one shipped so far is a pure additive `ALTER TABLE ... ADD COLUMN`, but confirm a new one
   is too before assuming the rest of this order holds.
2. `wrangler deploy --config packages/cloud-ci-worker/wrangler.toml` from the repo root (the
   file's own `[build].cwd` comment: relative `cwd` resolves against the invoking process's cwd,
   and every task that invokes this config does so from the repo root). This applies the DO class
   migration (mechanism 2) and ships the new `ensure_schema` (mechanism 3), which self-heals each
   DO instance's schema the next time it's touched.
3. `wrangler d1 migrations apply cloud-ci --remote` from `packages/cloud-ci-worker/` for the new
   `.sql` files (mechanism 1). For purely additive columns this can run before or after step 2;
   D1 is a read-only projection, not consulted by the DO's own correctness-critical logic.
4. If the change is **not** purely additive to an existing shape (the `NodeContainer`
   precedent above) — drain the affected Durable Object instances before the deploy in step 2,
   following that ADR's worked example; there is no generic tooling for this.

There is no rollback for any of the three mechanisms. Reversing a mistaken change means writing
and shipping a new forward migration/`ALTER` that undoes it, same as any other change.

## Deploy-to-Cloudflare

[ADR 0003](../adr/0003-single-tenant-deployment.md)'s Decision names "`wrangler deploy` (or a
Deploy-to-Cloudflare button)" as the two intended setup paths. The button does not exist yet, and
cannot work against this repo's current structure without restructuring it first.

**Why it would fail today.** Cloudflare's own Deploy-to-Cloudflare docs state the exact
requirement (developers.cloudflare.com/workers/platform/deploy-buttons/, "Last updated Jul 22,
2026", § Limitations › Monorepos): "If your repository URL contains a subdirectory, your
application must be fully isolated within that subdirectory, including any dependencies.
Otherwise, the build will fail." `packages/cloud-ci-worker/Cargo.toml` depends on three sibling
crates by relative path — `cloud-ci-core = { path = "../cloud-ci-core" }`, `cloud-ci-proto = {
path = "../cloud-ci-proto-rust" }`, `cloud-ci-reports = { path = "../cloud-ci-reports" }` — none
of which live inside `packages/cloud-ci-worker/`. Its own `wrangler.toml` `[build]` section and
`mise.toml`'s `dev` task both also require running from the repo root, not the package directory
alone. A button pointed at `.../tree/main/packages/cloud-ci-worker` would clone only that
subdirectory as the new repository's root; the sibling crates the build needs would not exist
there, and the build would fail exactly as Cloudflare's own docs warn. Making this subdirectory
self-contained (vendoring or publishing the three sibling crates, making the build path-
independent of the repo root) is a packaging/build change, not a docs change — out of scope here,
documented as the real gap instead of a button that would not work.

**If that gap is closed later:** the button's automatic resource provisioning
(same page, § Automatic resource provisioning) currently covers D1, R2, KV, Durable Objects,
Queues, and Workers AI — every binding this Worker uses except Analytics Engine and Containers.
Analytics Engine needs no provisioning step regardless: a dataset is created automatically on
first write once the binding exists (developers.cloudflare.com/analytics/analytics-engine/get-started/,
checked 2026-10-03). Whether Workers Builds (the build step the button uses) can build and push
this Worker's `container_probe/Dockerfile` the way a local `wrangler deploy` does is **[unverified]**
— no source read for this doc confirms or rules it out.

**Separate, narrower gap that blocks plain `wrangler deploy` auto-provisioning too:**
`packages/cloud-ci-worker/wrangler.toml` currently pins a placeholder `database_id =
"00000000-0000-0000-0000-000000000000"` for its D1 binding. Wrangler's own automatic resource
provisioning for a plain `wrangler deploy` (general availability note: wrangler ≥ 4.45.0 per the
Oct 24, 2025 changelog; this repo pins 4.145.0) only triggers when a binding's resource id is
*omitted* from the config — a filled-in placeholder makes `wrangler deploy` try to attach to a D1
database that does not exist and fail, rather than create one. A deployer must run `wrangler d1
create cloud-ci` and set the real id (or the config owner replaces the placeholder) before the
first production deploy succeeds, regardless of which setup path is used. This is a
`packages/cloud-ci-worker/wrangler.toml` change, not made here — see the note to the worker-owning
track at the end of this doc.

## Setup wizard

`cloud-ci setup github-app` and `cloud-ci setup allowed-orgs`
(`packages/cloud-ci-cli/src/setup_github_app.rs`, `.../src/setup.rs`) are real, implemented, and
already documented step by step with a full sequence diagram in
[auth.md § GitHub App setup](./auth.md#github-app-setup). This section covers the steps around
them that a fresh deployer needs and the CLI does not perform itself.

### Prerequisites the CLI assumes but does not create

1. **A Cloudflare account and API token.** `CLOUDFLARE_API_TOKEN` in the environment — read
   directly by `setup_github_app.rs`, the same credential `wrangler` itself uses, never a CLI
   flag so it never lands in shell history or a process list.
2. **An existing Secrets Store.** `cloud-ci setup github-app --secrets-store-id <id>` creates four
   secrets *inside* a store that must already exist; creating the store itself is explicitly out
   of scope of that command. Real command: `wrangler secrets-store store create <name> --remote`
   (developers.cloudflare.com/secrets-store/integrations/workers/, checked 2026-10-03).
3. **A deployed Worker** to attach the App and secrets to — `wrangler deploy --config
   packages/cloud-ci-worker/wrangler.toml` from the repo root, after resolving the `database_id`
   placeholder gap above. No Deploy-to-Cloudflare button path exists yet (above).
4. **A locally built `cloud-ci` CLI binary.** No published release or `cargo install` path exists
   in this repo today — `packages/cloud-ci-cli/Cargo.toml` sets `publish = false` and its
   `mise.toml` has no build/run task. A deployer runs it from a checkout.

### Step by step (what is real today)

1. `mise install` at the repo root.
2. `wrangler login` (or set `CLOUDFLARE_API_TOKEN`).
3. `wrangler d1 create cloud-ci`, then set the returned id as `database_id` in
   `packages/cloud-ci-worker/wrangler.toml` (prerequisite 3/the gap above).
4. `wrangler secrets-store store create cloud-ci --remote` (or reuse an existing store) and note
   its id.
5. `wrangler deploy --config packages/cloud-ci-worker/wrangler.toml` from the repo root.
6. From `packages/cloud-ci-cli/`:
   ```sh
   cargo run --release -- setup github-app \
     --name "cloud-ci (<company>)" --allowed-orgs <org1,org2> \
     --deployment-url https://<worker-hostname> \
     --cloudflare-account-id <id> --secrets-store-id <id-from-step-4> \
     --deploy
   ```
   Opens a real browser to GitHub's manifest flow — a human must click "Create GitHub App" in
   their own authenticated session; this step is not simulated anywhere in this repo. On success
   it writes `GITHUB_APP_ID`/`GITHUB_ALLOWED_ORGS` and the four Secrets Store bindings into
   `wrangler.toml` and (because `--deploy` was passed) redeploys.
7. Each allowed org's owner installs the App from GitHub's own UI — outside this CLI's control;
   until then the deployed Worker's webhook/OAuth/OIDC paths fail closed (auth.md's own note).
8. Later allowlist changes, no browser needed: `cargo run --release -- setup allowed-orgs --add
   <login> --deploy` from the same directory.

Steps 2 and 6–7 need a real Cloudflare account, a real GitHub account, and a real browser session
and were **not executed for this doc** — they are documented from the CLI's own source and
`auth.md`'s sequence diagram, not run end to end. The CLI's unit tests
(`setup.rs`'s pure `parse_logins`/`apply_op`/`serialize_logins`/`mutate_allowed_orgs`) are the
only part of this flow exercised by this repository's own test suite.

## Open items for other tracks

- **Worker config** (`packages/cloud-ci-worker/wrangler.toml`, not edited by this doc): the
  placeholder `database_id` blocks both the Deploy-to-Cloudflare button's and plain `wrangler
  deploy`'s automatic D1 provisioning (see above).
- **CLI** (`packages/cloud-ci-cli`, not edited by this doc): no build/run convenience task in its
  `mise.toml`, and no single orchestrating subcommand that runs the D1/Secrets-Store/deploy steps
  above before `setup github-app` — today's wizard is this doc's manual composition of real,
  separately-built pieces, not one guided command.
