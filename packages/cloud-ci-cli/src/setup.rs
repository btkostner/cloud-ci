//! Orchestrates `cloud-ci setup allowed-orgs --add <login> | --remove
//! <login>`, per `docs/design/auth.md`'s "Org allowlist changes"
//! paragraph and `crate::cli::AllowedOrgsArgs`' doc comment (this
//! command's own `--help` text) for the `--deploy`-is-opt-in reasoning.
//!
//! # TOML crate choice: `toml_edit`, not `toml`
//!
//! Neither crate was already a dependency anywhere in the workspace (the
//! `taplo` mentioned in AGENTS.md is a standalone lint CLI, not a Rust
//! library dependency). `toml_edit` is a format-preserving parser: editing
//! one key's value round-trips every other key, comment, and blank line in
//! `wrangler.toml` untouched. The plain `toml` crate re-serializes the
//! whole document from its `serde` model, which would lose the extensive
//! comments this file carries (see `packages/cloud-ci-worker/wrangler.toml`)
//! and could reorder tables. Since this command's whole job is "mutate one
//! value, leave everything else alone" (the task's own requirement),
//! `toml_edit` is the only defensible choice.
//!
//! # Pure logic lives here and is exhaustively unit-tested
//!
//! `mutate_allowed_orgs` and `diff_allowed_orgs` below take and return
//! plain strings/documents — no filesystem, no network, no `wrangler`
//! invocation — so every add/remove/dedup/round-trip case is covered by
//! `cargo test` with in-memory fixtures. The only I/O this module performs
//! beyond that is reading/writing `--file` and, only with `--deploy`,
//! spawning `wrangler deploy --config <file>`. That `wrangler deploy` spawn
//! is **not** exercised by any test here — it is a real, mutating call
//! against a real Cloudflare account, the same category of deploy-time
//! operation this codebase documents it cannot safely run in a test
//! environment (see `packages/cloud-ci-worker/wrangler.toml`'s own
//! "Placeholders until ... `cloud-ci setup` ... writes the real values"
//! comments, which describe exactly this gap).

use std::fmt;
use std::fs;
use std::process::Command as ProcessCommand;

use toml_edit::DocumentMut;

use crate::cli::AllowedOrgsArgs;

#[derive(Debug)]
pub struct SetupError {
    message: String,
}

impl SetupError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SetupError {}

/// One requested change to the `GITHUB_ALLOWED_ORGS` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op<'a> {
    Add(&'a str),
    Remove(&'a str),
}

/// Parses a comma-separated `GITHUB_ALLOWED_ORGS` value into its login
/// list. Empty entries (from a bare `""`, a trailing comma, or repeated
/// commas) are dropped — the stored value is never meant to carry blanks.
fn parse_logins(value: &str) -> Vec<&str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .collect()
}

/// Applies `op` to `logins`, in place, per the task's documented
/// behavior:
///
/// - **Add a login already present**: no-op, no duplicate appended.
/// - **Add a new login**: appended to the end.
/// - **Remove a login present**: removed.
/// - **Remove a login not present**: no-op.
///
/// Returns `true` if `logins` actually changed.
fn apply_op(logins: &mut Vec<String>, op: Op<'_>) -> bool {
    match op {
        Op::Add(login) => {
            if logins.iter().any(|existing| existing == login) {
                false
            } else {
                logins.push(login.to_string());
                true
            }
        }
        Op::Remove(login) => {
            let before = logins.len();
            logins.retain(|existing| existing != login);
            logins.len() != before
        }
    }
}

/// Serializes `logins` back into `GITHUB_ALLOWED_ORGS`'s comma-separated
/// string form. An empty list serializes to `""` (empty string), not a
/// removed key: the key's presence (always present, defaulting to
/// disallow-everything when empty) is the fail-closed contract documented
/// on `packages/cloud-ci-worker/wrangler.toml`'s `GITHUB_ALLOWED_ORGS = ""`
/// placeholder ("the webhook route treats a missing/empty
/// GITHUB_ALLOWED_ORGS as 'no org allowed'") — removing the key entirely
/// would also fail closed, but only by relying on a Worker-side default
/// rather than the authoritative, explicit value written at deploy time,
/// and removing a key every other value in the file keeps present is a
/// visibly bigger diff than writing `""`.
fn serialize_logins(logins: &[String]) -> String {
    logins.join(",")
}

/// Reads `GITHUB_ALLOWED_ORGS` out of `doc`'s `[vars]` table, applies
/// `op`, and writes the result back into the same table. Returns
/// `(old_value, new_value)`; `old_value == new_value` means the file was
/// already in the desired state. Missing `[vars]` or `GITHUB_ALLOWED_ORGS`
/// is treated as an empty string (zero logins), matching the Worker's own
/// fail-closed default for an absent value — `--add` against a missing
/// key simply creates it.
fn mutate_allowed_orgs(doc: &mut DocumentMut, op: Op<'_>) -> Result<(String, String), SetupError> {
    let vars = doc["vars"]
        .or_insert(toml_edit::table())
        .as_table_mut()
        .ok_or_else(|| SetupError::new("`vars` in wrangler.toml is not a table"))?;

    let old_value = vars
        .get("GITHUB_ALLOWED_ORGS")
        .and_then(|item| item.as_str())
        .unwrap_or("")
        .to_string();

    let mut logins: Vec<String> = parse_logins(&old_value)
        .into_iter()
        .map(str::to_string)
        .collect();
    apply_op(&mut logins, op);
    let new_value = serialize_logins(&logins);

    let item = &mut vars["GITHUB_ALLOWED_ORGS"];
    let old_decor = item.as_value().map(|value| value.decor().clone());
    *item = toml_edit::value(new_value.clone());
    if let (Some(decor), Some(value)) = (old_decor, item.as_value_mut()) {
        *value.decor_mut() = decor;
    }

    Ok((old_value, new_value))
}

/// Reads `args.file`, applies `--add`/`--remove`, writes the file back
/// (preserving every other key/comment/formatting via `toml_edit`'s
/// format-preserving round-trip), prints the old/new `GITHUB_ALLOWED_ORGS`
/// diff, and — only when `args.deploy` is set — runs
/// `wrangler deploy --config <file>`. See this module's doc comment for
/// why the deploy step is opt-in and untested here.
pub fn run_allowed_orgs(args: &AllowedOrgsArgs) -> Result<(), SetupError> {
    let op = match (&args.add, &args.remove) {
        (Some(login), None) => Op::Add(login),
        (None, Some(login)) => Op::Remove(login),
        // clap's `required(true)` ArgGroup on `add`/`remove` (see
        // `AllowedOrgsArgs`) makes both-set and neither-set unreachable.
        _ => {
            return Err(SetupError::new(
                "exactly one of --add or --remove is required",
            ));
        }
    };

    let text = fs::read_to_string(&args.file)
        .map_err(|err| SetupError::new(format!("could not read {}: {err}", args.file.display())))?;
    let mut doc: DocumentMut = text.parse().map_err(|err| {
        SetupError::new(format!("could not parse {}: {err}", args.file.display()))
    })?;

    let (old_value, new_value) = mutate_allowed_orgs(&mut doc, op)?;

    if old_value == new_value {
        match op {
            Op::Add(login) => println!(
                "{}: `{login}` is already in GITHUB_ALLOWED_ORGS (\"{old_value}\") — no change",
                args.file.display()
            ),
            Op::Remove(login) => println!(
                "{}: `{login}` is not in GITHUB_ALLOWED_ORGS (\"{old_value}\") — no change",
                args.file.display()
            ),
        }
        return Ok(());
    }

    fs::write(&args.file, doc.to_string()).map_err(|err| {
        SetupError::new(format!("could not write {}: {err}", args.file.display()))
    })?;

    println!(
        "{}: GITHUB_ALLOWED_ORGS \"{old_value}\" -> \"{new_value}\"",
        args.file.display()
    );

    if args.deploy {
        println!("running: wrangler deploy --config {}", args.file.display());
        let status = ProcessCommand::new("wrangler")
            .arg("deploy")
            .arg("--config")
            .arg(&args.file)
            .status()
            .map_err(|err| SetupError::new(format!("could not run wrangler deploy: {err}")))?;
        if !status.success() {
            return Err(SetupError::new(format!(
                "wrangler deploy exited with {status}"
            )));
        }
    } else {
        println!(
            "run `wrangler deploy --config {}` to apply this change (or re-run with --deploy)",
            args.file.display()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command, SetupCommand};

    fn scratch_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cloud-ci-cli-setup-test-{label}-{}",
            std::process::id()
        ))
    }

    fn parse(argv: &[&str]) -> AllowedOrgsArgs {
        let cli = Cli::parse_from(argv);
        let Command::Setup(setup_args) = cli.command else {
            unreachable!("expected Command::Setup");
        };
        let Some(SetupCommand::AllowedOrgs(args)) = setup_args.command else {
            unreachable!("expected SetupCommand::AllowedOrgs");
        };
        args
    }

    #[test]
    fn defaults_file_to_worker_wrangler_toml() {
        let args = parse(&["cloud-ci", "setup", "allowed-orgs", "--add", "acme-corp"]);
        assert_eq!(
            args.file.to_string_lossy(),
            "packages/cloud-ci-worker/wrangler.toml"
        );
        assert!(!args.deploy);
    }

    #[test]
    fn add_to_empty_list() {
        let mut logins: Vec<String> = Vec::new();
        assert!(apply_op(&mut logins, Op::Add("acme-corp")));
        assert_eq!(logins, vec!["acme-corp".to_string()]);
    }

    #[test]
    fn add_duplicate_is_a_noop() {
        let mut logins = vec!["acme-corp".to_string()];
        assert!(!apply_op(&mut logins, Op::Add("acme-corp")));
        assert_eq!(logins, vec!["acme-corp".to_string()]);
    }

    #[test]
    fn add_to_existing_list_appends() {
        let mut logins = vec!["acme-corp".to_string()];
        assert!(apply_op(&mut logins, Op::Add("acme-labs")));
        assert_eq!(
            logins,
            vec!["acme-corp".to_string(), "acme-labs".to_string()]
        );
    }

    #[test]
    fn remove_present_login() {
        let mut logins = vec!["acme-corp".to_string(), "acme-labs".to_string()];
        assert!(apply_op(&mut logins, Op::Remove("acme-corp")));
        assert_eq!(logins, vec!["acme-labs".to_string()]);
    }

    #[test]
    fn remove_absent_login_is_a_noop() {
        let mut logins = vec!["acme-corp".to_string()];
        assert!(!apply_op(&mut logins, Op::Remove("acme-labs")));
        assert_eq!(logins, vec!["acme-corp".to_string()]);
    }

    #[test]
    fn remove_down_to_empty_list_serializes_to_empty_string() {
        let mut logins = vec!["acme-corp".to_string()];
        assert!(apply_op(&mut logins, Op::Remove("acme-corp")));
        assert_eq!(serialize_logins(&logins), "");
    }

    #[test]
    fn mutate_allowed_orgs_adds_to_missing_vars_table() {
        let mut doc: DocumentMut = "name = \"cloud-ci\"\n".parse().unwrap_or_default();
        let (old, new) = mutate_allowed_orgs(&mut doc, Op::Add("acme-corp"))
            .unwrap_or_else(|err| unreachable!("expected Ok, got {err}"));
        assert_eq!(old, "");
        assert_eq!(new, "acme-corp");
        assert_eq!(
            doc["vars"]["GITHUB_ALLOWED_ORGS"].as_str(),
            Some("acme-corp")
        );
    }

    const FIXTURE: &str = r#"name = "cloud-ci"
main = "build/index.js"
compatibility_date = "2026-09-01"

# Not secret: public identifiers / an allowlist, not a credential.
[vars]
GITHUB_APP_ID = "123456"
GITHUB_APP_CLIENT_ID = "Iv1.8a61f9b3a7aba766"
GITHUB_ALLOWED_ORGS = "acme-corp,acme-labs"            # deployed allowlist, see below
CLOUD_CI_INGEST_AUDIENCE = ""

[build]
command = "worker-build --release"
cwd = "packages/cloud-ci-worker"
"#;

    fn write_fixture(dir: &std::path::Path) -> std::path::PathBuf {
        let _ = fs::create_dir_all(dir);
        let file = dir.join("wrangler.toml");
        let _ = fs::write(&file, FIXTURE);
        file
    }

    #[test]
    fn round_trip_preserves_unrelated_content_on_add() {
        let tmp = scratch_dir("roundtrip-add");
        let file = write_fixture(&tmp);

        let args = AllowedOrgsArgs {
            add: Some("acme-widgets".to_string()),
            remove: None,
            file: file.clone(),
            deploy: false,
        };
        let result = run_allowed_orgs(&args);
        let written = fs::read_to_string(&file).unwrap_or_default();
        let _ = fs::remove_dir_all(&tmp);

        if let Err(err) = result {
            unreachable!("expected Ok, got {err}");
        }
        assert!(written.contains("GITHUB_APP_ID = \"123456\""));
        assert!(written.contains(
            "GITHUB_ALLOWED_ORGS = \"acme-corp,acme-labs,acme-widgets\"            # deployed allowlist, see below"
        ));
        assert!(
            written.contains("# Not secret: public identifiers / an allowlist, not a credential.")
        );
        assert!(written.contains("cwd = \"packages/cloud-ci-worker\""));
    }

    #[test]
    fn add_existing_login_prints_no_change_and_does_not_rewrite_file() {
        let tmp = scratch_dir("add-existing");
        let file = write_fixture(&tmp);
        let before = fs::read_to_string(&file).unwrap_or_default();

        let args = AllowedOrgsArgs {
            add: Some("acme-corp".to_string()),
            remove: None,
            file: file.clone(),
            deploy: false,
        };
        let result = run_allowed_orgs(&args);
        let after = fs::read_to_string(&file).unwrap_or_default();
        let _ = fs::remove_dir_all(&tmp);

        if let Err(err) = result {
            unreachable!("expected Ok, got {err}");
        }
        assert_eq!(before, after);
    }

    #[test]
    fn remove_absent_login_prints_no_change_and_does_not_rewrite_file() {
        let tmp = scratch_dir("remove-absent");
        let file = write_fixture(&tmp);
        let before = fs::read_to_string(&file).unwrap_or_default();

        let args = AllowedOrgsArgs {
            add: None,
            remove: Some("acme-ghost".to_string()),
            file: file.clone(),
            deploy: false,
        };
        let result = run_allowed_orgs(&args);
        let after = fs::read_to_string(&file).unwrap_or_default();
        let _ = fs::remove_dir_all(&tmp);

        if let Err(err) = result {
            unreachable!("expected Ok, got {err}");
        }
        assert_eq!(before, after);
    }

    #[test]
    fn remove_down_to_empty_writes_empty_string_value() {
        let tmp = scratch_dir("remove-all");
        let file = write_fixture(&tmp);

        for login in ["acme-corp", "acme-labs"] {
            let args = AllowedOrgsArgs {
                add: None,
                remove: Some(login.to_string()),
                file: file.clone(),
                deploy: false,
            };
            if let Err(err) = run_allowed_orgs(&args) {
                let _ = fs::remove_dir_all(&tmp);
                unreachable!("expected Ok removing {login}, got {err}");
            }
        }

        let written = fs::read_to_string(&file).unwrap_or_default();
        let _ = fs::remove_dir_all(&tmp);
        assert!(written.contains("GITHUB_ALLOWED_ORGS = \"\""));
        assert!(written.contains("GITHUB_APP_ID = \"123456\""));
    }

    #[test]
    fn missing_file_is_a_run_error() {
        let tmp = scratch_dir("missing");
        let args = AllowedOrgsArgs {
            add: Some("acme-corp".to_string()),
            remove: None,
            file: tmp.join("does-not-exist.toml"),
            deploy: false,
        };
        match run_allowed_orgs(&args) {
            Err(err) => assert!(err.to_string().contains("could not read")),
            Ok(()) => unreachable!("expected a read error for a missing file"),
        }
    }
}
