//! `cloud-ci setup` (no subcommand): an idempotent walk through the real
//! setup steps in order, per `docs/design/deployment.md`'s "Setup wizard".
//!
//! Steps, in order: wrangler login, `wrangler.toml` bindings, D1
//! migrations, Secrets Store secrets, GitHub App, allowed orgs, then a
//! final checklist. Every step first inspects current state and is
//! skipped (and reported as such) when already satisfied, so re-running is
//! safe. A failed prerequisite stops the run; later steps are reported as
//! not run.
//!
//! # What this never does
//!
//! - Fake a step. The GitHub App step runs the real `cloud-ci setup
//!   github-app` subcommand (which needs a human in a browser) or prints
//!   the exact command; it is only reported done after re-reading
//!   `wrangler.toml` confirms `GITHUB_APP_ID` is set.
//! - Print secrets. Command output is only parsed, never echoed (a failed
//!   `d1 migrations apply` prints just the database, a non-zero-exit note
//!   and a fixed hint), and secret values are never read; only secret
//!   *names* from `wrangler.toml` are shown.
//! - Mutate anything in `--dry-run`: only read-only commands run
//!   (`wrangler whoami`, `d1 migrations list`, `secrets-store secret list`).
//!
//! Side effects go through the [`Runner`] and [`Fs`] traits so tests use
//! fakes with no network, filesystem, or subprocesses.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item};

use crate::cli::WizardArgs;

/// Captured result of a finished command. Contents are used for
/// parsing only and are never printed (no redaction is attempted, so none
/// is relied on).
pub struct CmdOutput {
    pub success: bool,
    pub stdout: String,
}

pub trait Runner {
    /// Runs `program` with captured output.
    fn run(&self, program: &str, args: &[String], cwd: Option<&Path>) -> Result<CmdOutput, String>;
    /// Runs `program` with inherited stdio (interactive); returns success.
    fn run_inherit(&self, program: &str, args: &[String]) -> Result<bool, String>;
    fn env_is_set(&self, name: &str) -> bool;
    /// Prints a line to the operator immediately, before the run's final
    /// report, so a mutating command is announced before it starts.
    fn announce(&self, msg: &str);
}

pub trait Fs {
    fn read_to_string(&self, path: &Path) -> Result<String, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Already satisfied; nothing was done.
    AlreadyDone,
    /// This run changed something and re-verified it.
    Applied,
    /// Dry run: this would be done.
    WouldApply,
    /// Needs a human; the detail carries the exact instruction.
    Manual,
    /// A prerequisite or action failed; the run stops here.
    Failed,
    /// An earlier step failed.
    NotRun,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::AlreadyDone => "skipped (already done)",
            Status::Applied => "done",
            Status::WouldApply => "would run (dry run)",
            Status::Manual => "manual action needed",
            Status::Failed => "FAILED",
            Status::NotRun => "not run",
        })
    }
}

#[derive(Debug, Clone)]
pub struct StepReport {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub steps: Vec<StepReport>,
    pub checklist: Vec<String>,
}

impl Report {
    pub fn failed(&self) -> bool {
        self.steps.iter().any(|s| s.status == Status::Failed)
    }

    pub fn render(&self, dry_run: bool) -> String {
        let mut out = String::new();
        if dry_run {
            out.push_str("cloud-ci setup (dry run: nothing is changed)\n");
        } else {
            out.push_str("cloud-ci setup\n");
        }
        for (i, s) in self.steps.iter().enumerate() {
            out.push_str(&format!("{}. {}: {}\n", i + 1, s.name, s.status));
            if !s.detail.is_empty() {
                for line in s.detail.lines() {
                    out.push_str(&format!("     {line}\n"));
                }
            }
        }
        out.push_str("\nChecklist (needs manual action or could not be verified):\n");
        if self.checklist.is_empty() {
            out.push_str("  (nothing)\n");
        }
        for item in &self.checklist {
            out.push_str(&format!("  [ ] {item}\n"));
        }
        out
    }
}

struct Outcome {
    status: Status,
    detail: String,
}

fn out(status: Status, detail: impl Into<String>) -> Outcome {
    Outcome {
        status,
        detail: detail.into(),
    }
}

struct Ctx<'a> {
    runner: &'a dyn Runner,
    fs: &'a dyn Fs,
    args: &'a WizardArgs,
    exe: &'a str,
    checklist: Vec<String>,
}

const PLACEHOLDER_DB_ID: &str = "00000000-0000-0000-0000-000000000000";
const SECRET_BINDINGS: [&str; 4] = [
    "GITHUB_APP_PRIVATE_KEY",
    "GITHUB_APP_CLIENT_SECRET",
    "GITHUB_WEBHOOK_SECRET",
    "CLOUD_CI_MASTER_KEY",
];

fn s(v: &str) -> String {
    v.to_string()
}

/// An identifier-like value (database name, store id, account id, org
/// login) that is forwarded to a subprocess: non-empty, only letters,
/// digits, `.`, `_`, `-`, and never starting with `-` (so it cannot be
/// parsed as a flag).
fn valid_ident(v: &str) -> bool {
    !v.is_empty()
        && !v.starts_with('-')
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Free text forwarded as a flag value (`--name`, `--deployment-url`):
/// non-empty, no control characters, never starting with `-`.
fn valid_free_text(v: &str) -> bool {
    !v.is_empty() && !v.starts_with('-') && !v.chars().any(char::is_control)
}

fn valid_orgs_list(v: &str) -> bool {
    let l = logins(v);
    !l.is_empty() && l.iter().all(|x| valid_ident(x))
}

fn load_doc(ctx: &Ctx<'_>) -> Result<DocumentMut, String> {
    let text = ctx
        .fs
        .read_to_string(&ctx.args.file)
        .map_err(|e| format!("cannot read {}: {e}", ctx.args.file.display()))?;
    // Report only the line number and the parser's description of the
    // error kind, never source text (it may sit next to a secret).
    text.parse::<DocumentMut>().map_err(|e| {
        let line = e
            .span()
            .and_then(|sp| text.get(..sp.start))
            .map(|prefix| prefix.matches('\n').count() + 1);
        match line {
            Some(l) => format!(
                "{} is not valid TOML: {} (line {l})",
                ctx.args.file.display(),
                e.message()
            ),
            None => format!(
                "{} is not valid TOML: {}",
                ctx.args.file.display(),
                e.message()
            ),
        }
    })
}

fn has_entry(doc: &DocumentMut, path: &[&str], key: &str, value: &str) -> bool {
    let mut item: Option<&Item> = doc.get(path[0]);
    for p in &path[1..] {
        item = item.and_then(|i| i.get(*p));
    }
    item.and_then(Item::as_array_of_tables).is_some_and(|a| {
        a.iter()
            .any(|t| t.get(key).and_then(Item::as_str) == Some(value))
    })
}

fn var(doc: &DocumentMut, name: &str) -> String {
    doc.get("vars")
        .and_then(|v| v.get(name))
        .and_then(Item::as_str)
        .unwrap_or("")
        .to_string()
}

fn database_name(doc: &DocumentMut) -> String {
    doc.get("d1_databases")
        .and_then(Item::as_array_of_tables)
        .and_then(|a| {
            a.iter()
                .find(|t| t.get("binding").and_then(Item::as_str) == Some("DB"))
        })
        .and_then(|t| t.get("database_name"))
        .and_then(Item::as_str)
        .unwrap_or("cloud-ci")
        .to_string()
}

fn step_login(ctx: &mut Ctx<'_>) -> Outcome {
    match ctx.runner.run("wrangler", &[s("whoami")], None) {
        Ok(o) if o.success && !o.stdout.to_lowercase().contains("not authenticated") => {
            out(Status::AlreadyDone, "wrangler is authenticated")
        }
        Ok(_) => out(
            Status::Failed,
            "wrangler is not logged in. Run `wrangler login` (or set CLOUDFLARE_API_TOKEN), then re-run `cloud-ci setup`.",
        ),
        Err(e) => out(
            Status::Failed,
            format!(
                "could not run `wrangler whoami`: {e}. Run `mise install` so wrangler is on PATH."
            ),
        ),
    }
}

fn step_bindings(ctx: &mut Ctx<'_>) -> Outcome {
    let doc = match load_doc(ctx) {
        Ok(d) => d,
        Err(e) => return out(Status::Failed, e),
    };
    let mut missing: Vec<String> = Vec::new();
    let checks: [(&[&str], &str, &str); 10] = [
        (&["d1_databases"], "binding", "DB"),
        (&["r2_buckets"], "binding", "ASSETS"),
        (&["analytics_engine_datasets"], "binding", "METRICS"),
        (&["queues", "producers"], "binding", "ANALYSIS_QUEUE"),
        (&["queues", "producers"], "binding", "MERGE_QUEUE"),
        (&["durable_objects", "bindings"], "name", "RUN_COORDINATOR"),
        (
            &["durable_objects", "bindings"],
            "name",
            "PULL_REQUEST_STATE",
        ),
        (&["durable_objects", "bindings"], "name", "REPO_STATE"),
        (&["durable_objects", "bindings"], "name", "CONTAINER_PROBE"),
        (&["durable_objects", "bindings"], "name", "NODE_CONTAINER"),
    ];
    for (path, key, val) in checks {
        if !has_entry(&doc, path, key, val) {
            missing.push(val.to_string());
        }
    }
    if doc
        .get("ai")
        .and_then(|t| t.get("binding"))
        .and_then(Item::as_str)
        != Some("AI")
    {
        missing.push(s("AI"));
    }
    if !missing.is_empty() {
        return out(
            Status::Failed,
            format!(
                "{} is missing required bindings: {}. Restore them from packages/cloud-ci-worker/wrangler.toml in the repo.",
                ctx.args.file.display(),
                missing.join(", ")
            ),
        );
    }
    let id_ok = doc
        .get("d1_databases")
        .and_then(Item::as_array_of_tables)
        .and_then(|a| {
            a.iter()
                .find(|t| t.get("binding").and_then(Item::as_str) == Some("DB"))
        })
        .and_then(|t| t.get("database_id"))
        .and_then(Item::as_str)
        .is_some_and(|id| !id.is_empty() && id != PLACEHOLDER_DB_ID);
    if !id_ok {
        return out(
            Status::Failed,
            format!(
                "the D1 binding DB still has the placeholder database_id. Run `wrangler d1 create {}` and set the returned id as database_id in {}, then re-run.",
                database_name(&doc),
                ctx.args.file.display()
            ),
        );
    }
    out(
        Status::AlreadyDone,
        "all required bindings present, D1 database_id is set",
    )
}

fn migrations_cwd(ctx: &Ctx<'_>) -> Option<PathBuf> {
    ctx.args.file.parent().map(Path::to_path_buf)
}

#[derive(Debug, PartialEq, Eq)]
enum MigState {
    /// Positively recognized: nothing left to apply.
    Clean,
    /// Positively recognized: at least one migration is pending.
    Pending,
    /// Failed exit or unrecognized output. Never a reason to mutate.
    Unknown,
}

/// Classifies `wrangler d1 migrations list` output. Only text that is
/// positively recognized counts: "No migrations to apply" is clean, the
/// "Migrations to be applied" heading is pending, and everything else,
/// including any failed exit, is unknown.
fn classify_migrations(success: bool, text: &str) -> MigState {
    if !success {
        return MigState::Unknown;
    }
    if text.contains("No migrations to apply") {
        MigState::Clean
    } else if text.contains("Migrations to be applied") {
        MigState::Pending
    } else {
        MigState::Unknown
    }
}

fn migrations_state(ctx: &Ctx<'_>, db: &str) -> MigState {
    let cwd = migrations_cwd(ctx);
    let args = [s("d1"), s("migrations"), s("list"), s(db), s("--remote")];
    match ctx.runner.run("wrangler", &args, cwd.as_deref()) {
        Ok(o) => classify_migrations(o.success, &o.stdout),
        Err(_) => MigState::Unknown,
    }
}

fn step_migrations(ctx: &mut Ctx<'_>) -> Outcome {
    let db = match load_doc(ctx) {
        Ok(d) => database_name(&d),
        Err(e) => return out(Status::Failed, e),
    };
    if !valid_ident(&db) {
        return out(
            Status::Failed,
            "the D1 database_name in wrangler.toml is empty or has unexpected characters (allowed: letters, digits, '.', '_', '-', not starting with '-'); no command was run",
        );
    }
    let manual = format!(
        "wrangler d1 migrations apply {db} --remote   (run from the directory containing wrangler.toml)"
    );
    match migrations_state(ctx, &db) {
        MigState::Unknown => out(
            Status::Failed,
            format!(
                "could not determine whether D1 `{db}` has pending migrations (the list command failed or its output was not recognized). No migration was applied. Check with `wrangler d1 migrations list {db} --remote`, apply by hand if needed: {manual}"
            ),
        ),
        MigState::Clean => out(
            Status::AlreadyDone,
            format!("D1 `{db}` has no unapplied migrations"),
        ),
        MigState::Pending if ctx.args.dry_run => out(
            Status::WouldApply,
            format!("D1 `{db}` has unapplied migrations; would run: {manual}"),
        ),
        MigState::Pending => {
            ctx.runner.announce(&format!(
                "D1 migrations: applying all pending migrations to REMOTE database `{db}` (wrangler d1 migrations apply {db} --remote)"
            ));
            let cwd = migrations_cwd(ctx);
            let args = [
                s("d1"),
                s("migrations"),
                s("apply"),
                db.clone(),
                s("--remote"),
            ];
            match ctx.runner.run("wrangler", &args, cwd.as_deref()) {
                Ok(o) if o.success => match migrations_state(ctx, &db) {
                    MigState::Clean => out(
                        Status::Applied,
                        format!("applied D1 migrations to remote `{db}` and re-verified"),
                    ),
                    _ => out(
                        Status::Failed,
                        format!(
                            "apply reported success but `{db}` is not confirmed clean afterwards; check with `wrangler d1 migrations list {db} --remote`"
                        ),
                    ),
                },
                Ok(_) => out(
                    Status::Failed,
                    format!(
                        "`wrangler d1 migrations apply {db} --remote` exited non-zero against remote database `{db}`. Its output is not shown here: run `wrangler d1 migrations apply {db} --remote` yourself to see the full output."
                    ),
                ),
                Err(e) => out(
                    Status::Failed,
                    format!(
                        "could not run `wrangler d1 migrations apply`: {e}. Run it by hand: {manual}"
                    ),
                ),
            }
        }
    }
}

/// Secret binding entries in `wrangler.toml`: (binding, store_id, secret_name).
fn secret_entries(doc: &DocumentMut) -> Vec<(String, String, String)> {
    doc.get("secrets_store_secrets")
        .and_then(Item::as_array_of_tables)
        .map(|a| {
            a.iter()
                .map(|t| {
                    let g = |k: &str| t.get(k).and_then(Item::as_str).unwrap_or("").to_string();
                    (g("binding"), g("store_id"), g("secret_name"))
                })
                .collect()
        })
        .unwrap_or_default()
}

const SECRETS_PER_PAGE_N: usize = 100;
const SECRETS_PER_PAGE: &str = "100";
const SECRETS_MAX_PAGES: usize = 50;

enum ListError {
    /// A list call failed; the store's contents are unknown.
    Unavailable,
    /// Still producing new names after the page cap; fail closed.
    TooManyPages,
}

/// Name-shaped tokens in `wrangler secrets-store secret list` output.
/// Matching is done on whole tokens, never substrings.
fn name_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .filter(|t| !t.is_empty())
}

/// Names read from a store listing, and whether the listing is proven
/// complete (so a missing name is really absent).
struct Listing {
    names: HashSet<String>,
    complete: bool,
}

/// wrangler 4.145.0's error text for an EMPTY page (`[unverified]` against
/// a live account: read from its distributed source). Only this text after
/// a full page proves the listing ended; any other failure may be transient.
const EMPTY_PAGE_MARKER: &str = "List request returned no secrets";

/// Reads `wrangler secrets-store secret list` pages (`--page`/`--per-page`)
/// and returns every token seen plus whether the listing is complete.
///
/// The loop ends in one of these ways:
/// - a page with fewer than [`SECRETS_PER_PAGE_N`] non-empty output lines
///   (header and border lines only inflate the count, which is the safe
///   direction: it can only cause one more request): complete;
/// - a page that adds no new token: complete;
/// - every wanted name seen: complete (nothing left to look for);
/// - a failed call after a full page whose text contains
///   [`EMPTY_PAGE_MARKER`]: complete (wrangler's real empty-page error);
/// - any OTHER failed call after a full page (a transient error, or a
///   runner error): NOT complete, so a missing name is unconfirmed rather
///   than absent.
///
/// A failed FIRST page is [`ListError::Unavailable`]. Hitting the page cap
/// while pages still add tokens is [`ListError::TooManyPages`], so absence
/// is never concluded from a truncated listing.
fn list_store_secrets(ctx: &Ctx<'_>, store: &str, wanted: &[&str]) -> Result<Listing, ListError> {
    let mut seen: HashSet<String> = HashSet::new();
    for page in 1..=SECRETS_MAX_PAGES {
        let args = [
            s("secrets-store"),
            s("secret"),
            s("list"),
            s(store),
            s("--remote"),
            s("--per-page"),
            s(SECRETS_PER_PAGE),
            s("--page"),
            page.to_string(),
        ];
        let o = match ctx.runner.run("wrangler", &args, None) {
            Ok(o) if o.success => o,
            Ok(o) if page > 1 => {
                return Ok(Listing {
                    names: seen,
                    complete: o.stdout.contains(EMPTY_PAGE_MARKER),
                });
            }
            Err(_) if page > 1 => {
                return Ok(Listing {
                    names: seen,
                    complete: false,
                });
            }
            _ => return Err(ListError::Unavailable),
        };
        let mut grew = false;
        for t in name_tokens(&o.stdout) {
            if seen.insert(t.to_string()) {
                grew = true;
            }
        }
        let rows = o.stdout.lines().filter(|l| !l.trim().is_empty()).count();
        if rows < SECRETS_PER_PAGE_N || !grew || wanted.iter().all(|w| seen.contains(*w)) {
            return Ok(Listing {
                names: seen,
                complete: true,
            });
        }
    }
    Err(ListError::TooManyPages)
}

fn step_secrets(ctx: &mut Ctx<'_>) -> Outcome {
    let doc = match load_doc(ctx) {
        Ok(d) => d,
        Err(e) => return out(Status::Failed, e),
    };
    let entries = secret_entries(&doc);
    let missing: Vec<&str> = SECRET_BINDINGS
        .iter()
        .copied()
        .filter(|b| {
            !entries
                .iter()
                .any(|(eb, st, sn)| eb == b && !st.is_empty() && !sn.is_empty())
        })
        .collect();
    if !missing.is_empty() {
        ctx.checklist.push(s(
            "Create a Secrets Store if you have none: `wrangler secrets-store store create cloud-ci --remote`, then pass its id as --secrets-store-id.",
        ));
        return out(
            Status::Manual,
            format!(
                "Secrets Store bindings not in wrangler.toml: {}. The GitHub App step creates the secrets and writes these bindings.",
                missing.join(", ")
            ),
        );
    }
    // Bindings exist; verify the named secrets exist in the store (names only).
    let bound: Vec<(&str, &str, &str)> = SECRET_BINDINGS
        .iter()
        .filter_map(|b| {
            entries
                .iter()
                .find(|(eb, st, sn)| eb == b && !st.is_empty() && !sn.is_empty())
                .map(|(_, st, sn)| (*b, st.as_str(), sn.as_str()))
        })
        .collect();
    if bound.iter().any(|(_, st, _)| !valid_ident(st)) {
        return out(
            Status::Failed,
            "a secrets_store_secrets store_id in wrangler.toml has unexpected characters (allowed: letters, digits, '.', '_', '-', not starting with '-'); no command was run",
        );
    }
    let mut stores: Vec<&str> = Vec::new();
    for (_, st, _) in &bound {
        if !stores.contains(st) {
            stores.push(st);
        }
    }
    let mut unverified: Vec<String> = Vec::new();
    let mut absent: Vec<String> = Vec::new();
    let mut unconfirmed: Vec<String> = Vec::new();
    for store in stores {
        let wanted: Vec<&str> = bound
            .iter()
            .filter(|(_, st, _)| *st == store)
            .map(|(_, _, sn)| *sn)
            .collect();
        match list_store_secrets(ctx, store, &wanted) {
            Ok(listing) => {
                for (b, st, sn) in &bound {
                    if *st == store && !listing.names.contains(*sn) {
                        if listing.complete {
                            absent.push(format!("{b} ({sn})"));
                        } else {
                            unconfirmed.push(format!("{b} ({sn})"));
                        }
                    }
                }
            }
            Err(ListError::TooManyPages) => {
                return out(
                    Status::Failed,
                    format!(
                        "the Secrets Store listing still had new entries after {SECRETS_MAX_PAGES} pages, so the secrets cannot be verified from here. Check with `wrangler secrets-store secret list <store-id> --remote --page N`."
                    ),
                );
            }
            Err(ListError::Unavailable) => {
                for (b, st, _) in &bound {
                    if *st == store {
                        unverified.push((*b).to_string());
                    }
                }
            }
        }
    }
    if !absent.is_empty() {
        return out(
            Status::Failed,
            format!(
                "wrangler.toml binds secrets that are not in the Secrets Store: {}. Re-run `cloud-ci setup github-app` or create them with `wrangler secrets-store secret create`.",
                absent.join(", ")
            ),
        );
    }
    if !unconfirmed.is_empty() {
        ctx.checklist.push(format!(
            "Confirm these secrets exist in the Secrets Store (the listing may have been cut short by an error): {}",
            unconfirmed.join(", ")
        ));
        return out(
            Status::Manual,
            format!(
                "could not confirm: the Secrets Store listing may have been cut short by an error, so these were not found but are not proven absent: {}",
                unconfirmed.join(", ")
            ),
        );
    }
    if !unverified.is_empty() {
        ctx.checklist.push(format!(
            "Confirm these secrets exist in the Secrets Store (could not list it): {}",
            unverified.join(", ")
        ));
        return out(
            Status::Manual,
            "bindings are present but the store could not be listed, so the secrets are unverified",
        );
    }
    out(
        Status::AlreadyDone,
        "all four secret bindings present and found in the Secrets Store",
    )
}

fn step_github_app(ctx: &mut Ctx<'_>) -> Outcome {
    let doc = match load_doc(ctx) {
        Ok(d) => d,
        Err(e) => return out(Status::Failed, e),
    };
    let entries = secret_entries(&doc);
    let bindings_ok = SECRET_BINDINGS
        .iter()
        .all(|b| entries.iter().any(|(eb, _, _)| eb == b));
    if !var(&doc, "GITHUB_APP_ID").is_empty() && bindings_ok {
        ctx.checklist.push(s(
            "Each allowed org's owner must install the GitHub App from GitHub's UI (cannot be verified from here).",
        ));
        return out(
            Status::AlreadyDone,
            "GITHUB_APP_ID is set and secret bindings exist",
        );
    }
    let a = ctx.args;
    if a.file.to_string_lossy().starts_with('-') {
        return out(
            Status::Failed,
            "--file must not start with '-'; no command was run",
        );
    }
    let mut cmd = vec![s("setup"), s("github-app")];
    let mut missing: Vec<&str> = Vec::new();
    let mut invalid: Vec<&str> = Vec::new();
    type FlagSpec<'a> = (&'static str, &'a Option<String>, fn(&str) -> bool);
    let flags: [FlagSpec<'_>; 5] = [
        ("--name", &a.name, valid_free_text),
        ("--allowed-orgs", &a.allowed_orgs, valid_orgs_list),
        ("--deployment-url", &a.deployment_url, valid_free_text),
        (
            "--cloudflare-account-id",
            &a.cloudflare_account_id,
            valid_ident,
        ),
        ("--secrets-store-id", &a.secrets_store_id, valid_ident),
    ];
    for (flag, val, ok) in flags {
        match val {
            Some(v) if !v.is_empty() => {
                if ok(v) {
                    cmd.push(s(flag));
                    cmd.push(v.clone());
                } else {
                    invalid.push(flag);
                }
            }
            _ => missing.push(flag),
        }
    }
    if !invalid.is_empty() {
        return out(
            Status::Failed,
            format!(
                "invalid value for {}: values must not start with '-', and ids/logins may only contain letters, digits, '.', '_', '-'; no command was run",
                invalid.join(", ")
            ),
        );
    }
    cmd.push(s("--file"));
    cmd.push(a.file.display().to_string());
    if a.public {
        cmd.push(s("--public"));
    }
    let shown = format!("cloud-ci {}", cmd.join(" "));
    ctx.checklist.push(s(
        "Each allowed org's owner must install the GitHub App from GitHub's UI (cannot be verified from here).",
    ));
    if !missing.is_empty() {
        return out(
            Status::Manual,
            format!(
                "GitHub App is not registered. Re-run with {} or run the manifest step by hand (a human must click \"Create GitHub App\" in a browser):\n{shown}",
                missing.join(", ")
            ),
        );
    }
    if !ctx.runner.env_is_set("CLOUDFLARE_API_TOKEN") {
        return out(
            Status::Manual,
            format!(
                "CLOUDFLARE_API_TOKEN is not set (the manifest step needs it to create secrets). Export it, then run:\n{shown}"
            ),
        );
    }
    if a.dry_run {
        return out(
            Status::WouldApply,
            format!("would run (opens a browser, needs a human): {shown}"),
        );
    }
    match ctx.runner.run_inherit(ctx.exe, &cmd) {
        Ok(true) => match load_doc(ctx) {
            Ok(d) if !var(&d, "GITHUB_APP_ID").is_empty() => out(
                Status::Applied,
                "GitHub App registered; GITHUB_APP_ID written to wrangler.toml",
            ),
            _ => out(
                Status::Failed,
                "the github-app command exited 0 but GITHUB_APP_ID is still empty in wrangler.toml",
            ),
        },
        Ok(false) => out(
            Status::Failed,
            format!(
                "`cloud-ci setup github-app` failed; fix the error above and re-run, or run by hand:\n{shown}"
            ),
        ),
        Err(e) => out(
            Status::Failed,
            format!("could not start `cloud-ci setup github-app`: {e}"),
        ),
    }
}

fn logins(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(s_owned)
        .collect()
}

fn s_owned(v: &str) -> String {
    v.to_string()
}

fn step_allowed_orgs(ctx: &mut Ctx<'_>) -> Outcome {
    if ctx.args.file.to_string_lossy().starts_with('-') {
        return out(
            Status::Failed,
            "--file must not start with '-'; no command was run",
        );
    }
    let doc = match load_doc(ctx) {
        Ok(d) => d,
        Err(e) => return out(Status::Failed, e),
    };
    let current = logins(&var(&doc, "GITHUB_ALLOWED_ORGS"));
    let desired = ctx
        .args
        .allowed_orgs
        .as_deref()
        .map(logins)
        .unwrap_or_default();
    if let Some(bad) = desired.iter().find(|d| !valid_ident(d)) {
        return out(
            Status::Failed,
            format!(
                "invalid org login `{bad}`: only letters, digits, '.', '_', '-' are allowed and it must not start with '-'; no command was run"
            ),
        );
    }
    if desired.is_empty() {
        if current.is_empty() {
            return out(
                Status::Manual,
                "GITHUB_ALLOWED_ORGS is empty (the Worker rejects every org). Run: cloud-ci setup allowed-orgs --add <login>   or pass --allowed-orgs.",
            );
        }
        return out(
            Status::AlreadyDone,
            format!("allowed orgs: {}", current.join(", ")),
        );
    }
    let to_add: Vec<&String> = desired.iter().filter(|d| !current.contains(d)).collect();
    if to_add.is_empty() {
        return out(
            Status::AlreadyDone,
            format!("allowed orgs already include: {}", desired.join(", ")),
        );
    }
    if ctx.args.dry_run {
        let names: Vec<&str> = to_add.iter().map(|x| x.as_str()).collect();
        return out(
            Status::WouldApply,
            format!("would add: {}", names.join(", ")),
        );
    }
    for login in &to_add {
        let cmd = vec![
            s("setup"),
            s("allowed-orgs"),
            s("--add"),
            (*login).clone(),
            s("--file"),
            ctx.args.file.display().to_string(),
        ];
        match ctx.runner.run_inherit(ctx.exe, &cmd) {
            Ok(true) => {}
            _ => {
                return out(
                    Status::Failed,
                    format!("`cloud-ci setup allowed-orgs --add {login}` failed"),
                );
            }
        }
    }
    match load_doc(ctx) {
        Ok(d) => {
            let now = logins(&var(&d, "GITHUB_ALLOWED_ORGS"));
            if desired.iter().all(|x| now.contains(x)) {
                ctx.checklist.push(s(
                    "Allowed orgs were edited in wrangler.toml only; run `wrangler deploy --config packages/cloud-ci-worker/wrangler.toml` from the repo root to apply them.",
                ));
                out(Status::Applied, format!("allowed orgs: {}", now.join(", ")))
            } else {
                out(
                    Status::Failed,
                    "allowed orgs still missing from wrangler.toml after the add",
                )
            }
        }
        Err(e) => out(Status::Failed, e),
    }
}

type StepFn = fn(&mut Ctx<'_>) -> Outcome;

const STEPS: [(&str, StepFn); 6] = [
    ("wrangler login", step_login),
    ("wrangler.toml bindings", step_bindings),
    ("D1 migrations", step_migrations),
    ("Secrets Store secrets", step_secrets),
    ("GitHub App", step_github_app),
    ("Allowed orgs", step_allowed_orgs),
];

/// Runs every step in order, stopping at the first `Failed`.
pub fn run_wizard(runner: &dyn Runner, fs: &dyn Fs, args: &WizardArgs, exe: &str) -> Report {
    let mut ctx = Ctx {
        runner,
        fs,
        args,
        exe,
        checklist: Vec::new(),
    };
    let mut report = Report::default();
    let mut stopped = false;
    for (name, step) in STEPS {
        if stopped {
            report.steps.push(StepReport {
                name,
                status: Status::NotRun,
                detail: String::new(),
            });
            continue;
        }
        let o = step(&mut ctx);
        if o.status == Status::Failed {
            stopped = true;
        }
        report.steps.push(StepReport {
            name,
            status: o.status,
            detail: o.detail,
        });
    }
    if !stopped {
        ctx.checklist.push(s(
            "Deploy if you have not since the last change: `wrangler deploy --config packages/cloud-ci-worker/wrangler.toml` from the repo root (deployed state is not checked here).",
        ));
    }
    for st in &report.steps {
        if st.status == Status::Failed {
            ctx.checklist.push(format!(
                "Fix the failed step '{}' and re-run `cloud-ci setup`.",
                st.name
            ));
        }
    }
    ctx.checklist.dedup();
    report.checklist = ctx.checklist;
    report
}

pub struct RealRunner;

impl Runner for RealRunner {
    fn run(&self, program: &str, args: &[String], cwd: Option<&Path>) -> Result<CmdOutput, String> {
        let mut c = std::process::Command::new(program);
        c.args(args);
        if let Some(d) = cwd.filter(|d| !d.as_os_str().is_empty()) {
            c.current_dir(d);
        }
        let o = c.output().map_err(|e| e.to_string())?;
        Ok(CmdOutput {
            success: o.status.success(),
            stdout: format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        })
    }

    fn run_inherit(&self, program: &str, args: &[String]) -> Result<bool, String> {
        std::process::Command::new(program)
            .args(args)
            .status()
            .map(|st| st.success())
            .map_err(|e| e.to_string())
    }

    fn env_is_set(&self, name: &str) -> bool {
        std::env::var(name).is_ok_and(|v| !v.is_empty())
    }

    fn announce(&self, msg: &str) {
        println!("{msg}");
    }
}

pub struct RealFs;

impl Fs for RealFs {
    fn read_to_string(&self, path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const SECRET_VALUE: &str = "SUPER-SECRET-PEM-VALUE";

    struct FakeFs(RefCell<String>);
    impl Fs for FakeFs {
        fn read_to_string(&self, _: &Path) -> Result<String, String> {
            Ok(self.0.borrow().clone())
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        calls: RefCell<Vec<String>>,
        /// prefix -> (success, stdout)
        replies: RefCell<HashMap<String, (bool, String)>>,
        /// Every call and announce, in order, tagged; `calls`-only
        /// consumers (`mutating`) are unaffected since this is separate.
        timeline: RefCell<Vec<String>>,
        /// Prefixes for which `run` returns `Err` instead of a reply.
        err_prefixes: RefCell<Vec<String>>,
        announcements: RefCell<Vec<String>>,
        token: bool,
    }
    impl FakeRunner {
        fn reply(&self, prefix: &str, ok: bool, stdout: &str) {
            self.replies
                .borrow_mut()
                .insert(prefix.into(), (ok, stdout.into()));
        }
        fn lookup(&self, line: &str) -> (bool, String) {
            self.replies
                .borrow()
                .iter()
                .find(|(k, _)| line.starts_with(k.as_str()))
                .map(|(_, v)| v.clone())
                .unwrap_or((true, String::new()))
        }
        fn mutating(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .filter(|c| {
                    c.contains("migrations apply")
                        || c.starts_with("cloud-ci ")
                        || c.contains("secret create")
                })
                .cloned()
                .collect()
        }
        /// After this call, `run` returns `Err` for any line starting with
        /// `prefix`, instead of a reply.
        fn err_on(&self, prefix: &str) {
            self.err_prefixes.borrow_mut().push(prefix.to_string());
        }
    }
    impl Runner for FakeRunner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            _: Option<&Path>,
        ) -> Result<CmdOutput, String> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.borrow_mut().push(line.clone());
            self.timeline.borrow_mut().push(format!("CALL: {line}"));
            if self
                .err_prefixes
                .borrow()
                .iter()
                .any(|p| line.starts_with(p.as_str()))
            {
                return Err("simulated transport error".to_string());
            }
            let (success, stdout) = self.lookup(&line);
            Ok(CmdOutput { success, stdout })
        }
        fn run_inherit(&self, program: &str, args: &[String]) -> Result<bool, String> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.borrow_mut().push(line.clone());
            self.timeline.borrow_mut().push(format!("CALL: {line}"));
            Ok(self.lookup(&line).0)
        }
        fn env_is_set(&self, _: &str) -> bool {
            self.token
        }
        fn announce(&self, msg: &str) {
            self.announcements.borrow_mut().push(msg.to_string());
            self.timeline.borrow_mut().push(format!("ANNOUNCE: {msg}"));
        }
    }

    /// Serves `wrangler secrets-store secret list` from a per-page
    /// function (keyed by the `--page` value) instead of `FakeRunner`'s
    /// single fixed reply, so pagination can be tested; every other
    /// command delegates to `base`.
    struct PagedRunner<'a> {
        base: &'a FakeRunner,
        page_result: fn(usize) -> (bool, String),
    }
    impl Runner for PagedRunner<'_> {
        fn run(&self, p: &str, a: &[String], c: Option<&Path>) -> Result<CmdOutput, String> {
            if a.first().map(String::as_str) == Some("secrets-store") {
                let page: usize = a
                    .iter()
                    .position(|x| x == "--page")
                    .and_then(|i| a.get(i + 1))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1);
                let line = format!("{p} {}", a.join(" "));
                self.base.calls.borrow_mut().push(line.clone());
                self.base
                    .timeline
                    .borrow_mut()
                    .push(format!("CALL: {line}"));
                let (success, stdout) = (self.page_result)(page);
                return Ok(CmdOutput { success, stdout });
            }
            self.base.run(p, a, c)
        }
        fn run_inherit(&self, p: &str, a: &[String]) -> Result<bool, String> {
            self.base.run_inherit(p, a)
        }
        fn env_is_set(&self, n: &str) -> bool {
            self.base.env_is_set(n)
        }
        fn announce(&self, msg: &str) {
            self.base.announce(msg)
        }
    }

    const BASE: &str = r#"
[vars]
GITHUB_APP_ID = ""
GITHUB_ALLOWED_ORGS = ""
[[d1_databases]]
binding = "DB"
database_name = "cloud-ci"
database_id = "11111111-1111-1111-1111-111111111111"
[[r2_buckets]]
binding = "ASSETS"
[[analytics_engine_datasets]]
binding = "METRICS"
[ai]
binding = "AI"
[[queues.producers]]
queue = "a"
binding = "ANALYSIS_QUEUE"
[[queues.producers]]
queue = "b"
binding = "MERGE_QUEUE"
[[durable_objects.bindings]]
name = "RUN_COORDINATOR"
[[durable_objects.bindings]]
name = "PULL_REQUEST_STATE"
[[durable_objects.bindings]]
name = "REPO_STATE"
[[durable_objects.bindings]]
name = "CONTAINER_PROBE"
[[durable_objects.bindings]]
name = "NODE_CONTAINER"
"#;

    fn configured() -> String {
        let mut t = BASE.replace("GITHUB_APP_ID = \"\"", "GITHUB_APP_ID = \"42\"");
        t = t.replace(
            "GITHUB_ALLOWED_ORGS = \"\"",
            "GITHUB_ALLOWED_ORGS = \"acme\"",
        );
        for (b, n) in [
            ("GITHUB_APP_PRIVATE_KEY", "github-app-private-key"),
            ("GITHUB_APP_CLIENT_SECRET", "github-app-client-secret"),
            ("GITHUB_WEBHOOK_SECRET", "github-webhook-secret"),
            ("CLOUD_CI_MASTER_KEY", "cloud-ci-master-key"),
        ] {
            t.push_str(&format!(
                "[[secrets_store_secrets]]\nbinding = \"{b}\"\nstore_id = \"st\"\nsecret_name = \"{n}\"\n"
            ));
        }
        t
    }

    fn args() -> WizardArgs {
        WizardArgs {
            file: PathBuf::from("w/wrangler.toml"),
            dry_run: false,
            name: None,
            allowed_orgs: None,
            deployment_url: None,
            cloudflare_account_id: None,
            secrets_store_id: None,
            public: false,
        }
    }

    fn healthy_runner() -> FakeRunner {
        let r = FakeRunner {
            token: true,
            ..Default::default()
        };
        r.reply(
            "wrangler d1 migrations list",
            true,
            "No migrations to apply!",
        );
        r.reply(
            "wrangler secrets-store secret list",
            true,
            "github-app-private-key github-app-client-secret github-webhook-secret cloud-ci-master-key",
        );
        r
    }

    #[test]
    fn rerun_on_configured_deployment_skips_everything() {
        let r = healthy_runner();
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert!(!rep.failed());
        assert!(
            rep.steps.iter().all(|s| s.status == Status::AlreadyDone),
            "{:?}",
            rep.steps
        );
        assert!(r.mutating().is_empty());
        assert!(rep.render(false).contains("skipped (already done)"));
    }

    #[test]
    fn steps_run_in_documented_order() {
        let r = healthy_runner();
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        let names: Vec<&str> = rep.steps.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            [
                "wrangler login",
                "wrangler.toml bindings",
                "D1 migrations",
                "Secrets Store secrets",
                "GitHub App",
                "Allowed orgs"
            ]
        );
        let calls = r.calls.borrow();
        assert!(calls[0].starts_with("wrangler whoami"));
        assert!(calls[1].starts_with("wrangler d1 migrations list"));
        assert!(calls[2].starts_with("wrangler secrets-store"));
    }

    #[test]
    fn failed_login_stops_and_later_steps_do_not_run() {
        let r = healthy_runner();
        r.reply("wrangler whoami", true, "You are not authenticated");
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert!(rep.failed());
        assert_eq!(rep.steps[0].status, Status::Failed);
        assert!(rep.steps[0].detail.contains("wrangler login"));
        assert!(rep.steps[1..].iter().all(|s| s.status == Status::NotRun));
        assert_eq!(r.calls.borrow().len(), 1);
    }

    #[test]
    fn placeholder_database_id_and_missing_binding_fail_clearly() {
        let r = healthy_runner();
        let ph = configured().replace("11111111-1111-1111-1111-111111111111", PLACEHOLDER_DB_ID);
        let rep = run_wizard(&r, &FakeFs(RefCell::new(ph)), &args(), "cloud-ci");
        assert_eq!(rep.steps[1].status, Status::Failed);
        assert!(rep.steps[1].detail.contains("wrangler d1 create cloud-ci"));
        assert_eq!(rep.steps[2].status, Status::NotRun);

        let no_r2 = configured().replace("binding = \"ASSETS\"", "binding = \"OTHER\"");
        let rep = run_wizard(&r, &FakeFs(RefCell::new(no_r2)), &args(), "cloud-ci");
        assert!(rep.steps[1].detail.contains("ASSETS"));
    }

    #[test]
    fn pending_migrations_are_applied_and_reverified() {
        let r = healthy_runner();
        let fs = FakeFs(RefCell::new(configured()));
        r.reply(
            "wrangler d1 migrations list",
            true,
            "Migrations to be applied:\n0021_x.sql",
        );
        // After apply the list must report clean; swap reply when apply is seen.
        struct Flip<'a>(&'a FakeRunner);
        impl Runner for Flip<'_> {
            fn run(&self, p: &str, a: &[String], c: Option<&Path>) -> Result<CmdOutput, String> {
                if a.contains(&s("apply")) {
                    self.0.reply(
                        "wrangler d1 migrations list",
                        true,
                        "No migrations to apply!",
                    );
                }
                self.0.run(p, a, c)
            }
            fn run_inherit(&self, p: &str, a: &[String]) -> Result<bool, String> {
                self.0.run_inherit(p, a)
            }
            fn env_is_set(&self, n: &str) -> bool {
                self.0.env_is_set(n)
            }
            fn announce(&self, msg: &str) {
                self.0.announce(msg)
            }
        }
        let rep = run_wizard(&Flip(&r), &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[2].status, Status::Applied);
        assert_eq!(r.mutating().len(), 1);
        // W3: the apply is announced before it runs, naming its target.
        assert!(
            r.announcements
                .borrow()
                .iter()
                .any(|m| m.contains("cloud-ci") && m.to_lowercase().contains("remote")),
            "{:?}",
            r.announcements.borrow()
        );
        // L3: the announce happens strictly before the apply call, not after.
        let timeline = r.timeline.borrow();
        let announce_at = timeline.iter().position(|t| t.starts_with("ANNOUNCE:"));
        let apply_at = timeline
            .iter()
            .position(|t| t.starts_with("CALL:") && t.contains("migrations apply"));
        let (Some(announce_idx), Some(apply_idx)) = (announce_at, apply_at) else {
            unreachable!("missing announce or apply call: {timeline:?}");
        };
        assert!(announce_idx < apply_idx, "{timeline:?}");
    }

    #[test]
    fn unrecognized_migrations_output_fails_closed_without_applying() {
        let r = healthy_runner();
        r.reply(
            "wrangler d1 migrations list",
            true,
            "some future wrangler output format this wizard does not recognize",
        );
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[2].status, Status::Failed, "{:?}", rep.steps[2]);
        assert!(rep.steps[2].detail.contains("could not determine"));
        assert!(r.mutating().is_empty());
        assert!(
            !r.calls
                .borrow()
                .iter()
                .any(|c| c.contains("migrations apply"))
        );
    }

    #[test]
    fn mentioning_sql_without_the_pending_heading_never_applies() {
        // L1: a bare ".sql" substring is not a pending marker; only the
        // real "Migrations to be applied" heading is.
        let r = healthy_runner();
        r.reply(
            "wrangler d1 migrations list",
            true,
            "warning: unrecognized output mode, see notes.sql for details",
        );
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[2].status, Status::Failed, "{:?}", rep.steps[2]);
        assert!(
            !r.calls
                .borrow()
                .iter()
                .any(|c| c.contains("migrations apply"))
        );
    }

    #[test]
    fn failed_list_exit_never_triggers_apply_even_with_pending_looking_output() {
        // Pins the `!o.success` check: pending-shaped text on a failed
        // exit must still be Unknown, never Pending.
        let r = healthy_runner();
        r.reply(
            "wrangler d1 migrations list",
            false,
            "Migrations to be applied:\n0099_new.sql",
        );
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[2].status, Status::Failed, "{:?}", rep.steps[2]);
        assert!(
            !r.calls
                .borrow()
                .iter()
                .any(|c| c.contains("migrations apply"))
        );
    }

    #[test]
    fn list_transport_error_is_unknown_not_pending() {
        // L3: pins `migrations_state`'s `Err(_) => MigState::Unknown` arm,
        // which no other test exercises (the fakes only ever return `Ok`).
        let r = healthy_runner();
        r.err_on("wrangler d1 migrations list");
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[2].status, Status::Failed, "{:?}", rep.steps[2]);
        assert!(
            !r.calls
                .borrow()
                .iter()
                .any(|c| c.contains("migrations apply"))
        );
    }

    #[test]
    fn migration_apply_failure_prints_no_raw_output() {
        // M2: the apply-failure message must never echo wrangler's own
        // output, not even behind a heuristic redaction — only the
        // database name, a non-zero-exit note, and a fixed hint.
        let r = healthy_runner();
        r.reply(
            "wrangler d1 migrations list",
            true,
            "Migrations to be applied:\n0021_x.sql",
        );
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let json_quoted = "{\"token\":\"AbCdEfGhIjKlMnOpQrStUvWxYz123456\"}";
        let trailing_comma = "token=AbCdEfGhIjKlMnOpQrStUvWxYz123456,";
        let lowercase40 = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        r.reply(
            "wrangler d1 migrations apply",
            false,
            &format!("Error: {jwt} {json_quoted} {trailing_comma} {lowercase40}"),
        );
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[2].status, Status::Failed, "{:?}", rep.steps[2]);
        let detail = &rep.steps[2].detail;
        assert!(!detail.contains(jwt), "{detail}");
        assert!(
            !detail.contains("AbCdEfGhIjKlMnOpQrStUvWxYz123456"),
            "{detail}"
        );
        assert!(!detail.contains(lowercase40), "{detail}");
        assert!(!detail.to_lowercase().contains("redacted"), "{detail}");
        assert!(detail.contains("cloud-ci"), "{detail}");
        assert!(
            detail.contains("run `wrangler d1 migrations apply cloud-ci --remote` yourself"),
            "{detail}"
        );
    }

    #[test]
    fn dry_run_changes_nothing() {
        let r = healthy_runner();
        r.reply(
            "wrangler d1 migrations list",
            true,
            "Migrations to be applied:\n0021_x.sql",
        );
        let mut a = args();
        a.dry_run = true;
        a.allowed_orgs = Some("acme,newco".into());
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &a, "cloud-ci");
        assert_eq!(rep.steps[2].status, Status::WouldApply);
        assert_eq!(rep.steps[5].status, Status::WouldApply);
        assert!(r.mutating().is_empty(), "{:?}", r.mutating());
        assert!(rep.render(true).contains("dry run"));
    }

    #[test]
    fn unregistered_app_without_args_prints_exact_manual_command() {
        let r = healthy_runner();
        let fs = FakeFs(RefCell::new(BASE.to_string()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[3].status, Status::Manual);
        let app = &rep.steps[4];
        assert_eq!(app.status, Status::Manual);
        assert!(app.detail.contains("cloud-ci setup github-app"));
        assert!(app.detail.contains("--deployment-url"));
        assert!(r.mutating().is_empty());
        assert!(!rep.failed());
        assert!(rep.render(false).contains("install the GitHub App"));
    }

    #[test]
    fn app_step_runs_real_subcommand_and_verifies_result() {
        let r = healthy_runner();
        let mut a = args();
        a.name = Some("cloud-ci (acme)".into());
        a.allowed_orgs = Some("acme".into());
        a.deployment_url = Some("https://ci.example".into());
        a.cloudflare_account_id = Some("acct".into());
        a.secrets_store_id = Some("st".into());
        // The subcommand "ran" but did not write GITHUB_APP_ID: must not claim success.
        let fs = FakeFs(RefCell::new(BASE.to_string()));
        let rep = run_wizard(&r, &fs, &a, "cloud-ci");
        assert_eq!(rep.steps[4].status, Status::Failed);
        assert!(
            r.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("cloud-ci setup github-app --name"))
        );
        assert_eq!(rep.steps[5].status, Status::NotRun);
    }

    #[test]
    fn secrets_never_appear_in_output() {
        let r = healthy_runner();
        r.reply(
            "wrangler whoami",
            true,
            &format!("logged in token {SECRET_VALUE}"),
        );
        r.reply(
            "wrangler d1 migrations list",
            true,
            &format!("No migrations to apply! {SECRET_VALUE}"),
        );
        r.reply(
            "wrangler secrets-store secret list",
            true,
            &format!("github-app-private-key {SECRET_VALUE}"),
        );
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        let text = rep.render(false);
        assert!(!text.contains(SECRET_VALUE));
        // A missing store secret fails but still names only the binding.
        assert!(rep.failed());
        assert!(!rep.steps.iter().any(|s| s.detail.contains(SECRET_VALUE)));
    }

    #[test]
    fn missing_cloudflare_token_blocks_github_app_without_invoking_it() {
        // Pins the CLOUDFLARE_API_TOKEN precheck: every flag present and
        // valid, but no token, must stop before `run_inherit` is called
        // for the github-app subcommand specifically (a later, unrelated
        // step is still free to run its own real subcommand).
        let r = FakeRunner {
            token: false,
            ..Default::default()
        };
        r.reply(
            "wrangler d1 migrations list",
            true,
            "No migrations to apply!",
        );
        let mut a = args();
        a.name = Some("cloud-ci (acme)".into());
        a.allowed_orgs = Some("acme".into());
        a.deployment_url = Some("https://ci.example".into());
        a.cloudflare_account_id = Some("acct".into());
        a.secrets_store_id = Some("st".into());
        let fs = FakeFs(RefCell::new(BASE.to_string()));
        let rep = run_wizard(&r, &fs, &a, "cloud-ci");
        assert_eq!(rep.steps[4].status, Status::Manual, "{:?}", rep.steps[4]);
        assert!(rep.steps[4].detail.contains("CLOUDFLARE_API_TOKEN"));
        assert!(
            !r.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("cloud-ci setup github-app"))
        );
    }

    /// Builds one page of `wrangler secrets-store secret list` output:
    /// one `name` per line, padded with unique filler lines up to
    /// `total_rows` so the row-count short-page check does not fire
    /// early for a test that wants a full (100-row) page.
    fn page_with(names: &[&str], total_rows: usize, filler_prefix: &str) -> String {
        let mut lines: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let mut i = 0;
        while lines.len() < total_rows {
            lines.push(format!("{filler_prefix}-{i}"));
            i += 1;
        }
        lines.join("\n")
    }

    #[test]
    fn secrets_store_pagination_reads_every_page_until_found() {
        let r = healthy_runner();
        fn two_pages(page: usize) -> (bool, String) {
            match page {
                1 => (
                    true,
                    page_with(
                        &["github-app-private-key", "github-app-client-secret"],
                        SECRETS_PER_PAGE_N,
                        "filler1",
                    ),
                ),
                _ => (
                    true,
                    page_with(
                        &["github-webhook-secret", "cloud-ci-master-key"],
                        2,
                        "filler2",
                    ),
                ),
            }
        }
        let paged = PagedRunner {
            base: &r,
            page_result: two_pages,
        };
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&paged, &fs, &args(), "cloud-ci");
        assert_eq!(
            rep.steps[3].status,
            Status::AlreadyDone,
            "{:?}",
            rep.steps[3]
        );
        let calls = r.calls.borrow().clone();
        assert!(calls.iter().any(|c| c.contains("--page 1")));
        assert!(calls.iter().any(|c| c.contains("--page 2")));
    }

    #[test]
    fn secrets_store_matching_is_exact_not_a_substring() {
        // The single-line reply is one output row, under the per-page
        // row count, so the real-wrangler short-page rule (M1) stops
        // after page 1 without needing a second, failing call.
        let r = healthy_runner();
        let toml = configured().replace(
            "secret_name = \"github-app-private-key\"",
            "secret_name = \"FOO\"",
        );
        r.reply(
            "wrangler secrets-store secret list",
            true,
            "FOO_OLD github-app-client-secret github-webhook-secret cloud-ci-master-key",
        );
        let fs = FakeFs(RefCell::new(toml));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[3].status, Status::Failed, "{:?}", rep.steps[3]);
        assert!(rep.steps[3].detail.contains("(FOO)"));
        let calls = r.calls.borrow().clone();
        let list_calls = calls.iter().filter(|c| c.contains("secrets-store")).count();
        assert_eq!(list_calls, 1, "{calls:?}");
    }

    #[test]
    fn absent_secret_on_a_full_page_followed_by_a_real_empty_page_failure_is_reported_failed() {
        // M1: wrangler 4.145.0 throws a non-zero-exit FatalError on an
        // EMPTY page ("List request returned no secrets."). A secret
        // absent from a full (100-row) first page must still surface as
        // Failed naming it, not as an unverifiable Manual step.
        let r = healthy_runner();
        fn full_then_empty_page_error(page: usize) -> (bool, String) {
            if page == 1 {
                (
                    true,
                    page_with(
                        &[
                            "github-app-private-key",
                            "github-webhook-secret",
                            "cloud-ci-master-key",
                        ],
                        SECRETS_PER_PAGE_N,
                        "filler",
                    ),
                )
            } else {
                (false, s("List request returned no secrets."))
            }
        }
        let paged = PagedRunner {
            base: &r,
            page_result: full_then_empty_page_error,
        };
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&paged, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[3].status, Status::Failed, "{:?}", rep.steps[3]);
        assert!(
            rep.steps[3].detail.contains("(github-app-client-secret)"),
            "{:?}",
            rep.steps[3]
        );
        let calls = r.calls.borrow().clone();
        let list_calls = calls.iter().filter(|c| c.contains("secrets-store")).count();
        assert_eq!(list_calls, 2, "{calls:?}");
    }

    #[test]
    fn short_first_page_with_an_absent_secret_fails_naming_it_in_one_call() {
        let r = healthy_runner();
        r.reply(
            "wrangler secrets-store secret list",
            true,
            "github-app-private-key\ngithub-app-client-secret\ngithub-webhook-secret",
        );
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[3].status, Status::Failed, "{:?}", rep.steps[3]);
        assert!(
            rep.steps[3].detail.contains("(cloud-ci-master-key)"),
            "{:?}",
            rep.steps[3]
        );
        let calls = r.calls.borrow().clone();
        let list_calls = calls.iter().filter(|c| c.contains("secrets-store")).count();
        assert_eq!(list_calls, 1, "{calls:?}");
    }

    #[test]
    fn page_one_failure_is_unverified_not_a_false_absence() {
        let r = healthy_runner();
        r.reply("wrangler secrets-store secret list", false, "");
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[3].status, Status::Manual, "{:?}", rep.steps[3]);
        assert!(!rep.failed());
    }

    #[test]
    fn full_page_then_transient_failure_is_unconfirmed_not_absent() {
        // P1: a failure on page 2 that is NOT wrangler's own empty-page
        // error text must not be read as "the listing ended, so the
        // missing name is absent" -- it may just be a transient error
        // that cut the listing short. Report "could not confirm", not
        // Failed.
        let r = healthy_runner();
        fn full_then_transient_failure(page: usize) -> (bool, String) {
            if page == 1 {
                (
                    true,
                    page_with(
                        &[
                            "github-app-private-key",
                            "github-webhook-secret",
                            "cloud-ci-master-key",
                        ],
                        SECRETS_PER_PAGE_N,
                        "filler",
                    ),
                )
            } else {
                (false, s("network error: fetch failed"))
            }
        }
        let paged = PagedRunner {
            base: &r,
            page_result: full_then_transient_failure,
        };
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&paged, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[3].status, Status::Manual, "{:?}", rep.steps[3]);
        assert!(!rep.failed(), "{:?}", rep.steps);
        assert!(
            rep.steps[3].detail.contains("could not confirm"),
            "{:?}",
            rep.steps[3]
        );
        assert!(
            !rep.steps[3].detail.contains("not in the Secrets Store"),
            "{:?}",
            rep.steps[3]
        );
        assert!(
            rep.steps[3].detail.contains("(github-app-client-secret)"),
            "{:?}",
            rep.steps[3]
        );
        let calls = r.calls.borrow().clone();
        let list_calls = calls.iter().filter(|c| c.contains("secrets-store")).count();
        assert_eq!(list_calls, 2, "{calls:?}");
    }

    #[test]
    fn transient_failure_on_page_two_with_every_wanted_name_still_on_page_two_is_unconfirmed() {
        // Reproduces the post-merge review's run h: a 120-secret store
        // where the four wanted names are on page 2, and page 2 fails
        // transiently -- none of the names were ever seen, but that must
        // not be reported as "absent", since the listing never finished.
        let r = healthy_runner();
        fn full_filler_then_transient_failure(page: usize) -> (bool, String) {
            if page == 1 {
                (true, page_with(&[], SECRETS_PER_PAGE_N, "filler"))
            } else {
                (false, s("502 Bad Gateway"))
            }
        }
        let paged = PagedRunner {
            base: &r,
            page_result: full_filler_then_transient_failure,
        };
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&paged, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[3].status, Status::Manual, "{:?}", rep.steps[3]);
        assert!(!rep.failed(), "{:?}", rep.steps);
        assert!(
            rep.steps[3].detail.contains("could not confirm"),
            "{:?}",
            rep.steps[3]
        );
        let calls = r.calls.borrow().clone();
        let list_calls = calls.iter().filter(|c| c.contains("secrets-store")).count();
        assert_eq!(
            list_calls, 2,
            "a third page must never be requested after page 2 fails: {calls:?}"
        );
    }

    #[test]
    fn runner_transport_error_on_page_two_after_a_full_page_is_also_unconfirmed() {
        // Same as the above, but page 2's failure is a true `Err` from the
        // runner (e.g. the subprocess itself could not start), not just a
        // non-zero exit -- pins the separate `Err(_) if page > 1` arm.
        let r = healthy_runner();
        struct ErrOnPage2<'a>(&'a FakeRunner);
        impl Runner for ErrOnPage2<'_> {
            fn run(&self, p: &str, a: &[String], c: Option<&Path>) -> Result<CmdOutput, String> {
                if a.first().map(String::as_str) == Some("secrets-store")
                    && a.iter().any(|x| x == "2")
                {
                    let line = format!("{p} {}", a.join(" "));
                    self.0.calls.borrow_mut().push(line.clone());
                    self.0.timeline.borrow_mut().push(format!("CALL: {line}"));
                    return Err("simulated subprocess spawn failure".to_string());
                }
                if a.first().map(String::as_str) == Some("secrets-store") {
                    let line = format!("{p} {}", a.join(" "));
                    self.0.calls.borrow_mut().push(line.clone());
                    self.0.timeline.borrow_mut().push(format!("CALL: {line}"));
                    return Ok(CmdOutput {
                        success: true,
                        stdout: page_with(&[], SECRETS_PER_PAGE_N, "filler"),
                    });
                }
                self.0.run(p, a, c)
            }
            fn run_inherit(&self, p: &str, a: &[String]) -> Result<bool, String> {
                self.0.run_inherit(p, a)
            }
            fn env_is_set(&self, n: &str) -> bool {
                self.0.env_is_set(n)
            }
            fn announce(&self, msg: &str) {
                self.0.announce(msg)
            }
        }
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&ErrOnPage2(&r), &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[3].status, Status::Manual, "{:?}", rep.steps[3]);
        assert!(!rep.failed(), "{:?}", rep.steps);
        assert!(
            rep.steps[3].detail.contains("could not confirm"),
            "{:?}",
            rep.steps[3]
        );
    }

    #[test]
    fn secrets_store_pagination_caps_and_fails_closed() {
        let r = healthy_runner();
        fn always_new(page: usize) -> (bool, String) {
            (
                true,
                page_with(&[], SECRETS_PER_PAGE_N, &format!("filler-{page}")),
            )
        }
        let paged = PagedRunner {
            base: &r,
            page_result: always_new,
        };
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&paged, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[3].status, Status::Failed, "{:?}", rep.steps[3]);
        assert!(rep.steps[3].detail.to_lowercase().contains("page"));
        let calls = r.calls.borrow().clone();
        let page_calls = calls.iter().filter(|c| c.contains("secrets-store")).count();
        assert_eq!(page_calls, SECRETS_MAX_PAGES);
    }

    #[test]
    fn invalid_org_login_is_rejected_without_running_a_command() {
        let r = healthy_runner();
        let mut a = args();
        a.allowed_orgs = Some("-not-a-login".into());
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &a, "cloud-ci");
        assert_eq!(rep.steps[5].status, Status::Failed, "{:?}", rep.steps[5]);
        assert!(rep.steps[5].detail.contains("invalid org login"));
        assert!(
            !r.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("cloud-ci setup allowed-orgs"))
        );
    }

    #[test]
    fn malformed_toml_reports_line_and_kind_not_source_text() {
        let secret_token = "LEAKED-SECRET-ABCDEF0123456789";
        let bad = format!("[vars]\nGITHUB_APP_ID = \"{secret_token}\nBROKEN\n");
        let r = healthy_runner();
        let fs = FakeFs(RefCell::new(bad));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[1].status, Status::Failed, "{:?}", rep.steps[1]);
        assert!(rep.steps[1].detail.contains("line"));
        assert!(!rep.steps[1].detail.contains(secret_token));
        assert!(!rep.steps[1].detail.contains("BROKEN"));
    }

    #[test]
    fn t1_picks_the_first_non_empty_binding_entry_not_the_first_entry() {
        // T1: a wrangler.toml with two `[[secrets_store_secrets]]` entries
        // for the same binding, where the first is empty (as a hand-edit
        // or a failed partial write might leave) and the second is the
        // real one, must use the second -- not fail on the first's empty
        // store_id.
        let r = healthy_runner();
        let mut toml = configured();
        toml = toml.replacen(
            "[[secrets_store_secrets]]\nbinding = \"GITHUB_APP_PRIVATE_KEY\"\nstore_id = \"st\"\nsecret_name = \"github-app-private-key\"\n",
            "[[secrets_store_secrets]]\nbinding = \"GITHUB_APP_PRIVATE_KEY\"\nstore_id = \"\"\nsecret_name = \"\"\n[[secrets_store_secrets]]\nbinding = \"GITHUB_APP_PRIVATE_KEY\"\nstore_id = \"st\"\nsecret_name = \"github-app-private-key\"\n",
            1,
        );
        let fs = FakeFs(RefCell::new(toml));
        let rep = run_wizard(&r, &fs, &args(), "cloud-ci");
        assert_eq!(
            rep.steps[3].status,
            Status::AlreadyDone,
            "{:?}",
            rep.steps[3]
        );
    }

    #[test]
    fn t2_allowed_orgs_rejects_a_leading_dash_file_without_running_a_command() {
        // T2: the same leading-'-' --file validation that step_github_app
        // has must also apply to step_allowed_orgs (they independently
        // forward --file to a spawned `cloud-ci` subcommand).
        let r = healthy_runner();
        let mut a = args();
        a.file = PathBuf::from("-w.toml");
        a.allowed_orgs = Some("newco".into());
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&r, &fs, &a, "cloud-ci");
        assert_eq!(rep.steps[5].status, Status::Failed, "{:?}", rep.steps[5]);
        assert!(
            rep.steps[5]
                .detail
                .contains("--file must not start with '-'")
        );
        assert!(
            !r.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("cloud-ci setup allowed-orgs"))
        );
    }

    #[test]
    fn t3_stops_reading_once_every_wanted_name_is_seen_on_a_full_page() {
        // T3: the `wanted.iter().all(|w| seen.contains(*w))` early exit
        // saves a request once every name has been found, even on a full
        // (100-row) page that would otherwise look like more pages follow.
        let r = healthy_runner();
        fn all_found_on_a_full_page_one(page: usize) -> (bool, String) {
            if page == 1 {
                (
                    true,
                    page_with(
                        &[
                            "github-app-private-key",
                            "github-app-client-secret",
                            "github-webhook-secret",
                            "cloud-ci-master-key",
                        ],
                        SECRETS_PER_PAGE_N,
                        "filler",
                    ),
                )
            } else {
                (
                    true,
                    page_with(&[], SECRETS_PER_PAGE_N, &format!("more-{page}")),
                )
            }
        }
        let paged = PagedRunner {
            base: &r,
            page_result: all_found_on_a_full_page_one,
        };
        let fs = FakeFs(RefCell::new(configured()));
        let rep = run_wizard(&paged, &fs, &args(), "cloud-ci");
        assert_eq!(
            rep.steps[3].status,
            Status::AlreadyDone,
            "{:?}",
            rep.steps[3]
        );
        let calls = r.calls.borrow().clone();
        let list_calls = calls.iter().filter(|c| c.contains("secrets-store")).count();
        assert_eq!(list_calls, 1, "{calls:?}");
    }
}
