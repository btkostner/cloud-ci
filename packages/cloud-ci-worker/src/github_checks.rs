//! GitHub Check Run and PR sticky-comment API clients: the wire-level
//! callers [docs/design/pr-comment.md](../../../docs/design/pr-comment.md)
//! describes, covering both halves of its "Summary" ("Check Runs (optional,
//! named)" and "One sticky PR comment (optional, per repo)"):
//!
//! - Check Runs: `POST /repos/{owner}/{repo}/check-runs` (create) and
//!   `PATCH /repos/{owner}/{repo}/check-runs/{check_run_id}` (update)
//!   (docs.github.com/en/rest/checks/runs, accessed 2026-10-02). Exactly
//!   when a check is created and how its `conclusion` is computed —
//!   `ci.check(name, opts)` creates it immediately as `queued`; its
//!   conclusion is "the worst of its attached nodes, with cached counting
//!   as success" once sealed, or `success`/"no matching tasks" if sealed
//!   with no attached nodes, or `failure` if the script itself failed —
//!   is decided entirely by the coordinator
//!   ([docs/design/dynamic-pipelines.md § "GitHub status checks"](../../../docs/design/dynamic-pipelines.md));
//!   this module only has to be able to carry whatever `status`/
//!   `conclusion`/`output` the coordinator hands it.
//! - Issue comments (PRs are issues in GitHub's API — pr-comment.md's own
//!   framing of its "Marker-based upsert" section): `GET
//!   /repos/{owner}/{repo}/issues/{issue_number}/comments` (list, to find
//!   an existing sticky comment), `POST
//!   /repos/{owner}/{repo}/issues/{issue_number}/comments` (create), and
//!   `PATCH /repos/{owner}/{repo}/issues/comments/{comment_id}` (update)
//!   (docs.github.com/en/rest/issues/comments, accessed 2026-10-02).
//! - [`find_sticky_comment_index`]: the pure string-matching half of
//!   pr-comment.md's "Marker-based upsert" step 3 ("list ... Adopt the
//!   first comment whose body starts with the `cloud-ci:pr-comment`
//!   marker") — given a list of comment bodies and a `(repo_id,
//!   pr_number)` pair, finds the first body carrying that pair's exact
//!   marker. The doc's other half of step 3 (also checking
//!   `performed_via_github_app.id` equals our App id, "Comments carrying
//!   our marker from any other author are ignored, since anyone can paste
//!   the marker") needs the full [`IssueComment`] (its `id`, to adopt),
//!   not just the body text, so that filter is the caller's job once it
//!   has the real list — this function only locates the candidate by
//!   content.
//!
//! All five HTTP-calling functions here authenticate with an
//! **installation** access token
//! ([`github_app::fetch_installation_token`]'s output), not an App-level
//! JWT — these are repo-scoped actions (creating/editing a check or a
//! comment on one repo), unlike the App-level `/app/*` endpoints
//! `github_app.rs` calls. They all send the identical four-header set
//! ([`installation_token_headers`]): Bearer installation token, the
//! recommended `Accept`, the pinned `X-GitHub-Api-Version`, and the
//! `User-Agent` GitHub requires on every REST API request — the same
//! style as `github_app.rs`'s `app_jwt_headers`, consolidated into one
//! shared builder here since these five calls need exactly that set and
//! nothing endpoint-specific among them.
//!
//! Same layering and the same honest boundary as `github_app.rs`: URL/
//! header construction and response-shape parsing are pure enough to
//! unit test (fed realistic fixture JSON matching GitHub's documented
//! response shapes) and are exercised by `cargo test`; the actual HTTP
//! calls are **not** live-verified against GitHub, since no real GitHub
//! repo/PR/installation token exists in this environment to call them
//! against.
//!
//! [`create_check_run`]/[`update_check_run`] now have their first real
//! caller: `coordinator::mod`'s `RunCoordinator` calls them from
//! `StartJob` (create, first time a job names a check),
//! `CompleteShard` (update, shard-table progress), and run close
//! (update, final `completed`/conclusion) — see that module's doc
//! comment for the storage design and idempotency mechanism. The issue
//! (PR sticky-comment) comment functions below still have no caller:
//! webhook handling and `PullRequestState`'s flush loop don't exist yet.
//! Templating/rendering a `PrReport` into the markdown these functions'
//! `body`/`output.summary` fields carry is still out of scope — the
//! Check Run caller builds its own independent shard-table markdown
//! rather than routing through `pr_comment::render_pr_report`
//! (pr-comment.md's "Template rendering" section flags rendering via
//! MiniJinja as `[unverified]` for `wasm32-unknown-unknown` pending its
//! own Phase 0 spike, and that cross-module wiring is bigger later
//! work once a real run's aggregate data is available in the right
//! shape) — this module remains a capability check for the wire calls
//! themselves, same as `github_app.rs` was for App-level auth.

use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq)]
pub struct GithubChecksError(String);

impl std::fmt::Display for GithubChecksError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for GithubChecksError {}

/// `X-GitHub-Api-Version` — same pinned value `github_app.rs` uses for
/// the `/app/*` endpoints (docs.github.com/en/rest/checks/runs and
/// docs.github.com/en/rest/issues/comments both document the same
/// version header, accessed 2026-10-02).
const GITHUB_API_VERSION: &str = "2022-11-28";

/// `User-Agent` GitHub requires on every REST API request
/// (docs.github.com/en/rest/using-the-rest-api/troubleshooting-the-rest-api#user-agent-required,
/// accessed 2026-10-02). `github_app.rs`'s module docs record that
/// omitting this header got a live `403` ("Please make sure your request
/// has a User-Agent header") rather than a GitHub-shaped auth failure —
/// carried over here so the same bug can't recur in this module.
const USER_AGENT: &str = "cloud-ci-worker";

/// The four headers every function in this module sends: a Bearer
/// **installation** access token (not an App-level JWT — see module
/// docs), the recommended `Accept`, the pinned [`GITHUB_API_VERSION`],
/// and the [`USER_AGENT`] GitHub requires on every REST API request.
/// Mirrors `github_app.rs`'s `app_jwt_headers` shape, consolidated into
/// one builder here since all five of this module's endpoints need
/// exactly this set.
fn installation_token_headers(installation_token: &str) -> [(&'static str, String); 4] {
    [
        ("authorization", format!("Bearer {installation_token}")),
        ("accept", "application/vnd.github+json".to_string()),
        ("x-github-api-version", GITHUB_API_VERSION.to_string()),
        ("user-agent", USER_AGENT.to_string()),
    ]
}

/// Builds a `worker::Headers` from [`installation_token_headers`] plus a
/// JSON `Content-Type`, for the four calls in this module that send a
/// JSON request body (every one except [`list_issue_comments`], which is
/// a bodyless `GET`).
fn json_request_headers(installation_token: &str) -> Result<worker::Headers, GithubChecksError> {
    let headers = worker::Headers::new();
    for (name, value) in installation_token_headers(installation_token) {
        headers
            .set(name, &value)
            .map_err(|e| GithubChecksError(format!("cannot set {name} header: {e}")))?;
    }
    headers
        .set("content-type", "application/json")
        .map_err(|e| GithubChecksError(format!("cannot set content-type header: {e}")))?;
    Ok(headers)
}

// --- Check Runs ------------------------------------------------------

/// `status` per docs.github.com/en/rest/checks/runs (accessed
/// 2026-10-02): the Create/Update Check Run endpoints document
/// `waiting`/`requested`/`pending` as additional values only GitHub
/// Actions can set ("Only GitHub Actions can set a status of waiting,
/// pending, or requested"); cloud-ci only ever needs the three generic
/// values a GitHub App can set, matching
/// [dynamic-pipelines.md § "GitHub status checks"](../../../docs/design/dynamic-pipelines.md)'s
/// `queued` (check created immediately) → `in_progress` → `completed`
/// lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRunStatus {
    Queued,
    InProgress,
    Completed,
}

/// `conclusion` per docs.github.com/en/rest/checks/runs (accessed
/// 2026-10-02): "Required if you provide completed_at or a status of
/// completed. ... You cannot change a check run conclusion to stale,
/// only GitHub can set this." cloud-ci never sends `Stale` as a request
/// value for that reason, but keeps the variant for deserializing a
/// response that carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRunConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    Stale,
}

/// `output` per docs.github.com/en/rest/checks/runs (accessed
/// 2026-10-02): "Check runs can accept a variety of data in the output
/// object" — trimmed to `title`/`summary`/`text`, the three fields
/// pr-comment.md's "Check Runs" section needs ("Check Run `output.summary`
/// holds the slice of the rendered comment relevant to that check").
/// `annotations`/`images` (failure line/column annotations) are a later
/// round's work once the renderer exists to produce them.
#[derive(Debug, Clone, Serialize)]
pub struct CheckRunOutput {
    pub title: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// `POST /repos/{owner}/{repo}/check-runs` request body
/// (docs.github.com/en/rest/checks/runs#create-a-check-run, accessed
/// 2026-10-02): `name` and `head_sha` are the only required fields;
/// `status` defaults to `queued` server-side if omitted, matching
/// dynamic-pipelines.md's "creates the check run immediately (`queued`)".
#[derive(Debug, Clone, Serialize)]
pub struct CreateCheckRunRequest {
    pub name: String,
    pub head_sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<CheckRunStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<CheckRunConclusion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<CheckRunOutput>,
}

/// `PATCH /repos/{owner}/{repo}/check-runs/{check_run_id}` request body
/// (docs.github.com/en/rest/checks/runs#update-a-check-run, accessed
/// 2026-10-02): every field is optional ("Providing conclusion will
/// automatically set the status parameter to completed"); unlike
/// [`CreateCheckRunRequest`] there is no `head_sha` field — the endpoint
/// does not accept one, since a check run's commit is fixed at creation.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateCheckRunRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<CheckRunStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<CheckRunConclusion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<CheckRunOutput>,
}

/// A created/updated check run, trimmed from the full documented
/// response (docs.github.com/en/rest/checks/runs#create-a-check-run,
/// accessed 2026-10-02 — the real response also carries `node_id`,
/// `url`, `check_suite`, `app`, `pull_requests`, ...) to the fields a
/// caller needs: `id` (to PATCH it later), plus `status`/`conclusion` to
/// confirm what GitHub actually stored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CheckRun {
    pub id: u64,
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
}

/// `POST /repos/{owner}/{repo}/check-runs`'s URL — pure string
/// construction, unit-testable without the Workers runtime.
fn check_runs_url(owner: &str, repo: &str) -> String {
    format!("https://api.github.com/repos/{owner}/{repo}/check-runs")
}

/// `PATCH /repos/{owner}/{repo}/check-runs/{check_run_id}`'s URL, with
/// `check_run_id` interpolated — pure string construction, unit-testable
/// without the Workers runtime.
fn check_run_url(owner: &str, repo: &str, check_run_id: u64) -> String {
    format!("https://api.github.com/repos/{owner}/{repo}/check-runs/{check_run_id}")
}

/// `POST /repos/{owner}/{repo}/check-runs`
/// (docs.github.com/en/rest/checks/runs#create-a-check-run, accessed
/// 2026-10-02): creates a new check run, used for `ci.check(name, opts)`
/// (dynamic-pipelines.md's "creates the check run immediately
/// (`queued`)") and for an external run's first-seen `--check <name>`
/// (byo-ci.md).
///
/// Not live-verified against GitHub — see module docs.
pub async fn create_check_run(
    installation_token: &str,
    owner: &str,
    repo: &str,
    request: &CreateCheckRunRequest,
) -> Result<CheckRun, GithubChecksError> {
    let url = check_runs_url(owner, repo);
    let headers = json_request_headers(installation_token)?;
    let body = serde_json::to_string(request)
        .map_err(|e| GithubChecksError(format!("cannot encode create check run body: {e}")))?;

    let mut init = worker::RequestInit::new();
    init.with_method(worker::Method::Post);
    init.with_headers(headers);
    init.with_body(Some(wasm_bindgen::JsValue::from_str(&body)));

    let request = worker::Request::new_with_init(&url, &init)
        .map_err(|e| GithubChecksError(format!("cannot build create check run request: {e}")))?;

    let mut response = worker::Fetch::Request(request)
        .send()
        .await
        .map_err(|e| GithubChecksError(format!("create check run request failed: {e}")))?;

    if response.status_code() != 201 {
        let body = response.text().await.unwrap_or_default();
        return Err(GithubChecksError(format!(
            "create check run failed: {} {body}",
            response.status_code()
        )));
    }

    response
        .json::<CheckRun>()
        .await
        .map_err(|e| GithubChecksError(format!("cannot decode create check run response: {e}")))
}

/// `PATCH /repos/{owner}/{repo}/check-runs/{check_run_id}`
/// (docs.github.com/en/rest/checks/runs#update-a-check-run, accessed
/// 2026-10-02): moves a check through `queued` → `in_progress` →
/// `completed` as its attached nodes report, and seals it (see
/// dynamic-pipelines.md's "Check sealing") with a final `conclusion`.
///
/// Not live-verified against GitHub — see module docs.
pub async fn update_check_run(
    installation_token: &str,
    owner: &str,
    repo: &str,
    check_run_id: u64,
    request: &UpdateCheckRunRequest,
) -> Result<CheckRun, GithubChecksError> {
    let url = check_run_url(owner, repo, check_run_id);
    let headers = json_request_headers(installation_token)?;
    let body = serde_json::to_string(request)
        .map_err(|e| GithubChecksError(format!("cannot encode update check run body: {e}")))?;

    let mut init = worker::RequestInit::new();
    init.with_method(worker::Method::Patch);
    init.with_headers(headers);
    init.with_body(Some(wasm_bindgen::JsValue::from_str(&body)));

    let request = worker::Request::new_with_init(&url, &init)
        .map_err(|e| GithubChecksError(format!("cannot build update check run request: {e}")))?;

    let mut response = worker::Fetch::Request(request)
        .send()
        .await
        .map_err(|e| GithubChecksError(format!("update check run request failed: {e}")))?;

    if response.status_code() != 200 {
        let body = response.text().await.unwrap_or_default();
        return Err(GithubChecksError(format!(
            "update check run failed: {} {body}",
            response.status_code()
        )));
    }

    response
        .json::<CheckRun>()
        .await
        .map_err(|e| GithubChecksError(format!("cannot decode update check run response: {e}")))
}

// --- Issue (PR) comments ----------------------------------------------

/// GitHub's documented max per page for `GET
/// /repos/{owner}/{repo}/issues/{issue_number}/comments`
/// (docs.github.com/en/rest/issues/comments#list-issue-comments, accessed
/// 2026-10-02): "The number of results per page (max 100)." Matches
/// pr-comment.md's "Marker-based upsert" step 3: "list ...
/// `?per_page=100` across pages."
const ISSUE_COMMENTS_PER_PAGE: u32 = 100;

/// An issue (PR) comment, trimmed from the full documented response
/// (docs.github.com/en/rest/issues/comments#list-issue-comments, accessed
/// 2026-10-02 — the real response also carries `node_id`, `url`, `user`,
/// `created_at`, `updated_at`, `author_association`,
/// `performed_via_github_app`, `reactions`, ...) to `id` (to PATCH it
/// later or delete a duplicate — pr-comment.md step 3: "If several of
/// ours are found ... adopt the oldest and delete the rest") and `body`
/// (to run [`find_sticky_comment_index`] against). `body` is documented
/// as an optional string (not present under some `Accept` media types);
/// this module always requests the default `application/vnd.github+json`
/// media type, which always carries it, so a missing `body` is treated
/// as empty rather than a parse failure.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    #[serde(default)]
    pub body: String,
}

/// `GET /repos/{owner}/{repo}/issues/{issue_number}/comments`'s URL for
/// one page, with `issue_number`/`page` interpolated — pure string
/// construction, unit-testable without the Workers runtime. Same
/// `page`/`per_page` pagination mechanism `github_app.rs`'s
/// `list_installations_url` uses (see that function's doc comment for
/// why this endpoint's own documented parameters are `page`/`per_page`,
/// not a `Link` header).
fn issue_comments_page_url(owner: &str, repo: &str, issue_number: u64, page: u32) -> String {
    format!(
        "https://api.github.com/repos/{owner}/{repo}/issues/{issue_number}/comments?per_page={ISSUE_COMMENTS_PER_PAGE}&page={page}"
    )
}

/// `POST /repos/{owner}/{repo}/issues/{issue_number}/comments`'s and the
/// per-page `GET`'s shared base URL, with `issue_number` interpolated —
/// pure string construction, unit-testable without the Workers runtime.
fn issue_comments_url(owner: &str, repo: &str, issue_number: u64) -> String {
    format!("https://api.github.com/repos/{owner}/{repo}/issues/{issue_number}/comments")
}

/// `PATCH /repos/{owner}/{repo}/issues/comments/{comment_id}`'s URL, with
/// `comment_id` interpolated — pure string construction, unit-testable
/// without the Workers runtime.
fn issue_comment_url(owner: &str, repo: &str, comment_id: u64) -> String {
    format!("https://api.github.com/repos/{owner}/{repo}/issues/comments/{comment_id}")
}

/// `GET /repos/{owner}/{repo}/issues/{issue_number}/comments`, paginated
/// to completion (docs.github.com/en/rest/issues/comments#list-issue-comments,
/// accessed 2026-10-02): pr-comment.md's "Marker-based upsert" step 3
/// fallback path, used when `pr_comments.comment_id` is unknown (first
/// render, or D1 lost the row). Same pagination-to-completion shape as
/// `github_app.rs`'s `list_installations`: walks pages until a short (or
/// empty) page comes back.
///
/// Not live-verified against GitHub — see module docs.
pub async fn list_issue_comments(
    installation_token: &str,
    owner: &str,
    repo: &str,
    issue_number: u64,
) -> Result<Vec<IssueComment>, GithubChecksError> {
    let headers = worker::Headers::new();
    for (name, value) in installation_token_headers(installation_token) {
        headers
            .set(name, &value)
            .map_err(|e| GithubChecksError(format!("cannot set {name} header: {e}")))?;
    }

    let mut comments = Vec::new();
    let mut page = 1u32;
    loop {
        let url = issue_comments_page_url(owner, repo, issue_number, page);
        let mut init = worker::RequestInit::new();
        init.with_method(worker::Method::Get);
        init.with_headers(headers.clone());

        let request = worker::Request::new_with_init(&url, &init).map_err(|e| {
            GithubChecksError(format!("cannot build list issue comments request: {e}"))
        })?;

        let mut response = worker::Fetch::Request(request)
            .send()
            .await
            .map_err(|e| GithubChecksError(format!("list issue comments request failed: {e}")))?;

        if response.status_code() != 200 {
            let body = response.text().await.unwrap_or_default();
            return Err(GithubChecksError(format!(
                "list issue comments failed: {} {body}",
                response.status_code()
            )));
        }

        let page_comments = response.json::<Vec<IssueComment>>().await.map_err(|e| {
            GithubChecksError(format!("cannot decode list issue comments response: {e}"))
        })?;
        let page_len = page_comments.len();
        comments.extend(page_comments);

        if page_len < ISSUE_COMMENTS_PER_PAGE as usize {
            break;
        }
        page += 1;
    }

    Ok(comments)
}

/// `POST /repos/{owner}/{repo}/issues/{issue_number}/comments`
/// (docs.github.com/en/rest/issues/comments#create-an-issue-comment,
/// accessed 2026-10-02): creates the sticky comment the first time a PR
/// needs one (pr-comment.md's "Marker-based upsert" step 3: "If none is
/// found, `POST` ..."). `body` is the rendered markdown, including the
/// leading `<!-- cloud-ci:pr-comment v1 repo=<repo_id> pr=<number> -->`
/// marker ([`sticky_comment_marker`]) — this function does not add the
/// marker itself, since templating/rendering is out of scope this round
/// (see module docs).
///
/// Not live-verified against GitHub — see module docs.
pub async fn create_issue_comment(
    installation_token: &str,
    owner: &str,
    repo: &str,
    issue_number: u64,
    body: &str,
) -> Result<IssueComment, GithubChecksError> {
    let url = issue_comments_url(owner, repo, issue_number);
    let headers = json_request_headers(installation_token)?;
    let json_body = serde_json::to_string(&IssueCommentBody { body })
        .map_err(|e| GithubChecksError(format!("cannot encode create issue comment body: {e}")))?;

    let mut init = worker::RequestInit::new();
    init.with_method(worker::Method::Post);
    init.with_headers(headers);
    init.with_body(Some(wasm_bindgen::JsValue::from_str(&json_body)));

    let request = worker::Request::new_with_init(&url, &init).map_err(|e| {
        GithubChecksError(format!("cannot build create issue comment request: {e}"))
    })?;

    let mut response = worker::Fetch::Request(request)
        .send()
        .await
        .map_err(|e| GithubChecksError(format!("create issue comment request failed: {e}")))?;

    if response.status_code() != 201 {
        let body = response.text().await.unwrap_or_default();
        return Err(GithubChecksError(format!(
            "create issue comment failed: {} {body}",
            response.status_code()
        )));
    }

    response
        .json::<IssueComment>()
        .await
        .map_err(|e| GithubChecksError(format!("cannot decode create issue comment response: {e}")))
}

/// `PATCH /repos/{owner}/{repo}/issues/comments/{comment_id}`
/// (docs.github.com/en/rest/issues/comments#update-an-issue-comment,
/// accessed 2026-10-02): edits the sticky comment in place for the life
/// of the PR's current head sha (pr-comment.md's "Marker-based upsert"
/// step 1, the common case once `comment_id` is known).
///
/// Not live-verified against GitHub — see module docs.
pub async fn update_issue_comment(
    installation_token: &str,
    owner: &str,
    repo: &str,
    comment_id: u64,
    body: &str,
) -> Result<IssueComment, GithubChecksError> {
    let url = issue_comment_url(owner, repo, comment_id);
    let headers = json_request_headers(installation_token)?;
    let json_body = serde_json::to_string(&IssueCommentBody { body })
        .map_err(|e| GithubChecksError(format!("cannot encode update issue comment body: {e}")))?;

    let mut init = worker::RequestInit::new();
    init.with_method(worker::Method::Patch);
    init.with_headers(headers);
    init.with_body(Some(wasm_bindgen::JsValue::from_str(&json_body)));

    let request = worker::Request::new_with_init(&url, &init).map_err(|e| {
        GithubChecksError(format!("cannot build update issue comment request: {e}"))
    })?;

    let mut response = worker::Fetch::Request(request)
        .send()
        .await
        .map_err(|e| GithubChecksError(format!("update issue comment request failed: {e}")))?;

    if response.status_code() != 200 {
        let body = response.text().await.unwrap_or_default();
        return Err(GithubChecksError(format!(
            "update issue comment failed: {} {body}",
            response.status_code()
        )));
    }

    response
        .json::<IssueComment>()
        .await
        .map_err(|e| GithubChecksError(format!("cannot decode update issue comment response: {e}")))
}

/// `{"body": ...}` — the identical request shape both
/// [`create_issue_comment`] and [`update_issue_comment`] send (GitHub
/// documents `body` as the sole request field for both endpoints).
#[derive(Debug, Clone, Serialize)]
struct IssueCommentBody<'a> {
    body: &'a str,
}

/// The hidden HTML marker pr-comment.md's "Comment layout (mockup)" and
/// "Marker-based upsert" sections document as the first line of every
/// sticky comment's body: `<!-- cloud-ci:pr-comment v1 repo=<repo_id>
/// pr=<number> -->`.
pub fn sticky_comment_marker(repo_id: u64, pr_number: u64) -> String {
    format!("<!-- cloud-ci:pr-comment v1 repo={repo_id} pr={pr_number} -->")
}

/// The pure string-matching half of pr-comment.md's "Marker-based
/// upsert" step 3: "Adopt the first comment whose body starts with the
/// `cloud-ci:pr-comment` marker." Given `comment_bodies` (in the order
/// [`list_issue_comments`] returns them — ascending id, per GitHub's
/// "Issue comments are ordered by ascending ID") and the `(repo_id,
/// pr_number)` this PR's marker must carry, returns the index of the
/// first body starting with [`sticky_comment_marker`]`(repo_id,
/// pr_number)`, or `None` if no body matches.
///
/// Deliberately takes only bodies, not full [`IssueComment`]s: the
/// doc's other adoption condition (`performed_via_github_app.id` must
/// equal our App id, "Comments carrying our marker from any other author
/// are ignored, since anyone can paste the marker") needs fields this
/// function doesn't have — the caller applies that filter itself once it
/// has the real [`IssueComment`] list, using this function only to
/// narrow by content first.
pub fn find_sticky_comment_index(
    comment_bodies: &[String],
    repo_id: u64,
    pr_number: u64,
) -> Option<usize> {
    let marker = sticky_comment_marker(repo_id, pr_number);
    comment_bodies
        .iter()
        .position(|body| body.starts_with(&marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_runs_url_interpolates_owner_and_repo() {
        assert_eq!(
            check_runs_url("acme", "web"),
            "https://api.github.com/repos/acme/web/check-runs"
        );
    }

    #[test]
    fn check_run_url_interpolates_check_run_id() {
        assert_eq!(
            check_run_url("acme", "web", 9001),
            "https://api.github.com/repos/acme/web/check-runs/9001"
        );
    }

    #[test]
    fn installation_token_headers_carry_bearer_accept_version_and_user_agent() {
        let headers = installation_token_headers("ghs_abc123");
        assert_eq!(
            headers[0],
            ("authorization", "Bearer ghs_abc123".to_string())
        );
        assert_eq!(
            headers[1],
            ("accept", "application/vnd.github+json".to_string())
        );
        assert_eq!(
            headers[2],
            ("x-github-api-version", GITHUB_API_VERSION.to_string())
        );
        assert_eq!(headers[3], ("user-agent", USER_AGENT.to_string()));
    }

    #[test]
    fn create_check_run_request_omits_absent_optional_fields() -> Result<(), serde_json::Error> {
        let request = CreateCheckRunRequest {
            name: "web/build".to_string(),
            head_sha: "def5678".to_string(),
            status: Some(CheckRunStatus::Queued),
            conclusion: None,
            details_url: None,
            output: None,
        };
        let json = serde_json::to_value(&request)?;
        assert_eq!(json["name"], "web/build");
        assert_eq!(json["head_sha"], "def5678");
        assert_eq!(json["status"], "queued");
        assert!(json.get("conclusion").is_none());
        assert!(json.get("details_url").is_none());
        assert!(json.get("output").is_none());
        Ok(())
    }

    #[test]
    fn create_check_run_request_serializes_completed_conclusion_and_output()
    -> Result<(), serde_json::Error> {
        let request = CreateCheckRunRequest {
            name: "web/build".to_string(),
            head_sha: "def5678".to_string(),
            status: Some(CheckRunStatus::Completed),
            conclusion: Some(CheckRunConclusion::Failure),
            details_url: Some("https://ci.example.com/acme/web/pull/412".to_string()),
            output: Some(CheckRunOutput {
                title: "2 failed".to_string(),
                summary: "2 failed, 14 passed".to_string(),
                text: None,
            }),
        };
        let json = serde_json::to_value(&request)?;
        assert_eq!(json["status"], "completed");
        assert_eq!(json["conclusion"], "failure");
        assert_eq!(
            json["details_url"],
            "https://ci.example.com/acme/web/pull/412"
        );
        assert_eq!(json["output"]["title"], "2 failed");
        assert_eq!(json["output"]["summary"], "2 failed, 14 passed");
        assert!(json["output"].get("text").is_none());
        Ok(())
    }

    #[test]
    fn check_run_conclusion_variants_serialize_to_documented_snake_case_strings()
    -> Result<(), serde_json::Error> {
        // docs.github.com/en/rest/checks/runs#update-a-check-run's documented
        // enum: action_required, cancelled, failure, neutral, success,
        // skipped, stale, timed_out.
        let cases = [
            (CheckRunConclusion::Success, "success"),
            (CheckRunConclusion::Failure, "failure"),
            (CheckRunConclusion::Neutral, "neutral"),
            (CheckRunConclusion::Cancelled, "cancelled"),
            (CheckRunConclusion::Skipped, "skipped"),
            (CheckRunConclusion::TimedOut, "timed_out"),
            (CheckRunConclusion::ActionRequired, "action_required"),
            (CheckRunConclusion::Stale, "stale"),
        ];
        for (variant, expected) in cases {
            assert_eq!(serde_json::to_value(variant)?, expected);
        }
        Ok(())
    }

    #[test]
    fn update_check_run_request_has_no_head_sha_field() -> Result<(), serde_json::Error> {
        let request = UpdateCheckRunRequest {
            status: Some(CheckRunStatus::InProgress),
            ..Default::default()
        };
        let json = serde_json::to_value(&request)?;
        assert!(
            json.get("head_sha").is_none(),
            "update endpoint does not accept head_sha"
        );
        assert_eq!(json["status"], "in_progress");
        Ok(())
    }

    #[test]
    fn check_run_response_parses_documented_shape() -> Result<(), serde_json::Error> {
        // Trimmed from docs.github.com/en/rest/checks/runs
        // #create-a-check-run (extra fields like `node_id`/`app`/
        // `check_suite`/`pull_requests` are present on the real response
        // and must be ignored, not rejected).
        let body = r#"{
            "id": 4,
            "head_sha": "ce587453ced02b1526dfb4cb910479d431683101",
            "node_id": "MDg6Q2hlY2tSdW40",
            "external_id": "42",
            "url": "https://api.github.com/repos/github/hello-world/check-runs/4",
            "html_url": "https://github.com/github/hello-world/runs/4",
            "details_url": "https://example.com",
            "status": "in_progress",
            "conclusion": null,
            "started_at": "2018-05-04T01:14:52Z",
            "completed_at": null,
            "name": "mighty_readme",
            "check_suite": { "id": 5 },
            "app": null,
            "pull_requests": []
        }"#;
        let parsed: CheckRun = serde_json::from_str(body)?;
        assert_eq!(parsed.id, 4);
        assert_eq!(parsed.status, "in_progress");
        assert_eq!(parsed.conclusion, None);
        Ok(())
    }

    #[test]
    fn check_run_response_parses_completed_conclusion() -> Result<(), serde_json::Error> {
        let body = r#"{
            "id": 4,
            "head_sha": "ce587453ced02b1526dfb4cb910479d431683101",
            "status": "completed",
            "conclusion": "success",
            "name": "mighty_readme"
        }"#;
        let parsed: CheckRun = serde_json::from_str(body)?;
        assert_eq!(parsed.status, "completed");
        assert_eq!(parsed.conclusion, Some("success".to_string()));
        Ok(())
    }

    #[test]
    fn issue_comments_page_url_carries_issue_number_page_and_max_per_page() {
        assert_eq!(
            issue_comments_page_url("acme", "web", 412, 1),
            "https://api.github.com/repos/acme/web/issues/412/comments?per_page=100&page=1"
        );
        assert_eq!(
            issue_comments_page_url("acme", "web", 412, 3),
            "https://api.github.com/repos/acme/web/issues/412/comments?per_page=100&page=3"
        );
    }

    #[test]
    fn issue_comments_url_interpolates_issue_number() {
        assert_eq!(
            issue_comments_url("acme", "web", 412),
            "https://api.github.com/repos/acme/web/issues/412/comments"
        );
    }

    #[test]
    fn issue_comment_url_interpolates_comment_id() {
        assert_eq!(
            issue_comment_url("acme", "web", 55667788),
            "https://api.github.com/repos/acme/web/issues/comments/55667788"
        );
    }

    #[test]
    fn issue_comment_response_parses_documented_shape() -> Result<(), serde_json::Error> {
        // Trimmed from docs.github.com/en/rest/issues/comments
        // #list-issue-comments (extra fields like `node_id`/`user`/
        // `created_at`/`author_association`/`reactions` are present on
        // the real response and must be ignored, not rejected).
        let body = r#"{
            "id": 1,
            "node_id": "MDEyOklzc3VlQ29tbWVudDE=",
            "url": "https://api.github.com/repos/octocat/Hello-World/issues/comments/1",
            "html_url": "https://github.com/octocat/Hello-World/issues/1347#issuecomment-1",
            "body": "<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->\n### cloud-ci: running",
            "user": { "login": "octocat", "id": 1 },
            "created_at": "2011-04-14T16:00:49Z",
            "updated_at": "2011-04-14T16:00:49Z",
            "issue_url": "https://api.github.com/repos/octocat/Hello-World/issues/1347",
            "author_association": "COLLABORATOR"
        }"#;
        let parsed: IssueComment = serde_json::from_str(body)?;
        assert_eq!(parsed.id, 1);
        assert!(
            parsed
                .body
                .starts_with("<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->")
        );
        Ok(())
    }

    #[test]
    fn issue_comment_response_defaults_missing_body_to_empty() -> Result<(), serde_json::Error> {
        // `body` is documented as not required (some media types omit it).
        let body = r#"{ "id": 2 }"#;
        let parsed: IssueComment = serde_json::from_str(body)?;
        assert_eq!(parsed.id, 2);
        assert_eq!(parsed.body, "");
        Ok(())
    }

    #[test]
    fn issue_comment_body_serializes_only_body_field() -> Result<(), serde_json::Error> {
        let payload = IssueCommentBody { body: "Me too" };
        let json = serde_json::to_value(&payload)?;
        assert_eq!(json, serde_json::json!({ "body": "Me too" }));
        Ok(())
    }

    #[test]
    fn sticky_comment_marker_matches_pr_comment_md_mockup_format() {
        assert_eq!(
            sticky_comment_marker(8812, 412),
            "<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->"
        );
    }

    #[test]
    fn find_sticky_comment_index_finds_marker_among_decoys() {
        let bodies = vec![
            "Looks good to me!".to_string(),
            "<!-- renovate-comment-id:123 -->\nUpdate dependency foo".to_string(),
            "<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->\n### cloud-ci: 2 failed".to_string(),
            "Thanks for the review".to_string(),
        ];
        assert_eq!(find_sticky_comment_index(&bodies, 8812, 412), Some(2));
    }

    #[test]
    fn find_sticky_comment_index_returns_none_when_no_match() {
        let bodies = vec![
            "Looks good to me!".to_string(),
            "<!-- renovate-comment-id:123 -->\nUpdate dependency foo".to_string(),
        ];
        assert_eq!(find_sticky_comment_index(&bodies, 8812, 412), None);
    }

    #[test]
    fn find_sticky_comment_index_distinguishes_by_repo_id_and_pr_number() {
        // Two cloud-ci comments present, but for different PRs/repos than
        // the one being searched for — must not false-positive on either.
        let bodies = vec![
            "<!-- cloud-ci:pr-comment v1 repo=8812 pr=99 -->\n### cloud-ci: running".to_string(),
            "<!-- cloud-ci:pr-comment v1 repo=9999 pr=412 -->\n### cloud-ci: running".to_string(),
        ];
        assert_eq!(find_sticky_comment_index(&bodies, 8812, 412), None);

        let bodies_with_match = vec![
            "<!-- cloud-ci:pr-comment v1 repo=8812 pr=99 -->\n### cloud-ci: running".to_string(),
            "<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->\n### cloud-ci: running".to_string(),
            "<!-- cloud-ci:pr-comment v1 repo=9999 pr=412 -->\n### cloud-ci: running".to_string(),
        ];
        assert_eq!(
            find_sticky_comment_index(&bodies_with_match, 8812, 412),
            Some(1)
        );
    }

    #[test]
    fn find_sticky_comment_index_requires_marker_at_start_not_merely_contained() {
        // pr-comment.md: "Adopt the first comment whose body *starts
        // with* the cloud-ci:pr-comment marker" — a comment that merely
        // quotes the marker partway through (e.g. someone pasting it in
        // a bug report) must not be adopted.
        let bodies = vec![
            "I think the bot's marker looks like this: <!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->"
                .to_string(),
        ];
        assert_eq!(find_sticky_comment_index(&bodies, 8812, 412), None);
    }

    #[test]
    fn find_sticky_comment_index_picks_first_match_when_several_present() {
        // pr-comment.md step 3: "If several of ours are found (an
        // earlier race), adopt the oldest" — list order is ascending id
        // (oldest first), so "first match" is "oldest".
        let bodies = vec![
            "<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->\n### cloud-ci: running (oldest)"
                .to_string(),
            "<!-- cloud-ci:pr-comment v1 repo=8812 pr=412 -->\n### cloud-ci: running (newer)"
                .to_string(),
        ];
        assert_eq!(find_sticky_comment_index(&bodies, 8812, 412), Some(0));
    }
}
