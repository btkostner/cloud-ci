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
//! - Print secrets. Command output is never echoed, and secret values are
//!   never read; only secret *names* from `wrangler.toml` are shown.
//! - Mutate anything in `--dry-run`: only read-only commands run
//!   (`wrangler whoami`, `d1 migrations list`, `secrets-store secret list`).
//!
//! Side effects go through the [`Runner`] and [`Fs`] traits so tests use
//! fakes with no network, filesystem, or subprocesses.

use std::fmt;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item};

use crate::cli::WizardArgs;

/// Captured result of a finished command. Contents are used for
/// parsing only and are never printed.
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

fn load_doc(ctx: &Ctx<'_>) -> Result<DocumentMut, String> {
    let text = ctx
        .fs
        .read_to_string(&ctx.args.file)
        .map_err(|e| format!("cannot read {}: {e}", ctx.args.file.display()))?;
    text.parse::<DocumentMut>()
        .map_err(|e| format!("{} is not valid TOML: {e}", ctx.args.file.display()))
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

/// `Some(true)` = nothing left to apply, `Some(false)` = pending,
/// `None` = could not be determined.
fn migrations_clean(ctx: &Ctx<'_>, db: &str) -> Option<bool> {
    let cwd = migrations_cwd(ctx);
    let args = [s("d1"), s("migrations"), s("list"), s(db), s("--remote")];
    let o = ctx.runner.run("wrangler", &args, cwd.as_deref()).ok()?;
    if !o.success {
        return None;
    }
    Some(o.stdout.contains("No migrations to apply"))
}

fn step_migrations(ctx: &mut Ctx<'_>) -> Outcome {
    let db = match load_doc(ctx) {
        Ok(d) => database_name(&d),
        Err(e) => return out(Status::Failed, e),
    };
    let manual = format!(
        "wrangler d1 migrations apply {db} --remote   (run from the directory containing wrangler.toml)"
    );
    match migrations_clean(ctx, &db) {
        None => out(
            Status::Failed,
            format!(
                "could not list D1 migrations for `{db}` (is the database created and wrangler logged in?). Check with `wrangler d1 migrations list {db} --remote`."
            ),
        ),
        Some(true) => out(
            Status::AlreadyDone,
            format!("D1 `{db}` has no unapplied migrations"),
        ),
        Some(false) if ctx.args.dry_run => out(
            Status::WouldApply,
            format!("D1 `{db}` has unapplied migrations; would run: {manual}"),
        ),
        Some(false) => {
            let cwd = migrations_cwd(ctx);
            let args = [
                s("d1"),
                s("migrations"),
                s("apply"),
                db.clone(),
                s("--remote"),
            ];
            match ctx.runner.run("wrangler", &args, cwd.as_deref()) {
                Ok(o) if o.success => match migrations_clean(ctx, &db) {
                    Some(true) => out(
                        Status::Applied,
                        format!("applied D1 migrations to `{db}` and re-verified"),
                    ),
                    _ => out(
                        Status::Failed,
                        format!(
                            "apply reported success but `{db}` still lists unapplied migrations; run: {manual}"
                        ),
                    ),
                },
                _ => out(
                    Status::Failed,
                    format!("`wrangler d1 migrations apply` failed; run it by hand: {manual}"),
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
    let mut unverified: Vec<String> = Vec::new();
    let mut absent: Vec<String> = Vec::new();
    for b in SECRET_BINDINGS {
        let Some((_, store, name)) = entries.iter().find(|(eb, _, _)| eb == b) else {
            continue;
        };
        let args = [
            s("secrets-store"),
            s("secret"),
            s("list"),
            store.clone(),
            s("--remote"),
        ];
        match ctx.runner.run("wrangler", &args, None) {
            Ok(o) if o.success => {
                if !o.stdout.contains(name.as_str()) {
                    absent.push(format!("{b} ({name})"));
                }
            }
            _ => unverified.push(b.to_string()),
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
    let mut cmd = vec![s("setup"), s("github-app")];
    let mut missing: Vec<&str> = Vec::new();
    for (flag, val) in [
        ("--name", &a.name),
        ("--allowed-orgs", &a.allowed_orgs),
        ("--deployment-url", &a.deployment_url),
        ("--cloudflare-account-id", &a.cloudflare_account_id),
        ("--secrets-store-id", &a.secrets_store_id),
    ] {
        match val {
            Some(v) if !v.is_empty() => {
                cmd.push(s(flag));
                cmd.push(v.clone());
            }
            _ => missing.push(flag),
        }
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
            let (success, stdout) = self.lookup(&line);
            Ok(CmdOutput { success, stdout })
        }
        fn run_inherit(&self, program: &str, args: &[String]) -> Result<bool, String> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.borrow_mut().push(line.clone());
            Ok(self.lookup(&line).0)
        }
        fn env_is_set(&self, _: &str) -> bool {
            self.token
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
        r.reply("wrangler d1 migrations list", true, "0021_x.sql pending");
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
        }
        let rep = run_wizard(&Flip(&r), &fs, &args(), "cloud-ci");
        assert_eq!(rep.steps[2].status, Status::Applied);
        assert_eq!(r.mutating().len(), 1);
    }

    #[test]
    fn dry_run_changes_nothing() {
        let r = healthy_runner();
        r.reply("wrangler d1 migrations list", true, "0021_x.sql pending");
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
}
