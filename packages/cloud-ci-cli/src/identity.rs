//! Derives the `(repo_id, sha, run_key, attempt)` run identity, per
//! `docs/design/byo-ci.md`'s "Run identity" table, plus the ingest server URL
//! and the job name.
//!
//! Precedence for every field: explicit CLI flag > `CLOUD_CI_*` environment
//! variable > GitHub Actions auto-detection > hard error. `attempt` is the
//! only field with a further fallback (`1`) once every other source is
//! exhausted, matching the table's "defaults to 1" note. The job name
//! follows the same precedence via [`resolve_job_name`], kept separate from
//! [`RunIdentity`] since it isn't part of the run-identity tuple.
//!
//! Resolution reads environment variables and (for `sha` on `pull_request`
//! events) one JSON file through [`EnvSource`] rather than `std::env`/`std::fs`
//! directly, so the precedence logic is a pure function of its inputs and can
//! be unit tested without touching the real process environment.

#[cfg(test)]
use std::collections::HashMap;

/// Everything resolution needs from the outside world.
pub trait EnvSource {
    fn var(&self, key: &str) -> Option<String>;
    fn read_to_string(&self, path: &str) -> Option<String>;
}

/// Reads the real process environment and filesystem.
pub struct RealEnv;

impl EnvSource for RealEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn read_to_string(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

/// Explicit `--sha`/`--run-key`/`--attempt`/`--repo-id`/`--server-url` flags,
/// as parsed from the command line (before precedence is applied).
#[derive(Debug, Clone, Default)]
pub struct RunIdentityFlags {
    pub sha: Option<String>,
    pub run_key: Option<String>,
    pub attempt: Option<u32>,
    pub repo_id: Option<u64>,
    pub server_url: Option<String>,
}

/// The fully resolved run identity plus the deployment base URL to call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIdentity {
    pub sha: String,
    pub run_key: String,
    pub attempt: u32,
    pub repo_id: u64,
    pub server_url: String,
}

/// Resolves [`RunIdentity`] from `flags` and `env`, or returns the list of
/// fields that could not be resolved from any source (a hard error).
pub fn resolve_run_identity(
    flags: &RunIdentityFlags,
    env: &dyn EnvSource,
) -> Result<RunIdentity, Vec<String>> {
    let in_github_actions = env.var("GITHUB_ACTIONS").as_deref() == Some("true");

    let mut missing = Vec::new();

    let sha = flags
        .sha
        .clone()
        .or_else(|| non_empty(env.var("CLOUD_CI_SHA")))
        .or_else(|| in_github_actions.then(|| github_actions_sha(env)).flatten());
    if sha.is_none() {
        missing.push("--sha (or CLOUD_CI_SHA)".to_string());
    }

    let run_key = flags
        .run_key
        .clone()
        .or_else(|| non_empty(env.var("CLOUD_CI_RUN_KEY")))
        .or_else(|| {
            in_github_actions
                .then(|| env.var("GITHUB_RUN_ID").map(|id| format!("gha/{id}")))
                .flatten()
        });
    if run_key.is_none() {
        missing.push("--run-key (or CLOUD_CI_RUN_KEY)".to_string());
    }

    let attempt = flags
        .attempt
        .or_else(|| non_empty(env.var("CLOUD_CI_ATTEMPT")).and_then(|v| v.parse().ok()))
        .or_else(|| {
            in_github_actions
                .then(|| env.var("GITHUB_RUN_ATTEMPT").and_then(|v| v.parse().ok()))
                .flatten()
        })
        .unwrap_or(1);

    let repo_id = resolve_repo_id(flags.repo_id, env);
    if repo_id.is_none() {
        missing.push("--repo-id (or CLOUD_CI_REPO_ID)".to_string());
    }

    let server_url = resolve_server_url(flags.server_url.clone(), env);
    if server_url.is_none() {
        missing.push("--server-url (or CLOUD_CI_SERVER_URL)".to_string());
    }

    if !missing.is_empty() {
        return Err(missing);
    }

    Ok(RunIdentity {
        // Each field was just proven `Some` above, but the compiler can't see
        // that across the `missing` checks, so fall back to an empty/zero
        // value that is unreachable in practice rather than using `unwrap`.
        sha: sha.unwrap_or_default(),
        run_key: run_key.unwrap_or_default(),
        attempt,
        repo_id: repo_id.unwrap_or_default(),
        server_url: server_url.unwrap_or_default(),
    })
}

/// Resolves `repo_id`: explicit flag > `CLOUD_CI_REPO_ID` env var >
/// GitHub Actions `GITHUB_REPOSITORY_ID` auto-detection. Factored out of
/// [`resolve_run_identity`] so `cloud-ci split --strategy timing`
/// (`crate::split`'s `RemoteHistoryLookup`) can resolve just this field
/// without needing the full `(sha, run_key, attempt)` run identity it has
/// no use for.
pub(crate) fn resolve_repo_id(explicit: Option<u64>, env: &dyn EnvSource) -> Option<u64> {
    let in_github_actions = env.var("GITHUB_ACTIONS").as_deref() == Some("true");
    explicit
        .or_else(|| non_empty(env.var("CLOUD_CI_REPO_ID")).and_then(|v| v.parse().ok()))
        .or_else(|| {
            in_github_actions
                .then(|| env.var("GITHUB_REPOSITORY_ID").and_then(|v| v.parse().ok()))
                .flatten()
        })
}

/// Resolves the deployment base URL: explicit flag > `CLOUD_CI_SERVER_URL`
/// env var. Same factoring reason as [`resolve_repo_id`].
pub(crate) fn resolve_server_url(explicit: Option<String>, env: &dyn EnvSource) -> Option<String> {
    explicit.or_else(|| non_empty(env.var("CLOUD_CI_SERVER_URL")))
}

/// Resolves the job name: explicit `--job` flag > `CLOUD_CI_JOB` env var >
/// `$GITHUB_JOB` when running in GitHub Actions > hard error. Kept separate
/// from [`RunIdentity`] because the job name is not part of the
/// `(repo_id, sha, run_key, attempt)` run-identity tuple, but it follows the
/// same flag > `CLOUD_CI_*` env > GitHub Actions auto-detect > error
/// precedence through the same testable [`EnvSource`].
pub fn resolve_job_name(flag: Option<&str>, env: &dyn EnvSource) -> Result<String, String> {
    let in_github_actions = env.var("GITHUB_ACTIONS").as_deref() == Some("true");

    flag.map(str::to_string)
        .or_else(|| non_empty(env.var("CLOUD_CI_JOB")))
        .or_else(|| {
            in_github_actions
                .then(|| non_empty(env.var("GITHUB_JOB")))
                .flatten()
        })
        .ok_or_else(|| "--job (or CLOUD_CI_JOB)".to_string())
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

/// `sha` on GitHub Actions: `GITHUB_EVENT_PATH`'s `pull_request.head.sha` on
/// `pull_request` events (never the ephemeral merge-commit `GITHUB_SHA`),
/// otherwise `GITHUB_SHA`.
fn github_actions_sha(env: &dyn EnvSource) -> Option<String> {
    let pull_request_head_sha = (env.var("GITHUB_EVENT_NAME").as_deref() == Some("pull_request"))
        .then(|| env.var("GITHUB_EVENT_PATH"))
        .flatten()
        .and_then(|path| env.read_to_string(&path))
        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
        .and_then(|json| {
            json.pointer("/pull_request/head/sha")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    pull_request_head_sha.or_else(|| non_empty(env.var("GITHUB_SHA")))
}

/// Test double backed by in-memory maps, so precedence logic never touches
/// the real environment or filesystem.
#[cfg(test)]
pub struct MapEnv {
    vars: HashMap<String, String>,
    files: HashMap<String, String>,
}

#[cfg(test)]
impl MapEnv {
    pub fn new(vars: &[(&str, &str)]) -> Self {
        Self {
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            files: HashMap::new(),
        }
    }

    pub fn with_file(mut self, path: &str, contents: &str) -> Self {
        self.files.insert(path.to_string(), contents.to_string());
        self
    }
}

#[cfg(test)]
impl EnvSource for MapEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }

    fn read_to_string(&self, path: &str) -> Option<String> {
        self.files.get(path).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_flags_win_over_everything() -> Result<(), Vec<String>> {
        let flags = RunIdentityFlags {
            sha: Some("flagsha".to_string()),
            run_key: Some("flagkey".to_string()),
            attempt: Some(9),
            repo_id: Some(42),
            server_url: Some("https://flag.example".to_string()),
        };
        let env = MapEnv::new(&[
            ("GITHUB_ACTIONS", "true"),
            ("CLOUD_CI_SHA", "envsha"),
            ("CLOUD_CI_RUN_KEY", "envkey"),
            ("CLOUD_CI_ATTEMPT", "2"),
            ("CLOUD_CI_REPO_ID", "7"),
            ("CLOUD_CI_SERVER_URL", "https://env.example"),
            ("GITHUB_SHA", "ghasha"),
            ("GITHUB_RUN_ID", "100"),
            ("GITHUB_RUN_ATTEMPT", "3"),
            ("GITHUB_REPOSITORY_ID", "9"),
        ]);

        let identity = resolve_run_identity(&flags, &env)?;
        assert_eq!(identity.sha, "flagsha");
        assert_eq!(identity.run_key, "flagkey");
        assert_eq!(identity.attempt, 9);
        assert_eq!(identity.repo_id, 42);
        assert_eq!(identity.server_url, "https://flag.example");
        Ok(())
    }

    #[test]
    fn cloud_ci_env_wins_over_github_actions_auto_detect() -> Result<(), Vec<String>> {
        let flags = RunIdentityFlags::default();
        let env = MapEnv::new(&[
            ("GITHUB_ACTIONS", "true"),
            ("CLOUD_CI_SHA", "envsha"),
            ("CLOUD_CI_RUN_KEY", "envkey"),
            ("CLOUD_CI_ATTEMPT", "2"),
            ("CLOUD_CI_REPO_ID", "7"),
            ("CLOUD_CI_SERVER_URL", "https://env.example"),
            ("GITHUB_SHA", "ghasha"),
            ("GITHUB_RUN_ID", "100"),
            ("GITHUB_RUN_ATTEMPT", "3"),
            ("GITHUB_REPOSITORY_ID", "9"),
        ]);

        let identity = resolve_run_identity(&flags, &env)?;
        assert_eq!(identity.sha, "envsha");
        assert_eq!(identity.run_key, "envkey");
        assert_eq!(identity.attempt, 2);
        assert_eq!(identity.repo_id, 7);
        assert_eq!(identity.server_url, "https://env.example");
        Ok(())
    }

    #[test]
    fn github_actions_pull_request_sha_comes_from_event_payload_not_github_sha()
    -> Result<(), Vec<String>> {
        let flags = RunIdentityFlags {
            server_url: Some("https://s.example".to_string()),
            ..Default::default()
        };
        let env = MapEnv::new(&[
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_EVENT_NAME", "pull_request"),
            ("GITHUB_EVENT_PATH", "/tmp/event.json"),
            ("GITHUB_SHA", "mergecommitsha"),
            ("GITHUB_RUN_ID", "100"),
            ("GITHUB_REPOSITORY_ID", "9"),
        ])
        .with_file(
            "/tmp/event.json",
            r#"{"pull_request":{"head":{"sha":"headsha"}}}"#,
        );

        let identity = resolve_run_identity(&flags, &env)?;
        assert_eq!(identity.sha, "headsha");
        assert_eq!(identity.run_key, "gha/100");
        Ok(())
    }

    #[test]
    fn github_actions_push_sha_falls_back_to_github_sha() -> Result<(), Vec<String>> {
        let flags = RunIdentityFlags {
            server_url: Some("https://s.example".to_string()),
            ..Default::default()
        };
        let env = MapEnv::new(&[
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_EVENT_NAME", "push"),
            ("GITHUB_SHA", "pushsha"),
            ("GITHUB_RUN_ID", "100"),
            ("GITHUB_RUN_ATTEMPT", "4"),
            ("GITHUB_REPOSITORY_ID", "9"),
        ]);

        let identity = resolve_run_identity(&flags, &env)?;
        assert_eq!(identity.sha, "pushsha");
        assert_eq!(identity.attempt, 4);
        Ok(())
    }

    #[test]
    fn attempt_defaults_to_one_outside_github_actions_with_nothing_set() -> Result<(), Vec<String>>
    {
        let flags = RunIdentityFlags {
            sha: Some("s".to_string()),
            run_key: Some("k".to_string()),
            repo_id: Some(1),
            server_url: Some("https://s.example".to_string()),
            attempt: None,
        };
        let env = MapEnv::new(&[]);

        let identity = resolve_run_identity(&flags, &env)?;
        assert_eq!(identity.attempt, 1);
        Ok(())
    }

    #[test]
    fn missing_everything_outside_github_actions_lists_every_required_field() {
        let flags = RunIdentityFlags::default();
        let env = MapEnv::new(&[]);

        let result = resolve_run_identity(&flags, &env);
        assert_eq!(
            result,
            Err(vec![
                "--sha (or CLOUD_CI_SHA)".to_string(),
                "--run-key (or CLOUD_CI_RUN_KEY)".to_string(),
                "--repo-id (or CLOUD_CI_REPO_ID)".to_string(),
                "--server-url (or CLOUD_CI_SERVER_URL)".to_string(),
            ])
        );
    }

    #[test]
    fn job_name_flag_wins_over_everything() {
        let env = MapEnv::new(&[
            ("GITHUB_ACTIONS", "true"),
            ("CLOUD_CI_JOB", "env-job"),
            ("GITHUB_JOB", "gha-job"),
        ]);
        assert_eq!(
            resolve_job_name(Some("flag-job"), &env),
            Ok("flag-job".to_string())
        );
    }

    #[test]
    fn job_name_cloud_ci_env_wins_over_github_actions_auto_detect() {
        let env = MapEnv::new(&[
            ("GITHUB_ACTIONS", "true"),
            ("CLOUD_CI_JOB", "env-job"),
            ("GITHUB_JOB", "gha-job"),
        ]);
        assert_eq!(resolve_job_name(None, &env), Ok("env-job".to_string()));
    }

    #[test]
    fn job_name_falls_back_to_github_job_in_actions() {
        let env = MapEnv::new(&[("GITHUB_ACTIONS", "true"), ("GITHUB_JOB", "gha-job")]);
        assert_eq!(resolve_job_name(None, &env), Ok("gha-job".to_string()));
    }

    #[test]
    fn job_name_errors_when_nothing_set() {
        let env = MapEnv::new(&[]);
        assert_eq!(
            resolve_job_name(None, &env),
            Err("--job (or CLOUD_CI_JOB)".to_string())
        );
    }
}
