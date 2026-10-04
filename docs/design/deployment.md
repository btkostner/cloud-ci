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

`cloud-ci setup` (no subcommand, `packages/cloud-ci-cli/src/setup_wizard.rs`) is the one
orchestrating command: it walks a deployer through every step below, in order, re-checking
current state before acting so re-running it is safe — a step already satisfied is reported
`skipped (already done)` and nothing is re-sent. It is built entirely on the real, already-shipped
pieces (`cloud-ci setup github-app`/`setup allowed-orgs`, `wrangler` itself) and never fakes a
step: where a step needs a real browser/GitHub account/Cloudflare credentials, the wizard either
runs the real subcommand that does the work and re-verifies its result from `wrangler.toml`
before calling it done, or prints the exact command/instruction for the operator to run
themselves. `--dry-run` runs only read-only checks (`wrangler whoami`, `d1 migrations list`,
`secrets-store secret list`) and performs no mutating command, file write, or deploy.

### Steps, in order

1. **`wrangler login`** — runs `wrangler whoami`; a non-authenticated result is a hard failure
   that stops the wizard (clear message: run `wrangler login` or set `CLOUDFLARE_API_TOKEN`).
2. **`wrangler.toml` bindings** — checks every required binding
   (`DB`/`ASSETS`/`METRICS`/`AI`/the two queues/all five Durable Object bindings) is present in
   `--file` (default `packages/cloud-ci-worker/wrangler.toml`), and that the D1 binding's
   `database_id` is not the repo's placeholder. Missing bindings or a still-placeholder id fail
   with the exact fix (e.g. `wrangler d1 create cloud-ci`); this step never edits the file itself.
3. **D1 migrations** — `wrangler d1 migrations list <db> --remote`, classified positively: the
   exact text "No migrations to apply" is clean, the "Migrations to be applied" heading is
   pending, and anything else — including a non-zero exit — is undeterminable and fails the step
   with no mutating command run. Only a positively-pending result (never `--dry-run`) runs
   `wrangler d1 migrations apply <db> --remote`, first printing which database it targets. A
   failed apply never echoes wrangler's own output — not even redacted — only the database name,
   a non-zero-exit note, and a fixed hint to run the same command by hand to see the full output
   (a prior round's heuristic redaction missed real token shapes such as a JWT or a JSON-quoted
   value; the fix is to print nothing from wrangler at all, not a better heuristic). The listing
   is re-run afterward and must come back positively clean before the step reports done.
4. **Secrets Store secrets** — reads the four `[[secrets_store_secrets]]` bindings
   (`GITHUB_APP_PRIVATE_KEY`/`GITHUB_APP_CLIENT_SECRET`/`GITHUB_WEBHOOK_SECRET`/
   `CLOUD_CI_MASTER_KEY`) out of `wrangler.toml` and, if present, confirms each named secret
   shows up in `wrangler secrets-store secret list` for its store — by exact name match, never a
   substring (`FOO` does not match a listed `FOO_OLD`), paging through every result
   (`--per-page 100`/`--page`, capped at 50 pages — hitting the cap while new names are still
   appearing fails the step rather than concluding absence from a truncated listing). Reading
   stops after the first page with fewer than 100 output rows (wrangler's own page size), or when
   a later page's call itself fails — wrangler 4.145.0 throws a non-zero-exit error ("List
   request returned no secrets.") on a genuinely empty page rather than returning one
   successfully, so that failure is treated as the end of the listing, not as "could not verify"
   — **except** a failure on the very first page, which still means the store could not be
   listed at all (reported manual, not a false absence). No secret value is ever read or printed.
   Missing bindings are reported manual (the next step creates them); a binding that names a
   secret absent from the store is a hard failure naming it.
5. **GitHub App** — if `GITHUB_APP_ID` is already set and the four secret bindings exist, this is
   a no-op. Otherwise it runs `cloud-ci setup github-app` itself (forwarding `--name`,
   `--allowed-orgs`, `--deployment-url`, `--cloudflare-account-id`, `--secrets-store-id`, `--public`
   if given) — the real manifest flow, which still opens a browser and blocks on a human clicking
   "Create GitHub App" (auth.md's one un-drivable step, unchanged) — and only reports it done after
   re-reading `wrangler.toml` and confirming `GITHUB_APP_ID` is now set; a subcommand exit code of
   0 that somehow left `GITHUB_APP_ID` empty is reported as a failure, not a success. Missing flags
   or a missing `CLOUDFLARE_API_TOKEN` print the exact command to run by hand instead of guessing.
   Always adds "each allowed org's owner must install the App from GitHub's UI" to the final
   checklist — this CLI has no way to verify an installation.
6. **Allowed orgs** — compares `--allowed-orgs` (if given) against the current
   `GITHUB_ALLOWED_ORGS`; any login not yet present is added via `cloud-ci setup allowed-orgs
   --add` (one real subcommand call per login), then re-read to confirm. With no `--allowed-orgs`
   flag and a non-empty existing list, this step is a no-op report; an empty list with nothing
   requested is reported manual (the Worker fails closed on an empty allowlist).

After the last step, the wizard prints a **checklist** of everything it could not verify or do
itself: the GitHub-UI installation step, a reminder to `wrangler deploy` if config changed, and
(if any step failed) which step to fix before re-running. The checklist is built the same way
regardless of whether the run as a whole succeeded or stopped early.

### What is exercised and what is not

The step-ordering, idempotent-re-run, failed-prerequisite-stops, dry-run-changes-nothing,
no-raw-output-on-apply-failure, positive migration-state classification (never treating
unrecognized or failed-exit output as pending), paginated exact-name secret matching against
wrangler 4.145.0's documented empty-page failure behavior, announce-before-apply ordering, and
forwarded-value validation behavior are covered by `setup_wizard.rs`'s own unit tests against
fake command-runner and filesystem implementations — no network, no real `wrangler`, no real
Cloudflare/GitHub account, same pattern as the rest of this `setup` family (see "Step by step"
below). Three of those checks (the migration-list failed-exit branch, the
`CLOUDFLARE_API_TOKEN` precheck, and the migration-list `Err` transport-failure branch) were
additionally confirmed load-bearing by removing each one in a scratch copy and observing its own
pinning test fail.

Separately, the built `cloud-ci` binary was run against a real subprocess: a shell-script
`wrangler` on `PATH` that logs every invocation and mimics wrangler 4.145.0's documented output
shapes, including the empty-page `FatalError` on `secrets-store secret list` and an unrecognized
`d1 migrations list` listing — see "Real-subprocess evidence" below for the exact scenarios and
observed calls. `cloud-ci setup --dry-run` was also run once with no fake `wrangler` present at
all: `wrangler` was not on the `PATH` that shell used (it is pinned via mise in
`packages/cloud-ci-worker/mise.toml`, so a shell that has run `mise activate`/`mise exec` there
would find it; that shell had not), and no Cloudflare account was available either. That run
correctly stopped at step 1 with "could not run `wrangler whoami`: No such file or directory"
rather than claiming success.

### Real-subprocess evidence

Built with `CARGO_TARGET_DIR` pointed at a scratch directory (never this repo's own `target/`,
confirmed empty of new files afterward) and run with a shell-script `wrangler` placed first on
`PATH` that logs every invocation and mimics wrangler 4.145.0's documented output shapes,
against a temporary copy of `packages/cloud-ci-worker/wrangler.toml` (the real file was never
written):

- **Unrecognized `d1 migrations list` output, exit 0** — `D1 migrations` reported `FAILED`
  ("could not determine whether D1 ... has pending migrations"); the logged calls were exactly
  `wrangler whoami` then `wrangler d1 migrations list cloud-ci --remote` — zero
  `migrations apply` calls.
- **A pending migration followed by a failing `migrations apply`** — the announce line
  ("D1 migrations: applying all pending migrations to REMOTE database `cloud-ci` ...") printed
  before the step report, `migrations apply` was called exactly once, and the fake script's own
  stderr (deliberately including a JWT-shaped string) never appeared anywhere in the CLI's
  output — grepping the full captured output for that string returned zero matches. The failure
  detail read only: "`wrangler d1 migrations apply cloud-ci --remote` exited non-zero against
  remote database `cloud-ci`. Its output is not shown here: run ... yourself to see the full
  output."
- **An absent secret, reproducing wrangler's real empty-page-failure shape (M1)** — the fake
  `secrets-store secret list` returned a short (3-row) successful page 1 naming three of the four
  required secrets, then a non-zero exit with "List request returned no secrets." for any later
  page — the same shape the review's reading of wrangler 4.145.0's source describes. The step
  reported `FAILED`: "wrangler.toml binds secrets that are not in the Secrets Store:
  CLOUD_CI_MASTER_KEY (cloud-ci-master-key)", from exactly one `secrets-store secret list` call
  (the short-page stop rule fired, so no second, failing call was needed to reach that
  conclusion) — not the `Manual`/"could not be verified" outcome the pre-M1 code produced for
  this same scenario.
- **`--dry-run` against the same pending-migration, absent-secret setup** — logged calls were
  only `wrangler whoami`, `d1 migrations list`, and one `secrets-store secret list`; no
  `migrations apply` or any other mutating call was made.

None of these runs exercised step 5's actual GitHub App manifest flow (it needs a human in a
real browser) or a real Cloudflare account's live D1/Secrets Store state — only wrangler's own
documented CLI contract, mimicked by the fake script.

### Prerequisites the CLI assumes but does not create

1. **A Cloudflare account and API token.** `CLOUDFLARE_API_TOKEN` in the environment — read
   directly by `setup_github_app.rs`, the same credential `wrangler` itself uses, never a CLI
   flag so it never lands in shell history or a process list.
2. **An existing Secrets Store.** `cloud-ci setup github-app --secrets-store-id <id>` creates four
   secrets *inside* a store that must already exist; creating the store itself is explicitly out
   of scope of that command (and of the wizard's step 4 above). Real command: `wrangler
   secrets-store store create <name> --remote`
   (developers.cloudflare.com/secrets-store/integrations/workers/, checked 2026-10-03).
3. **A deployed Worker** to attach the App and secrets to — `wrangler deploy --config
   packages/cloud-ci-worker/wrangler.toml` from the repo root, after resolving the `database_id`
   placeholder gap above. No Deploy-to-Cloudflare button path exists yet (above).
4. **A locally built `cloud-ci` CLI binary.** No published release or `cargo install` path exists
   in this repo today — `packages/cloud-ci-cli/Cargo.toml` sets `publish = false` and its
   `mise.toml` has no build/run task. A deployer runs it from a checkout.

### Step by step (what is real today)

1. `mise install` at the repo root.
2. `wrangler d1 create cloud-ci`, then set the returned id as `database_id` in
   `packages/cloud-ci-worker/wrangler.toml` (prerequisite 3/the gap above) — the wizard's step 2
   detects and reports this placeholder but does not edit the file itself.
3. `wrangler secrets-store store create cloud-ci --remote` (or reuse an existing store) and note
   its id.
4. `wrangler deploy --config packages/cloud-ci-worker/wrangler.toml` from the repo root.
5. From the repo root (or `packages/cloud-ci-cli/` with `cargo run --release --`):
   ```sh
   cloud-ci setup --name "cloud-ci (<company>)" --allowed-orgs <org1,org2> \
     --deployment-url https://<worker-hostname> \
     --cloudflare-account-id <id> --secrets-store-id <id-from-step-3>
   ```
   Runs the `wrangler login`, bindings, and D1-migration checks, then opens a real browser to
   GitHub's manifest flow for the GitHub App step — a human must click "Create GitHub App" in
   their own authenticated session; this step is not simulated anywhere in this repo. On success
   it writes `GITHUB_APP_ID`/`GITHUB_ALLOWED_ORGS` and the four Secrets Store bindings into
   `wrangler.toml`, then adds the requested orgs. Re-running the same command after a partial
   failure (e.g. before the App was created) skips every already-done step and resumes where it
   stopped.
6. `wrangler deploy --config packages/cloud-ci-worker/wrangler.toml` from the repo root — the
   wizard's own final checklist item, not run automatically (same "deploy-time-only" reasoning as
   `AllowedOrgsArgs::deploy`/`GithubAppArgs::deploy`, which this wizard inherits by calling those
   same subcommands without `--deploy`).
7. Each allowed org's owner installs the App from GitHub's own UI — outside this CLI's control;
   until then the deployed Worker's webhook/OAuth/OIDC paths fail closed (auth.md's own note). The
   wizard's checklist always names this step.
8. Later allowlist changes, no browser needed: `cloud-ci setup --allowed-orgs <org1,org2,...>` (or
   `cloud-ci setup allowed-orgs --add <login>` for one login with `--deploy`).

Steps 2–7 above need a real Cloudflare account, a real GitHub account, and (for step 5's App
creation) a real browser session; they were **not executed end to end for this doc** — see "What
is exercised and what is not" above for exactly what was and was not run in this environment.

### Known limitations

Accepted residuals from an independent review of this wizard, not fixed in this round because
each is either a documentation-only gap or a deliberately fail-closed edge case, not a false
success:

- **Secret-name matching reads the whole page's text, not a structured table.** `name_tokens`
  splits every non-identifier character, so it also tokenizes wrangler's own header line (store
  id, page number) and any `Comment`/`Scopes` column wrangler prints alongside each secret name.
  A secret name that happened to also appear in a comment would be (incorrectly) counted present.
  wrangler has no machine-readable (`--format json` or similar) output for this command as of
  2026-10-03's check; parsing its human-table output is the only option today.
- **`--per-page 100` as a maximum is `[unverified]`.** The wizard relies on 100 being the real
  per-page ceiling (and therefore the real page size to detect a "short" last page) for Secrets
  Store secrets; this was not re-confirmed against a live account for this round, only against
  wrangler's own `--help` text, which documents `--per-page`'s default (10) but not its maximum.
- **A single duplicate `[[secrets_store_secrets]]` entry for one binding, where the first copy
  has an empty `store_id`/`secret_name`, produces a misleading error** ("store_id ... has
  unexpected characters") instead of naming the duplicate. This only happens with a hand-edited,
  already-malformed `wrangler.toml`; the step still fails closed, just with the wrong reason.
- **`CmdOutput` carries no exit code**, only a `success` boolean — an apply failure's message can
  say "exited non-zero" but not which code. Adding one is a larger interface change than this
  round's scope.
- **wrangler's exact migration-table and empty-page-error text are read from wrangler
  4.145.0's own distributed source** (the version this repo pins in
  `packages/cloud-ci-worker/mise.toml`), not from a live account in this environment; "Real-
  subprocess evidence" below exercises a script that mimics that text, not wrangler itself.

## Open items for other tracks

- **Worker config** (`packages/cloud-ci-worker/wrangler.toml`, not edited by this doc): the
  placeholder `database_id` blocks both the Deploy-to-Cloudflare button's and plain `wrangler
  deploy`'s automatic D1 provisioning (see above); the wizard detects and reports this but cannot
  provision a database on a deployer's behalf.
- **CLI** (`packages/cloud-ci-cli`, not edited by this doc): still no build/run convenience task
  in its `mise.toml` — a deployer runs `cargo run --release --` (or builds once and runs the
  binary) from a checkout, same as every other `cloud-ci` subcommand.

