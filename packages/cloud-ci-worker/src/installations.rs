//! `installation`/`installation_repositories` GitHub webhook handling
//! (docs/design/auth.md § "Multiple orgs and installations", "Data
//! model"): discovers new App installations, gates them against
//! `GITHUB_ALLOWED_ORGS`, and keeps the `installations`/`repos` D1 tables
//! (`migrations/0003_installations_and_repos.sql`) current.
//!
//! Same layering as `github_app.rs`/`coordinator/mod.rs`: payload parsing
//! and the allowlist decision are pure, unit-tested with plain
//! `cargo test`; the D1 read/write functions below need the Workers
//! runtime and are only exercised by the live smoke test
//! (`mise run //packages/cloud-ci-worker:dev`), same as
//! `coordinator::mod`'s `project_*_to_d1` functions.
//!
//! `lib.rs`'s `/webhooks/github` route is the only caller: it verifies
//! `webhook::verify_signature` first, then parses the raw body into
//! [`InstallationEvent`]/[`InstallationRepositoriesEvent`] and dispatches
//! here based on `action`.

use serde::Deserialize;
use worker::Env;
use worker::wasm_bindgen::JsValue;

/// `installation.account` per
/// docs.github.com/en/webhooks/webhook-events-and-payloads#installation
/// (accessed 2026-10-02). GitHub's payload has many more fields
/// (`node_id`, `avatar_url`, ...); `id`/`login`/`type` are needed here —
/// `id` is the numeric account id docs/design/byo-ci.md § Auth requires
/// matching a `repository_owner_id` OIDC claim against (logins are
/// mutable across renames/transfers; the numeric id is not) — and `serde`
/// ignores the rest.
#[derive(Debug, Clone, Deserialize)]
pub struct InstallationAccount {
    pub id: u64,
    pub login: String,
    #[serde(rename = "type")]
    pub account_type: String,
}

/// `installation` per the same doc: only the fields this module's
/// allowlist/upsert logic needs.
#[derive(Debug, Clone, Deserialize)]
pub struct InstallationPayload {
    pub id: u64,
    pub account: InstallationAccount,
}

/// Top-level `installation` event body. `action` is one of
/// `created`/`deleted`/`suspend`/`unsuspend`, plus several this module
/// does not act on (`new_permissions_accepted`, etc.) — those are simply
/// no-ops here, not errors, since the manifest subscribes to the whole
/// `installation` event family and GitHub may add actions over time.
#[derive(Debug, Clone, Deserialize)]
pub struct InstallationEvent {
    pub action: String,
    pub installation: InstallationPayload,
}

/// One entry of `repositories`/`repositories_added`/`repositories_removed`
/// per docs.github.com/en/webhooks/webhook-events-and-payloads#installation_repositories
/// (accessed 2026-10-02) — `id`/`name` only; `full_name`/`private` etc.
/// are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoRef {
    pub id: u64,
    pub name: String,
}

/// Top-level `installation_repositories` event body. `action` is
/// `added` or `removed`.
#[derive(Debug, Clone, Deserialize)]
pub struct InstallationRepositoriesEvent {
    pub action: String,
    pub installation: InstallationPayload,
    #[serde(default)]
    pub repositories_added: Vec<RepoRef>,
    #[serde(default)]
    pub repositories_removed: Vec<RepoRef>,
}

/// Checks `login` against the comma-separated `GITHUB_ALLOWED_ORGS` var.
///
/// Case-insensitive: GitHub account logins are case-insensitive (you
/// cannot register `Acme-Corp` and `acme-corp` as two different accounts),
/// so comparing case-sensitively would let an operator's allowlist entry
/// silently fail to match a differently-cased delivery for the same
/// account — a correctness footgun with no corresponding security benefit,
/// since case can never be the only thing distinguishing two real GitHub
/// accounts.
pub fn is_allowed_org(login: &str, allowed_orgs: &str) -> bool {
    allowed_orgs
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|org| org.eq_ignore_ascii_case(login))
}

// ---------------------------------------------------------------------------
// D1 reads/writes — needs the Workers runtime, not covered by `cargo test`
// (see module docs).
// ---------------------------------------------------------------------------

/// `installation.created` for an allowlisted account: upserts the
/// `installations` row. Re-delivery of the same `created` event (at-least-
/// once webhook delivery) is idempotent via `ON CONFLICT`.
pub async fn upsert_installation(
    env: &Env,
    installation_id: u64,
    account_login: &str,
    account_type: &str,
    account_id: u64,
    installed_at_s: i64,
) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare(
        "INSERT INTO installations (installation_id, account_login, account_type, account_id, suspended_at, installed_at) \
         VALUES (?1, ?2, ?3, ?4, NULL, ?5) \
         ON CONFLICT (installation_id) DO UPDATE SET \
             account_login = excluded.account_login, \
             account_type = excluded.account_type, \
             account_id = excluded.account_id",
    )
    .bind(&[
        JsValue::from_f64(installation_id as f64),
        JsValue::from_str(account_login),
        JsValue::from_str(account_type),
        JsValue::from_f64(account_id as f64),
        JsValue::from_f64(installed_at_s as f64),
    ])?
    .run()
    .await?;
    Ok(())
}

/// `installation.deleted`: removes the installation and every repo it
/// owned. D1 (SQLite) does not enforce `FOREIGN KEY` constraints by
/// default, so this explicitly deletes `repos` first rather than relying
/// on cascading behavior that isn't actually enabled.
pub async fn delete_installation_row(env: &Env, installation_id: u64) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare("DELETE FROM repos WHERE installation_id = ?1")
        .bind(&[JsValue::from_f64(installation_id as f64)])?
        .run()
        .await?;
    db.prepare("DELETE FROM installations WHERE installation_id = ?1")
        .bind(&[JsValue::from_f64(installation_id as f64)])?
        .run()
        .await?;
    Ok(())
}

/// `installation.suspend`/`unsuspend`: sets or clears `suspended_at`.
/// `suspended_at_s = None` clears it (unsuspend).
pub async fn set_suspended(
    env: &Env,
    installation_id: u64,
    suspended_at_s: Option<i64>,
) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare("UPDATE installations SET suspended_at = ?1 WHERE installation_id = ?2")
        .bind(&[
            suspended_at_s.map_or(JsValue::NULL, |s| JsValue::from_f64(s as f64)),
            JsValue::from_f64(installation_id as f64),
        ])?
        .run()
        .await?;
    Ok(())
}

/// `installation_repositories.added`: upserts one `repos` row. Idempotent
/// for redelivery.
pub async fn upsert_repo(
    env: &Env,
    repo_id: u64,
    installation_id: u64,
    name: &str,
) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare(
        "INSERT INTO repos (repo_id, installation_id, name) VALUES (?1, ?2, ?3) \
         ON CONFLICT (repo_id) DO UPDATE SET \
             installation_id = excluded.installation_id, \
             name = excluded.name",
    )
    .bind(&[
        JsValue::from_f64(repo_id as f64),
        JsValue::from_f64(installation_id as f64),
        JsValue::from_str(name),
    ])?
    .run()
    .await?;
    Ok(())
}

/// `installation_repositories.removed`: removes one `repos` row.
pub async fn delete_repo(env: &Env, repo_id: u64) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare("DELETE FROM repos WHERE repo_id = ?1")
        .bind(&[JsValue::from_f64(repo_id as f64)])?
        .run()
        .await?;
    Ok(())
}

/// Every `installation_id` currently in the local `installations` table —
/// `reconcile::run`'s "our current local list" half of the diff
/// (docs/design/auth.md's "Multiple orgs and installations" "Discovery"
/// paragraph). Row values come back as `f64` over the D1/JS boundary
/// (same reasoning as [`lookup_repo_installation`]'s manual
/// `serde_json::Value` extraction), so this reads each row as a bare
/// number rather than a typed struct.
pub async fn list_installation_ids(env: &Env) -> worker::Result<Vec<u64>> {
    let db = env.d1("DB")?;
    let rows: Vec<serde_json::Value> = db
        .prepare("SELECT installation_id FROM installations")
        .all()
        .await?
        .results()?;
    rows.into_iter()
        .map(|row| {
            row.get("installation_id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| {
                    worker::Error::RustError("installations row missing installation_id".into())
                })
        })
        .collect()
}

/// The `repos`/`installations` join row `BeginRun`'s OIDC path needs to
/// decide whether a claimed `repository_id` belongs to an allowlisted,
/// non-suspended installation (docs/design/byo-ci.md § Auth).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoInstallationRow {
    pub account_id: u64,
    pub suspended_at: Option<i64>,
}

/// Looks up the `installations` row that owns `repo_id`, via the
/// `repos` -> `installations` join on `installation_id`. `None` means no
/// `repos` row exists for this id — distinct from a row existing but
/// failing the allowlist check, which [`AllowlistError::OwnerMismatch`]/
/// [`AllowlistError::Suspended`] cover.
pub async fn lookup_repo_installation(
    env: &Env,
    repo_id: u64,
) -> worker::Result<Option<RepoInstallationRow>> {
    let db = env.d1("DB")?;
    let row = db
        .prepare(
            "SELECT installations.account_id, installations.suspended_at \
             FROM repos JOIN installations \
                 ON repos.installation_id = installations.installation_id \
             WHERE repos.repo_id = ?1",
        )
        .bind(&[JsValue::from_f64(repo_id as f64)])?
        .first::<serde_json::Value>(None)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let account_id = row
        .get("account_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| {
            worker::Error::RustError("repos/installations row missing account_id".into())
        })?;
    let suspended_at = row.get("suspended_at").and_then(|v| v.as_i64());
    Ok(Some(RepoInstallationRow {
        account_id,
        suspended_at,
    }))
}

/// Why an OIDC-claimed repository failed the allowlist check
/// (docs/design/byo-ci.md § Auth) — the caller (`lib.rs::handle_begin_run`)
/// maps every variant to a rejected `BeginRun` call; none of them ever
/// fall through to success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowlistError {
    /// No `repos` row exists for the claimed `repository_id` — either the
    /// repo was never discovered via the `installation_repositories`
    /// webhook, or the App was uninstalled/the repo was removed since.
    RepoNotFound,
    /// A `repos` row exists, but its owning installation's numeric
    /// `account_id` does not match the claim's `repository_owner_id` —
    /// the exact renames/transfers case docs/design/byo-ci.md § Auth
    /// calls out matching on the name string would get wrong.
    OwnerMismatch,
    /// The owning installation exists and the owner matches, but it is
    /// currently suspended (`installations.suspended_at IS NOT NULL`).
    Suspended,
}

/// Pure "does this OIDC claim match an allowlisted, non-suspended
/// installation" decision (docs/design/byo-ci.md § Auth), given the
/// `repos`/`installations` row already fetched by
/// [`lookup_repo_installation`] for the claim's `repository_id`. Kept
/// separate from the D1 fetch so it is unit-testable with plain
/// `cargo test`, same layering as [`is_allowed_org`].
pub fn check_allowlist(
    repository_owner_id: u64,
    row: Option<RepoInstallationRow>,
) -> Result<(), AllowlistError> {
    let row = row.ok_or(AllowlistError::RepoNotFound)?;
    if row.account_id != repository_owner_id {
        return Err(AllowlistError::OwnerMismatch);
    }
    if row.suspended_at.is_some() {
        return Err(AllowlistError::Suspended);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_exact_match() {
        assert!(is_allowed_org("acme-corp", "acme-corp,acme-labs"));
        assert!(is_allowed_org("acme-labs", "acme-corp,acme-labs"));
    }

    #[test]
    fn rejects_login_not_in_list() {
        assert!(!is_allowed_org("evil-corp", "acme-corp,acme-labs"));
    }

    #[test]
    fn rejects_everything_when_allowlist_is_empty() {
        assert!(!is_allowed_org("acme-corp", ""));
    }

    #[test]
    fn match_is_case_insensitive() {
        assert!(is_allowed_org("Acme-Corp", "acme-corp"));
        assert!(is_allowed_org("acme-corp", "ACME-CORP"));
    }

    #[test]
    fn ignores_surrounding_whitespace_in_allowlist_entries() {
        assert!(is_allowed_org("acme-corp", " acme-corp , acme-labs "));
    }

    #[test]
    fn empty_entries_from_trailing_commas_never_match_an_empty_login() {
        assert!(!is_allowed_org("", "acme-corp,"));
    }

    #[test]
    fn installation_event_parses_documented_created_shape() -> Result<(), serde_json::Error> {
        // Trimmed from docs.github.com/en/webhooks/webhook-events-and-payloads
        // #installation (accessed 2026-10-02) — real payloads carry many
        // more fields (permissions, events, repository_selection, sender,
        // ...) which must be ignored, not rejected.
        let body = r#"{
            "action": "created",
            "installation": {
                "id": 12345,
                "account": { "login": "acme-corp", "id": 1, "type": "Organization" },
                "repository_selection": "all",
                "permissions": { "contents": "read" },
                "events": ["push"]
            },
            "sender": { "login": "octocat", "id": 2 }
        }"#;
        let event: InstallationEvent = serde_json::from_str(body)?;
        assert_eq!(event.action, "created");
        assert_eq!(event.installation.id, 12345);
        assert_eq!(event.installation.account.login, "acme-corp");
        assert_eq!(event.installation.account.account_type, "Organization");
        Ok(())
    }

    #[test]
    fn installation_event_parses_suspend_shape() -> Result<(), serde_json::Error> {
        let body = r#"{
            "action": "suspend",
            "installation": {
                "id": 999,
                "account": { "login": "acme-corp", "id": 1, "type": "Organization" }
            }
        }"#;
        let event: InstallationEvent = serde_json::from_str(body)?;
        assert_eq!(event.action, "suspend");
        Ok(())
    }

    #[test]
    fn installation_repositories_event_parses_documented_added_shape()
    -> Result<(), serde_json::Error> {
        // Trimmed from docs.github.com/en/webhooks/webhook-events-and-payloads
        // #installation_repositories (accessed 2026-10-02).
        let body = r#"{
            "action": "added",
            "installation": {
                "id": 12345,
                "account": { "login": "acme-corp", "id": 1, "type": "Organization" }
            },
            "repository_selection": "selected",
            "repositories_added": [
                { "id": 1, "name": "widgets", "full_name": "acme-corp/widgets", "private": false }
            ],
            "repositories_removed": []
        }"#;
        let event: InstallationRepositoriesEvent = serde_json::from_str(body)?;
        assert_eq!(event.action, "added");
        assert_eq!(event.installation.id, 12345);
        assert_eq!(event.repositories_added.len(), 1);
        assert_eq!(event.repositories_added[0].id, 1);
        assert_eq!(event.repositories_added[0].name, "widgets");
        assert!(event.repositories_removed.is_empty());
        Ok(())
    }

    #[test]
    fn installation_repositories_event_parses_removed_shape() -> Result<(), serde_json::Error> {
        let body = r#"{
            "action": "removed",
            "installation": {
                "id": 12345,
                "account": { "login": "acme-corp", "id": 1, "type": "Organization" }
            },
            "repositories_added": [],
            "repositories_removed": [
                { "id": 2, "name": "gadgets", "full_name": "acme-corp/gadgets", "private": true }
            ]
        }"#;
        let event: InstallationRepositoriesEvent = serde_json::from_str(body)?;
        assert_eq!(event.action, "removed");
        assert_eq!(event.repositories_removed.len(), 1);
        assert_eq!(event.repositories_removed[0].id, 2);
        assert!(event.repositories_added.is_empty());
        Ok(())
    }

    #[test]
    fn check_allowlist_rejects_missing_repo_row() {
        assert_eq!(
            check_allowlist(67890, None),
            Err(AllowlistError::RepoNotFound)
        );
    }

    #[test]
    fn check_allowlist_rejects_owner_mismatch() {
        let row = RepoInstallationRow {
            account_id: 11111,
            suspended_at: None,
        };
        assert_eq!(
            check_allowlist(67890, Some(row)),
            Err(AllowlistError::OwnerMismatch)
        );
    }

    #[test]
    fn check_allowlist_rejects_suspended_installation() {
        let row = RepoInstallationRow {
            account_id: 67890,
            suspended_at: Some(1_700_000_000),
        };
        assert_eq!(
            check_allowlist(67890, Some(row)),
            Err(AllowlistError::Suspended)
        );
    }

    #[test]
    fn check_allowlist_accepts_matching_non_suspended_installation() {
        let row = RepoInstallationRow {
            account_id: 67890,
            suspended_at: None,
        };
        assert_eq!(check_allowlist(67890, Some(row)), Ok(()));
    }
}
