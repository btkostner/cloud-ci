//! Per-repo role resolution (docs/design/auth.md § "Role resolution"):
//! given a signed-in user's GitHub login and a target repo, resolves the
//! user's cloud-ci role (`viewer`/`operator`/`admin`) from their GitHub
//! collaborator permission on that repo, looked up through the repo's own
//! installation token, read-through cached in D1 (`repo_role_cache`) with
//! a 5-minute TTL.
//!
//! Same three-layer split as `installations.rs`/`api_tokens.rs`:
//!
//! 1. [`map_role`], [`Role::as_str`]/[`Role::parse`], [`cache_is_fresh`],
//!    [`collaborator_permission_url`] — pure, no `worker` dependency.
//!    Unit-tested with plain `cargo test`.
//! 2. [`fetch_collaborator_permission`] — the actual
//!    `GET /repos/{owner}/{repo}/collaborators/{username}/permission`
//!    HTTP call. **Not live-verified**: no real GitHub App installation
//!    exists in this environment to call it against (same limitation as
//!    `github_app.rs`'s `/app/*` endpoints — see that module's docs). Its
//!    URL/header construction and response-shape parsing are unit-tested.
//! 3. [`lookup_cached_role`], [`upsert_role_cache`],
//!    [`lookup_repo_owner`] — need the Workers runtime (D1), exercised
//!    only by the live smoke test.
//!
//! [`resolve_role`] composes all three into the full read-through-cache
//! decision, including the Failure modes table's degraded-service
//! behavior ("Serve the last cached `repo_role_cache` row if present,
//! even if its TTL expired, rather than fail closed immediately; if no
//! cached row exists, resolve to viewer").
//!
//! `lib.rs::handle_issue_token` (`POST /v1/tokens`, `src/token_issuance.rs`)
//! is the first real caller: it calls [`resolve_role`] for every repo id
//! it needs admin on before minting a token, so this module is wired as
//! an actual authorization gate there. No other route in `lib.rs` calls
//! it yet, though — it is not yet a general dashboard/RPC authorization
//! gate, just no longer true that nothing calls it.

use serde::{Deserialize, Serialize};
use worker::Env;
use worker::wasm_bindgen::JsValue;

#[derive(Debug, PartialEq, Eq)]
pub struct RolesError(pub String);

impl std::fmt::Display for RolesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RolesError {}

/// The three cloud-ci roles (auth.md § "Role model and permission
/// matrix"), ordered least-to-most privileged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    /// The exact string stored in `repo_role_cache.role`.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }

    /// Parses a `repo_role_cache.role` value back into a [`Role`]. `None`
    /// for anything that isn't one of the three stored strings — a
    /// corrupted/foreign row, which callers treat as a cache miss rather
    /// than trusting an unrecognized value.
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "viewer" => Some(Role::Viewer),
            "operator" => Some(Role::Operator),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }
}

/// Maps a `GET /repos/{owner}/{repo}/collaborators/{username}/permission`
/// response's `role_name`/`permission` pair to a cloud-ci [`Role`], per
/// auth.md § "Role resolution"'s table:
///
/// | `role_name` | cloud-ci role |
/// | --- | --- |
/// | `read`, `triage` | viewer |
/// | `write` | operator |
/// | `maintain`, `admin` | admin |
/// | any custom repository role not listed above | fall back to `permission`: `admin`→admin, `write`→operator, else viewer |
///
/// Pure, exhaustively unit-tested without the Workers runtime.
pub fn map_role(role_name: &str, permission: &str) -> Role {
    match role_name {
        "read" | "triage" => Role::Viewer,
        "write" => Role::Operator,
        "maintain" | "admin" => Role::Admin,
        _ => match permission {
            "admin" => Role::Admin,
            "write" => Role::Operator,
            _ => Role::Viewer,
        },
    }
}

/// `repo_role_cache`'s 5-minute TTL (auth.md § "Role resolution": "cached
/// in D1 ... with a 5-minute TTL per `(user, repo)`").
pub const ROLE_CACHE_TTL_SECONDS: i64 = 5 * 60;

/// Pure "is this cached row still within its TTL" decision — split out
/// from the D1 read so it's unit-testable with plain `cargo test`, same
/// layering as `api_tokens::check_token`'s expiry check. `now_unix_s` is
/// a parameter for the same determinism/testability reason as every
/// other `now_unix_s`-taking pure function in this codebase.
pub fn cache_is_fresh(checked_at_s: i64, now_unix_s: i64, ttl_s: i64) -> bool {
    now_unix_s - checked_at_s < ttl_s
}

/// `GET /repos/{owner}/{repo}/collaborators/{username}/permission`'s URL,
/// with `owner`/`repo`/`username` interpolated — pure string
/// construction, unit-testable without the Workers runtime.
pub fn collaborator_permission_url(owner: &str, repo: &str, username: &str) -> String {
    format!("https://api.github.com/repos/{owner}/{repo}/collaborators/{username}/permission")
}

/// `GET /repos/{owner}/{repo}/collaborators/{username}/permission`'s
/// response shape (docs.github.com/en/rest/collaborators/collaborators,
/// accessed 2026-09-30): `permission` collapses `maintain` into `write`
/// and `triage` into `read`, while `role_name` reports the precise role —
/// see [`map_role`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaboratorPermissionResponse {
    pub permission: String,
    pub role_name: String,
}

const USER_AGENT: &str = "cloud-ci-worker";
const GITHUB_API_VERSION: &str = "2022-11-28";

/// Calls `GET /repos/{owner}/{repo}/collaborators/{username}/permission`
/// using an installation access token
/// ([`crate::github_app::fetch_installation_token`]'s output) — this
/// endpoint only needs the App's mandatory `Metadata: read` permission
/// and accepts an installation token (auth.md § "Role resolution").
/// **Not live-verified** — see module docs.
pub async fn fetch_collaborator_permission(
    installation_token: &str,
    owner: &str,
    repo: &str,
    username: &str,
) -> Result<CollaboratorPermissionResponse, RolesError> {
    let url = collaborator_permission_url(owner, repo, username);

    let headers = worker::Headers::new();
    headers
        .set("authorization", &format!("Bearer {installation_token}"))
        .map_err(|e| RolesError(format!("cannot set authorization header: {e}")))?;
    headers
        .set("accept", "application/vnd.github+json")
        .map_err(|e| RolesError(format!("cannot set accept header: {e}")))?;
    headers
        .set("x-github-api-version", GITHUB_API_VERSION)
        .map_err(|e| RolesError(format!("cannot set api-version header: {e}")))?;
    headers
        .set("user-agent", USER_AGENT)
        .map_err(|e| RolesError(format!("cannot set user-agent header: {e}")))?;

    let mut init = worker::RequestInit::new();
    init.with_method(worker::Method::Get);
    init.with_headers(headers);

    let request = worker::Request::new_with_init(&url, &init)
        .map_err(|e| RolesError(format!("cannot build collaborator-permission request: {e}")))?;

    let mut response = worker::Fetch::Request(request)
        .send()
        .await
        .map_err(|e| RolesError(format!("collaborator-permission request failed: {e}")))?;

    if response.status_code() != 200 {
        let body = response.text().await.unwrap_or_default();
        return Err(RolesError(format!(
            "collaborator-permission lookup failed: {} {body}",
            response.status_code()
        )));
    }

    response
        .json::<CollaboratorPermissionResponse>()
        .await
        .map_err(|e| {
            RolesError(format!(
                "cannot decode collaborator-permission response: {e}"
            ))
        })
}

// ---------------------------------------------------------------------------
// D1 reads/writes — needs the Workers runtime, not covered by `cargo test`
// (see module docs).
// ---------------------------------------------------------------------------

/// A `repo_role_cache` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleCacheRow {
    pub role: String,
    pub checked_at: i64,
}

/// Looks up the `repo_role_cache` row for `(user_id, repo_id)`, if any.
pub async fn lookup_cached_role(
    env: &Env,
    user_id: &str,
    repo_id: u64,
) -> worker::Result<Option<RoleCacheRow>> {
    let db = env.d1("DB")?;
    let row = db
        .prepare("SELECT role, checked_at FROM repo_role_cache WHERE user_id = ?1 AND repo_id = ?2")
        .bind(&[
            JsValue::from_str(user_id),
            JsValue::from_f64(repo_id as f64),
        ])?
        .first::<serde_json::Value>(None)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let role = row
        .get("role")
        .and_then(|v| v.as_str())
        .ok_or_else(|| worker::Error::RustError("repo_role_cache row missing role".into()))?
        .to_string();
    let checked_at = row
        .get("checked_at")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| worker::Error::RustError("repo_role_cache row missing checked_at".into()))?;
    Ok(Some(RoleCacheRow { role, checked_at }))
}

/// Upserts the `repo_role_cache` row for `(user_id, repo_id)` — the
/// write-back half of the read-through cache.
pub async fn upsert_role_cache(
    env: &Env,
    user_id: &str,
    repo_id: u64,
    role: &str,
    checked_at_s: i64,
) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare(
        "INSERT INTO repo_role_cache (user_id, repo_id, role, checked_at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (user_id, repo_id) DO UPDATE SET \
             role = excluded.role, \
             checked_at = excluded.checked_at",
    )
    .bind(&[
        JsValue::from_str(user_id),
        JsValue::from_f64(repo_id as f64),
        JsValue::from_str(role),
        JsValue::from_f64(checked_at_s as f64),
    ])?
    .run()
    .await?;
    Ok(())
}

/// The `repos`/`installations` join row role resolution needs: the owning
/// installation's id (to mint an installation token for) and the
/// `owner/repo` pair the collaborator-permission endpoint takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOwnerRow {
    pub installation_id: u64,
    pub owner_login: String,
    pub repo_name: String,
}

/// Looks up the `installations`/`repos` join row for `repo_id` — same
/// join `installations::lookup_repo_installation` does, but returning the
/// fields this module needs (`installation_id`/`account_login`/`name`)
/// rather than that function's (`account_id`/`suspended_at`). Kept as its
/// own query rather than widening `lookup_repo_installation`'s return
/// shape for an unrelated caller.
pub async fn lookup_repo_owner(env: &Env, repo_id: u64) -> worker::Result<Option<RepoOwnerRow>> {
    let db = env.d1("DB")?;
    let row = db
        .prepare(
            "SELECT installations.installation_id, installations.account_login, repos.name \
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
    let installation_id = row
        .get("installation_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| {
            worker::Error::RustError("repos/installations row missing installation_id".into())
        })?;
    let owner_login = row
        .get("account_login")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            worker::Error::RustError("repos/installations row missing account_login".into())
        })?
        .to_string();
    let repo_name = row
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| worker::Error::RustError("repos/installations row missing name".into()))?
        .to_string();
    Ok(Some(RepoOwnerRow {
        installation_id,
        owner_login,
        repo_name,
    }))
}

/// Full read-through-cache role resolution (auth.md § "Role resolution",
/// "Failure modes"):
///
/// 1. If a cached row exists and is within [`ROLE_CACHE_TTL_SECONDS`]
///    ([`cache_is_fresh`]), return it, parsed via [`Role::parse`].
/// 2. Otherwise, look up the repo's owning installation
///    ([`lookup_repo_owner`]), mint an installation token
///    ([`crate::github_app::mint_app_jwt`] +
///    [`crate::github_app::fetch_installation_token`]), and call
///    [`fetch_collaborator_permission`] for `github_login`. On success,
///    map the result via [`map_role`], write it back
///    ([`upsert_role_cache`]), and return it.
/// 3. If the GitHub call fails (rate limit, outage, etc.) and a stale
///    cached row exists, serve that stale row rather than failing closed
///    (auth.md's Failure modes table: "Serve the last cached
///    `repo_role_cache` row if present, even if its TTL expired"). If no
///    cached row exists at all, resolve to [`Role::Viewer`] (least
///    privilege) — the caller is expected to surface a degraded-service
///    banner, per the same table.
/// 4. If the repo has no `repos`/`installations` row at all
///    ([`lookup_repo_owner`] returns `None`), that's a distinct error
///    (the repo isn't registered with this deployment) — resolving to
///    viewer here would incorrectly imply "known repo, no access" rather
///    than "unknown repo", so this case returns `Err`.
pub async fn resolve_role(
    env: &Env,
    user_id: &str,
    repo_id: u64,
    github_login: &str,
    now_s: i64,
) -> worker::Result<Role> {
    let cached = lookup_cached_role(env, user_id, repo_id).await?;
    if let Some(row) = &cached
        && cache_is_fresh(row.checked_at, now_s, ROLE_CACHE_TTL_SECONDS)
        && let Some(role) = Role::parse(&row.role)
    {
        return Ok(role);
    }

    let Some(owner_row) = lookup_repo_owner(env, repo_id).await? else {
        return Err(worker::Error::RustError(
            "repo is not registered with this deployment".into(),
        ));
    };

    match resolve_role_from_github(env, &owner_row, github_login, now_s).await {
        Ok(role) => {
            upsert_role_cache(env, user_id, repo_id, role.as_str(), now_s).await?;
            Ok(role)
        }
        Err(e) => {
            // Degraded-service fallback (auth.md's Failure modes table) —
            // see doc comment above.
            if let Some(row) = cached.and_then(|row| Role::parse(&row.role)) {
                worker::console_log!(
                    "role lookup for user {user_id} repo {repo_id} failed ({e}); serving stale cached role"
                );
                Ok(row)
            } else {
                worker::console_log!(
                    "role lookup for user {user_id} repo {repo_id} failed ({e}); no cached row, defaulting to viewer"
                );
                Ok(Role::Viewer)
            }
        }
    }
}

/// Mints an App JWT and exchanges it for the installation token owning
/// `owner_row`'s repo — the common first half every GitHub-REST-calling
/// path in this crate needs once it has resolved a `repo_id` to its
/// owning installation ([`lookup_repo_owner`]). Shared by
/// [`resolve_role_from_github`] (below) and `coordinator`'s Check Run
/// wiring (docs/design/byo-ci.md's "Checks and scopes"), so both reuse
/// one GitHub-App-JWT-minting + installation-token-exchange
/// implementation instead of duplicating it.
pub async fn installation_token_for_repo(
    env: &Env,
    owner_row: &RepoOwnerRow,
    now_s: i64,
) -> Result<crate::github_app::InstallationToken, RolesError> {
    let private_key_pem = env
        .secret("GITHUB_APP_PRIVATE_KEY")
        .map_err(|e| RolesError(format!("GITHUB_APP_PRIVATE_KEY is not configured: {e}")))?
        .to_string();
    let app_id: u64 = env
        .var("GITHUB_APP_ID")
        .map_err(|e| RolesError(format!("GITHUB_APP_ID is not configured: {e}")))?
        .to_string()
        .parse()
        .map_err(|e| RolesError(format!("GITHUB_APP_ID is not numeric: {e}")))?;
    let app_jwt = crate::github_app::mint_app_jwt(&private_key_pem, app_id, now_s)
        .await
        .map_err(|e| RolesError(e.to_string()))?;
    crate::github_app::fetch_installation_token(&app_jwt, owner_row.installation_id)
        .await
        .map_err(|e| RolesError(e.to_string()))
}

/// The GitHub-calling half of [`resolve_role`]'s cache-miss path: mints
/// an App JWT, exchanges it for the owning installation's token, calls
/// the collaborator-permission endpoint, and maps the result. Split out
/// so [`resolve_role`]'s fallback logic (step 3 above) has one `Err` path
/// to catch, regardless of which sub-step failed.
async fn resolve_role_from_github(
    env: &Env,
    owner_row: &RepoOwnerRow,
    github_login: &str,
    now_s: i64,
) -> Result<Role, RolesError> {
    let installation_token = installation_token_for_repo(env, owner_row, now_s).await?;
    let permission = fetch_collaborator_permission(
        &installation_token.token,
        &owner_row.owner_login,
        &owner_row.repo_name,
        github_login,
    )
    .await?;
    Ok(map_role(&permission.role_name, &permission.permission))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_role_read_and_triage_are_viewer() {
        assert_eq!(map_role("read", "read"), Role::Viewer);
        assert_eq!(map_role("triage", "read"), Role::Viewer);
    }

    #[test]
    fn map_role_write_is_operator() {
        assert_eq!(map_role("write", "write"), Role::Operator);
    }

    #[test]
    fn map_role_maintain_and_admin_are_admin() {
        assert_eq!(map_role("maintain", "write"), Role::Admin);
        assert_eq!(map_role("admin", "admin"), Role::Admin);
    }

    #[test]
    fn map_role_custom_role_falls_back_to_permission_admin() {
        assert_eq!(map_role("custom-deploy-role", "admin"), Role::Admin);
    }

    #[test]
    fn map_role_custom_role_falls_back_to_permission_write() {
        assert_eq!(map_role("custom-deploy-role", "write"), Role::Operator);
    }

    #[test]
    fn map_role_custom_role_falls_back_to_viewer_for_anything_else() {
        assert_eq!(map_role("custom-deploy-role", "read"), Role::Viewer);
        assert_eq!(map_role("custom-deploy-role", "none"), Role::Viewer);
        assert_eq!(map_role("unknown", "unknown"), Role::Viewer);
    }

    #[test]
    fn role_as_str_and_parse_round_trip() {
        for role in [Role::Viewer, Role::Operator, Role::Admin] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
    }

    #[test]
    fn role_parse_rejects_unknown_strings() {
        assert_eq!(Role::parse("superadmin"), None);
        assert_eq!(Role::parse(""), None);
    }

    #[test]
    fn cache_is_fresh_within_ttl() {
        assert!(cache_is_fresh(1_000, 1_299, 300));
    }

    #[test]
    fn cache_is_fresh_at_exact_ttl_boundary_is_stale() {
        // now - checked_at == ttl is NOT fresh (strict less-than): a row
        // checked exactly 5 minutes ago should trigger a refresh, not be
        // treated as still within the window.
        assert!(!cache_is_fresh(1_000, 1_300, 300));
    }

    #[test]
    fn cache_is_fresh_past_ttl_is_stale() {
        assert!(!cache_is_fresh(1_000, 1_301, 300));
    }

    #[test]
    fn collaborator_permission_url_interpolates_owner_repo_username() {
        let url = collaborator_permission_url("acme-corp", "widgets", "octocat");
        assert_eq!(
            url,
            "https://api.github.com/repos/acme-corp/widgets/collaborators/octocat/permission"
        );
    }

    #[test]
    fn collaborator_permission_response_deserializes() -> Result<(), serde_json::Error> {
        let json = r#"{"permission":"write","role_name":"maintain"}"#;
        let resp: CollaboratorPermissionResponse = serde_json::from_str(json)?;
        assert_eq!(resp.permission, "write");
        assert_eq!(resp.role_name, "maintain");
        Ok(())
    }
}
