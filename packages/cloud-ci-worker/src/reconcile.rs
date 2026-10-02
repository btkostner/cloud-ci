//! Periodic GitHub App installation reconcile job
//! (docs/design/auth.md § "Multiple orgs and installations", "Discovery"
//! paragraph: "a periodic reconcile job calls `GET /app/installations`
//! (paginated, App JWT auth) and diffs against the `installations` table,
//! catching any installation whose webhook was missed or whose delivery
//! arrived while the Worker was deploying"; "Failure modes" table's
//! `installation.created` and allowlist-removal rows). Fills three gaps a
//! webhook delivery alone cannot guarantee:
//!
//! 1. an allowlisted installation GitHub has but the local `installations`
//!    table doesn't (webhook missed, or arrived mid-deploy) — upserted.
//! 2. an installation GitHub has whose account is not (or no longer) in
//!    `GITHUB_ALLOWED_ORGS` (a missed `installation.created` uninstall
//!    call, or the operator removed the org from the allowlist after it
//!    was installed) — uninstalled, same as the webhook path.
//! 3. a local row for an installation GitHub's list no longer contains
//!    (uninstalled directly in GitHub's UI, `installation.deleted`
//!    webhook missed) — deleted, repos included.
//!
//! Same layering as `github_app.rs`/`installations.rs`: [`diff`] and
//! [`parse_installed_at`] are pure, unit-tested with plain `cargo test`;
//! [`run`] mints the App JWT, makes the paginated GitHub call, and reads/
//! writes D1 — it needs the Workers runtime and is only exercised by the
//! `wrangler dev --test-scheduled` smoke run, not `cargo test`. `lib.rs`'s
//! `#[event(scheduled)]` handler is the only caller.

use crate::github_app;
use crate::installations;
use worker::Env;

/// One GitHub-reported installation, trimmed to what [`diff`] needs.
/// Decoupled from [`github_app::ListedInstallation`] (GitHub's wire
/// shape, RFC 3339 `created_at` string and all) so [`diff`] stays plain
/// data with no JSON/wire-format concerns — the same reasoning
/// `installations::InstallationAccount` vs `github_app::ListedInstallationAccount`
/// already follows for the two different endpoints that happen to
/// describe the same account fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInstallation {
    pub installation_id: u64,
    pub account_login: String,
    pub account_type: String,
    pub account_id: u64,
    pub installed_at_s: i64,
}

/// One action [`diff`] decides is needed to bring the local
/// `installations` table in line with GitHub's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Allowlisted installation GitHub has that the local table doesn't:
    /// `installations::upsert_installation`.
    Upsert(RemoteInstallation),
    /// Installation GitHub has whose account is not in
    /// `GITHUB_ALLOWED_ORGS`: `github_app::delete_installation`. No local
    /// row is ever written for it — same invariant the webhook path
    /// enforces for `installation.created` on a disallowed account.
    Uninstall { installation_id: u64 },
    /// Local row for an installation GitHub's list no longer contains:
    /// `installations::delete_installation_row`.
    DeleteLocal { installation_id: u64 },
}

/// Pure diff: given GitHub's current installation list and the
/// installation ids currently in the local `installations` table, decides
/// what to upsert/uninstall/delete. No network or D1 access — see module
/// docs.
///
/// Membership, not content, is what decides an upsert: an installation
/// GitHub reports whose id is already in `local_installation_ids` is left
/// alone (both sides already agree it exists), rather than re-upserted on
/// every pass — `installation_repositories`/`installation.suspend` events
/// already keep the rest of its state current via the webhook path, and
/// this job's job is to catch *missing* rows, not to re-sync every field
/// of a row both sides already have.
pub fn diff(
    remote: &[RemoteInstallation],
    local_installation_ids: &[u64],
    allowed_orgs: &str,
) -> Vec<ReconcileAction> {
    let mut actions = Vec::new();

    for installation in remote {
        if installations::is_allowed_org(&installation.account_login, allowed_orgs) {
            if !local_installation_ids.contains(&installation.installation_id) {
                actions.push(ReconcileAction::Upsert(installation.clone()));
            }
        } else {
            actions.push(ReconcileAction::Uninstall {
                installation_id: installation.installation_id,
            });
        }
    }

    let remote_ids: std::collections::HashSet<u64> =
        remote.iter().map(|i| i.installation_id).collect();
    for &installation_id in local_installation_ids {
        if !remote_ids.contains(&installation_id) {
            actions.push(ReconcileAction::DeleteLocal { installation_id });
        }
    }

    actions
}

/// Parses a GitHub REST API RFC 3339 timestamp (`created_at`, e.g.
/// `"2026-10-01T12:00:00Z"`) into unix seconds — the unit every other
/// timestamp in this codebase is stored/compared in
/// (`installations.installed_at`, `ingest_token`/`github_app` claim
/// fields). Pure, unit-tested without the Workers runtime.
pub fn parse_installed_at(created_at: &str) -> Result<i64, String> {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|dt| dt.timestamp())
        .map_err(|e| format!("cannot parse installation created_at {created_at:?}: {e}"))
}

/// Runs one full reconcile pass: mints an App JWT, fetches every page of
/// `GET /app/installations` ([`github_app::list_installations`]), reads
/// the local `installations` table ([`installations::list_installation_ids`]),
/// runs [`diff`], and applies each action.
///
/// A failed individual action (one bad uninstall call, one failed D1
/// write) is logged and does not abort the rest of the pass — same
/// best-effort reasoning as `lib.rs`'s webhook-driven
/// `uninstall_disallowed_installation`: a transient failure here is caught
/// again on the next scheduled run, not retried inline. A failure before
/// any actions are computed (JWT mint, the GitHub list call itself, or
/// the local D1 read) aborts the whole pass and is returned as an `Err`
/// for `lib.rs`'s `#[event(scheduled)]` handler to log.
pub async fn run(env: &Env) -> Result<(), String> {
    let private_key_pem = env
        .secret("GITHUB_APP_PRIVATE_KEY")
        .map_err(|e| format!("GITHUB_APP_PRIVATE_KEY is not configured: {e}"))?
        .to_string();
    let app_id: u64 = env
        .var("GITHUB_APP_ID")
        .map_err(|e| format!("GITHUB_APP_ID is not configured: {e}"))?
        .to_string()
        .parse()
        .map_err(|e| format!("GITHUB_APP_ID is not numeric: {e}"))?;
    let allowed_orgs = env
        .var("GITHUB_ALLOWED_ORGS")
        .map(|v| v.to_string())
        .unwrap_or_default();
    let now_s = (worker::Date::now().as_millis() / 1000) as i64;

    let app_jwt = github_app::mint_app_jwt(&private_key_pem, app_id, now_s)
        .await
        .map_err(|e| e.to_string())?;

    let listed = github_app::list_installations(&app_jwt)
        .await
        .map_err(|e| e.to_string())?;

    let remote: Vec<RemoteInstallation> = listed
        .iter()
        .filter_map(
            |installation| match parse_installed_at(&installation.created_at) {
                Ok(installed_at_s) => Some(RemoteInstallation {
                    installation_id: installation.id,
                    account_login: installation.account.login.clone(),
                    account_type: installation.account.account_type.clone(),
                    account_id: installation.account.id,
                    installed_at_s,
                }),
                Err(e) => {
                    worker::console_log!(
                        "reconcile: skipping installation {}: {e}",
                        installation.id
                    );
                    None
                }
            },
        )
        .collect();

    let local_ids = installations::list_installation_ids(env)
        .await
        .map_err(|e| format!("cannot list local installations: {e}"))?;

    for action in diff(&remote, &local_ids, &allowed_orgs) {
        match action {
            ReconcileAction::Upsert(installation) => {
                if let Err(e) = installations::upsert_installation(
                    env,
                    installation.installation_id,
                    &installation.account_login,
                    &installation.account_type,
                    installation.account_id,
                    installation.installed_at_s,
                )
                .await
                {
                    worker::console_log!(
                        "reconcile: upsert of installation {} failed: {e}",
                        installation.installation_id
                    );
                }
            }
            ReconcileAction::Uninstall { installation_id } => {
                if let Err(e) = github_app::delete_installation(&app_jwt, installation_id).await {
                    worker::console_log!(
                        "reconcile: uninstall of disallowed installation {installation_id} failed: {e}"
                    );
                }
            }
            ReconcileAction::DeleteLocal { installation_id } => {
                if let Err(e) = installations::delete_installation_row(env, installation_id).await {
                    worker::console_log!(
                        "reconcile: delete of local installation {installation_id} failed: {e}"
                    );
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(id: u64, login: &str) -> RemoteInstallation {
        RemoteInstallation {
            installation_id: id,
            account_login: login.to_string(),
            account_type: "Organization".to_string(),
            account_id: id * 100,
            installed_at_s: 1_000,
        }
    }

    #[test]
    fn github_has_it_we_dont_upserts() {
        let actions = diff(&[remote(1, "acme-corp")], &[], "acme-corp");
        assert_eq!(
            actions,
            vec![ReconcileAction::Upsert(remote(1, "acme-corp"))]
        );
    }

    #[test]
    fn we_have_it_github_doesnt_deletes_local() {
        let actions = diff(&[], &[1], "acme-corp");
        assert_eq!(
            actions,
            vec![ReconcileAction::DeleteLocal { installation_id: 1 }]
        );
    }

    #[test]
    fn github_has_it_but_disallowed_uninstalls_without_touching_local_rows() {
        let actions = diff(&[remote(1, "evil-corp")], &[], "acme-corp");
        assert_eq!(
            actions,
            vec![ReconcileAction::Uninstall { installation_id: 1 }]
        );
    }

    #[test]
    fn disallowed_installation_with_an_existing_local_row_is_still_uninstalled() {
        // Covers "org removed from the allowlist but not yet uninstalled"
        // (auth.md's "Org allowlist changes"/"Failure modes"): the local
        // row's mere existence does not exempt a now-disallowed account
        // from the uninstall attempt.
        let actions = diff(&[remote(1, "evil-corp")], &[1], "acme-corp");
        assert_eq!(
            actions,
            vec![ReconcileAction::Uninstall { installation_id: 1 }]
        );
    }

    #[test]
    fn both_agree_is_a_no_op() {
        let actions = diff(&[remote(1, "acme-corp")], &[1], "acme-corp");
        assert!(actions.is_empty());
    }

    #[test]
    fn multiple_installations_each_get_the_right_action() {
        let remote_list = vec![
            remote(1, "acme-corp"), // already local -> no-op
            remote(2, "acme-labs"), // missing locally -> upsert
            remote(3, "evil-corp"), // disallowed -> uninstall
        ];
        let local_ids = vec![1, 4]; // 4 no longer on GitHub's side -> delete
        let actions = diff(&remote_list, &local_ids, "acme-corp,acme-labs");
        assert_eq!(
            actions,
            vec![
                ReconcileAction::Upsert(remote(2, "acme-labs")),
                ReconcileAction::Uninstall { installation_id: 3 },
                ReconcileAction::DeleteLocal { installation_id: 4 },
            ]
        );
    }

    #[test]
    fn parses_rfc3339_timestamp_to_unix_seconds() -> Result<(), String> {
        assert_eq!(parse_installed_at("2026-10-01T12:00:00Z")?, 1_790_856_000);
        Ok(())
    }

    #[test]
    fn rejects_malformed_timestamp() {
        assert!(parse_installed_at("not-a-timestamp").is_err());
    }
}
