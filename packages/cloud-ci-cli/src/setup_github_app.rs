//! Orchestrates `cloud-ci setup github-app --allowed-orgs <logins>
//! --deployment-url <url> [--public] --cloudflare-account-id <id>
//! --secrets-store-id <id>`, the GitHub App manifest flow sketched in
//! `docs/design/auth.md`'s "### GitHub App setup" sequence diagram.
//!
//! # Scope: everything except the two genuinely un-drivable steps
//!
//! Every step of the diagram is implemented for real here *except* the two
//! that inherently need a human clicking a button in their own browser:
//!
//! - `Op->>T: open browser to github.com/settings/apps/new with manifest`
//!   — this module genuinely opens a browser (see [`open_in_browser`]), but
//!   nothing afterward is observable from this process until the loopback
//!   listener gets a request.
//! - `Op->>GH: confirm app creation` — the operator approving app creation
//!   on a real `github.com` page in a real browser session. No test in
//!   this module (or anywhere in this environment, which has no real
//!   GitHub browser session) exercises this; [`LoopbackListener::wait_for_code`]'s
//!   tests drive the *next* step (GitHub's redirect delivering `?code=...`)
//!   with a synthetic socket connection standing in for the browser.
//!
//! Every other step — manifest construction, the loopback listener and its
//! callback parsing, the GitHub manifest-conversion request/response, the
//! Cloudflare Secrets Store request/response, `CLOUD_CI_MASTER_KEY`
//! generation, and the `wrangler.toml` writer — is real and unit-tested.
//!
//! # Three layers, same split as `oauth.rs`/`github_app.rs` (cloud-ci-worker)
//!
//! 1. [`build_manifest`], [`build_manifest_form_html`], [`parse_callback_head`],
//!    [`generate_master_key`], [`mutate_wrangler_for_github_app`] — pure
//!    construction/parsing, no network. Unit-testable with plain `cargo test`.
//! 2. [`convert_manifest`], [`create_secrets_store_secret`] — real HTTP
//!    request/response construction against GitHub's manifest-conversion
//!    endpoint and Cloudflare's Secrets Store API. **Not live-verified**:
//!    this environment has no real manifest-flow `code` and no real
//!    Cloudflare Secrets-Store-scoped API token to drive either call
//!    against the real service (same category of limitation as
//!    `oauth.rs::exchange_code`/`github_app.rs::fetch_installation_token`
//!    — see those modules' docs for the precedent). Both functions take a
//!    `base_url` parameter precisely so tests can point them at a local
//!    HTTP fixture server instead (same pattern as `upload.rs`'s
//!    `full_upload_sequence_executes_against_a_real_http_fixture`); their
//!    request construction and response-shape parsing are proven that way.
//! 3. [`LoopbackListener`] — a real `127.0.0.1` TCP listener. Binding it
//!    and parsing a synthetic `GET /callback?code=...` delivered over a
//!    real socket are both tested; the thing that would normally *drive*
//!    that socket (a browser following GitHub's 302) is not.
//! 4. [`open_in_browser`] — a real, OS-dispatched "open this URL/file in
//!    the default browser" call (`open`/`xdg-open`/`cmd /C start`, no new
//!    dependency — no browser-opening crate was already vendored anywhere
//!    in this workspace, and the three-way OS dispatch is small enough
//!    that pulling in a crate like `webbrowser` for it isn't worth a new
//!    dependency). Genuinely launches a browser when run for real; no test
//!    here observes anything past the `Command::spawn` call succeeding,
//!    since what happens next is exactly the un-drivable human-approval
//!    step above.
//!
//! [`run_github_app`] wires all of this into the full sequence: build the
//! manifest, start the loopback listener, write+open a local HTML page that
//! auto-submits the manifest to GitHub (GitHub's documented manifest-flow
//! mechanism — a POST, not a GET with the manifest as a query string, which
//! would overflow typical URL length limits for this manifest's size), wait
//! for the callback (one-hour GitHub-side window per auth.md; this CLI
//! defaults to a shorter, CLI-practical 10 minutes via `--timeout-secs`),
//! convert the code, mint `CLOUD_CI_MASTER_KEY`, create the four Secrets
//! Store secrets, write `wrangler.toml`, and (only with `--deploy`, same
//! opt-in convention `setup.rs::run_allowed_orgs` already established) run
//! `wrangler deploy`.

use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command as ProcessCommand;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::cli::GithubAppArgs;

#[derive(Debug)]
pub struct GithubAppSetupError {
    message: String,
}

impl GithubAppSetupError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for GithubAppSetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GithubAppSetupError {}

// ---------------------------------------------------------------------------
// Layer 1: pure manifest construction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookAttributes {
    pub url: String,
}

/// The default permission set from `docs/design/auth.md`'s "GitHub App
/// setup" permission table — fixed, not configurable by a flag: the task
/// this command does is register *this* deployment's App, which only ever
/// needs these permissions (Contents: write is a separate, autofix-only
/// opt-in per that doc, not part of this default manifest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DefaultPermissions {
    pub contents: &'static str,
    pub checks: &'static str,
    pub pull_requests: &'static str,
    pub issues: &'static str,
    pub metadata: &'static str,
    pub statuses: &'static str,
    pub actions: &'static str,
}

const DEFAULT_PERMISSIONS: DefaultPermissions = DefaultPermissions {
    contents: "read",
    checks: "write",
    pull_requests: "write",
    issues: "write",
    metadata: "read",
    statuses: "write",
    actions: "read",
};

/// The default event subscription list from the same table — fixed for the
/// same reason as [`DEFAULT_PERMISSIONS`].
const DEFAULT_EVENTS: &[&str] = &[
    "push",
    "pull_request",
    "check_suite",
    "check_run",
    "issue_comment",
    "installation",
    "installation_repositories",
    "workflow_run",
];

/// The exact manifest shape from auth.md's "### GitHub App setup" JSON
/// block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Manifest {
    pub name: String,
    pub url: String,
    pub hook_attributes: HookAttributes,
    pub redirect_url: String,
    pub public: bool,
    pub default_permissions: DefaultPermissions,
    pub default_events: Vec<&'static str>,
}

/// Builds the manifest for `app_name`/`deployment_url`, with `redirect_url`
/// pointed at the loopback listener's own `127.0.0.1:<redirect_port>`
/// address (auth.md: "redirect_url = loopback" — the `code` this address
/// receives never leaves the operator's machine).
///
/// `public` only ever comes from the caller's explicit `--public` flag
/// ([`GithubAppArgs::public`]) — this function (and this whole command)
/// never inspects the deployer's GitHub orgs to decide whether they share
/// one Enterprise account; auth.md's "Multiple orgs and installations"
/// section documents that call as the operator's own, and a wrong
/// auto-detected guess here would either needlessly expose a private App's
/// install surface or silently block a legitimate multi-Enterprise setup.
pub fn build_manifest(
    app_name: &str,
    deployment_url: &str,
    redirect_port: u16,
    public: bool,
) -> Manifest {
    let deployment_url = deployment_url.trim_end_matches('/');
    Manifest {
        name: app_name.to_string(),
        url: deployment_url.to_string(),
        hook_attributes: HookAttributes {
            url: format!("{deployment_url}/webhooks/github"),
        },
        redirect_url: format!("http://127.0.0.1:{redirect_port}/callback"),
        public,
        default_permissions: DEFAULT_PERMISSIONS,
        default_events: DEFAULT_EVENTS.to_vec(),
    }
}

/// Builds the local HTML page this command opens in the operator's
/// browser: an auto-submitting form `POST`ing `manifest_json` to
/// `github.com/settings/apps/new`, per GitHub's documented manifest flow
/// (docs.github.com/en/apps/sharing-github-apps/registering-a-github-app-from-a-manifest,
/// accessed 2026-09-30) — the manifest rides as a POST body field, not a
/// query string (this manifest's JSON is well past comfortable URL-length
/// limits once `hook_attributes.url`/`redirect_url` are filled in). Pure
/// string construction, unit-tested for the exact `action`/field-name/
/// manifest-value shape without needing a real browser.
pub fn build_manifest_form_html(manifest_json: &str) -> String {
    let escaped = manifest_json
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<!doctype html>\n\
         <html><head><title>cloud-ci setup github-app</title></head><body>\n\
         <form id=\"manifest-form\" action=\"https://github.com/settings/apps/new\" method=\"post\">\n\
         <input type=\"hidden\" name=\"manifest\" value=\"{escaped}\">\n\
         </form>\n\
         <script>document.getElementById(\"manifest-form\").submit();</script>\n\
         <p>Submitting the GitHub App manifest&hellip; if nothing happens, \
         <button onclick=\"document.getElementById('manifest-form').submit()\">click here</button>.</p>\n\
         </body></html>\n"
    )
}

/// 32 random bytes, hex-encoded (64 hex chars) — matches the format every
/// other string-valued secret in this codebase already uses when read back
/// via `env.secret(...).to_string().into_bytes()` (`ingest_token.rs`'s
/// `INGEST_TOKEN_SECRET`, `.dev.vars.example`'s plain-string secrets):
/// Cloudflare Secrets Store secrets are stored and read as opaque strings,
/// not raw binary, so `CLOUD_CI_MASTER_KEY` has to be *some* string
/// encoding of its random bytes. Hex (rather than base64) is chosen only
/// because it needs no padding/URL-safety handling and is trivially
/// copy-pasteable if an operator ever needs to read it back out of a
/// terminal; nothing downstream (the HKDF root in auth.md's "Token format"
/// section) cares which encoding produced the UTF-8 bytes it's keyed with.
pub fn generate_master_key() -> Result<String, GithubAppSetupError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| GithubAppSetupError::new(format!("RNG unavailable: {err}")))?;
    Ok(hex_encode(&bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Layer 2: GitHub manifest conversion + Cloudflare Secrets Store requests
// ---------------------------------------------------------------------------

const GITHUB_API_BASE_URL: &str = "https://api.github.com";
const GITHUB_API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "cloud-ci-cli";

/// Cloudflare's REST API base, `v4` (developers.cloudflare.com/api,
/// accessed 2026-10-02).
const CLOUDFLARE_API_BASE_URL: &str = "https://api.cloudflare.com/client/v4";

/// The fields this command needs out of GitHub's manifest-conversion
/// response (docs.github.com/en/rest/apps/apps#create-a-github-app-from-a-manifest,
/// accessed 2026-09-30) — the real response carries many more (`slug`,
/// `owner`, `permissions`, `events`, ...), trimmed to what `run_github_app`
/// actually consumes, same trimming convention `github_app.rs`'s
/// `ListedInstallation` already uses for a different GitHub response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManifestConversion {
    pub id: u64,
    pub pem: String,
    pub webhook_secret: String,
    pub client_id: String,
    pub client_secret: String,
}

/// `POST /app-manifests/{code}/conversions`
/// (docs.github.com/en/rest/apps/apps#create-a-github-app-from-a-manifest,
/// accessed 2026-09-30): exchanges the manifest flow's `code` for the
/// App's real credentials. Documented as needing no authentication beyond
/// the one-time `code` itself — GitHub treats possession of a freshly
/// redirected `code` as proof the caller is the same browser session that
/// just approved app creation.
///
/// `base_url` is a parameter (not a hardcoded `GITHUB_API_BASE_URL` call)
/// so tests can point this at a local HTTP fixture server instead of real
/// `api.github.com` — see this module's doc comment for why the live call
/// itself is not exercised here.
pub fn convert_manifest(
    base_url: &str,
    code: &str,
) -> Result<ManifestConversion, GithubAppSetupError> {
    let url = format!(
        "{}/app-manifests/{code}/conversions",
        base_url.trim_end_matches('/')
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();

    let mut response = agent
        .post(&url)
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", GITHUB_API_VERSION)
        .header("user-agent", USER_AGENT)
        .send_empty()
        .map_err(|err| {
            GithubAppSetupError::new(format!("github manifest conversion request failed: {err}"))
        })?;

    let status = response.status().as_u16();
    let body = response.body_mut().read_to_vec().map_err(|err| {
        GithubAppSetupError::new(format!("could not read github response body: {err}"))
    })?;

    if !(200..300).contains(&status) {
        // Request body (empty, per send_empty()) has no secret material
        // to echo back, so the raw response body is safe to surface here
        // — kept asymmetric with create_secrets_store_secret's redacted
        // error path below on purpose.
        return Err(GithubAppSetupError::new(format!(
            "github manifest conversion failed: http {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }

    serde_json::from_slice(&body).map_err(|err| {
        GithubAppSetupError::new(format!(
            "could not parse github manifest conversion response: {err}"
        ))
    })
}

#[derive(Debug, Clone, Serialize)]
struct SecretCreateRequestItem<'a> {
    name: &'a str,
    scopes: Vec<&'static str>,
    value: &'a str,
    comment: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
struct CloudflareApiError {
    code: i64,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SecretCreateResult {
    store_id: String,
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SecretCreateApiResponse {
    success: bool,
    #[serde(default)]
    errors: Vec<CloudflareApiError>,
    result: Option<Vec<SecretCreateResult>>,
}

/// One secret this command created, keyed back from Cloudflare's own
/// response (not just echoing what was requested) — `store_id`/`name` are
/// exactly the two fields `wrangler.toml`'s `[[secrets_store_secrets]]`
/// bindings need (auth.md's "### Deploy-time configuration" excerpt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSecret {
    pub store_id: String,
    pub secret_name: String,
}

/// `POST /accounts/{account_id}/secrets_store/stores/{store_id}/secrets`
/// (developers.cloudflare.com/api/resources/secrets_store/subresources/stores/subresources/secrets/methods/create,
/// accessed 2026-10-02): creates one secret with `scopes: ["workers"]`
/// (this binding is only ever read by a Worker, per auth.md's "Secret
/// storage" table) and `value` as its plaintext — "write only", per that
/// same API reference: the API never returns the value back.
///
/// `base_url`/`api_token` are parameters so tests can point this at a
/// local HTTP fixture server instead of real Cloudflare — see this
/// module's doc comment for why the live call itself is not exercised
/// here (no real Cloudflare API token with Secrets Store write access
/// exists in this environment).
pub fn create_secrets_store_secret(
    base_url: &str,
    account_id: &str,
    store_id: &str,
    api_token: &str,
    secret_name: &str,
    value: &str,
) -> Result<CreatedSecret, GithubAppSetupError> {
    let url = format!(
        "{}/accounts/{account_id}/secrets_store/stores/{store_id}/secrets",
        base_url.trim_end_matches('/'),
    );
    let body = vec![SecretCreateRequestItem {
        name: secret_name,
        scopes: vec!["workers"],
        value,
        comment: "created by cloud-ci setup github-app",
    }];
    let body_bytes = serde_json::to_vec(&body).map_err(|err| {
        GithubAppSetupError::new(format!("could not encode cloudflare request: {err}"))
    })?;

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();

    let mut response = agent
        .post(&url)
        .header("authorization", format!("Bearer {api_token}"))
        .header("content-type", "application/json")
        .send(body_bytes)
        .map_err(|err| {
            GithubAppSetupError::new(format!("cloudflare secrets_store request failed: {err}"))
        })?;

    let status = response.status().as_u16();
    let response_body = response.body_mut().read_to_vec().map_err(|err| {
        GithubAppSetupError::new(format!("could not read cloudflare response body: {err}"))
    })?;

    if !(200..300).contains(&status) {
        // Unlike convert_manifest's empty-body request, this request's
        // body carries the secret's plaintext `value`. Cloudflare's own
        // non-2xx validation errors can plausibly echo submitted request
        // fields back in the response body, so the raw body must never
        // be included in the error message — only Cloudflare's own
        // structured `code`/`message` fields (same as the `!success`
        // branch below), or, failing that, just the status code.
        if let Ok(parsed) = serde_json::from_slice::<SecretCreateApiResponse>(&response_body)
            && !parsed.errors.is_empty()
        {
            let messages: Vec<String> = parsed
                .errors
                .iter()
                .map(|e| format!("{}: {}", e.code, e.message))
                .collect();
            let formatted = format!(
                "cloudflare secrets_store create failed: http {status}: {}",
                messages.join(", ")
            );
            return Err(GithubAppSetupError::new(
                formatted.replace(value, "[REDACTED]"),
            ));
        }
        return Err(GithubAppSetupError::new(format!(
            "cloudflare secrets_store create failed: http {status} (response body omitted — may contain submitted secret material)"
        )));
    }

    let parsed: SecretCreateApiResponse =
        serde_json::from_slice(&response_body).map_err(|err| {
            GithubAppSetupError::new(format!(
                "could not parse cloudflare secrets_store response: {err}"
            ))
        })?;

    if !parsed.success {
        let messages: Vec<String> = parsed
            .errors
            .iter()
            .map(|e| format!("{}: {}", e.code, e.message))
            .collect();
        let formatted = format!(
            "cloudflare secrets_store create reported failure: {}",
            messages.join(", ")
        );
        return Err(GithubAppSetupError::new(
            formatted.replace(value, "[REDACTED]"),
        ));
    }

    let created = parsed
        .result
        .and_then(|results| results.into_iter().next())
        .ok_or_else(|| {
            GithubAppSetupError::new("cloudflare secrets_store response carried no result")
        })?;

    Ok(CreatedSecret {
        store_id: created.store_id,
        secret_name: created.name,
    })
}

// ---------------------------------------------------------------------------
// Layer 3: the real loopback listener
// ---------------------------------------------------------------------------

/// A real `127.0.0.1:<port>` TCP listener standing in for the manifest
/// flow's `redirect_url` (auth.md's sequence diagram, step "start loopback
/// listener").
pub struct LoopbackListener {
    listener: TcpListener,
    port: u16,
}

impl LoopbackListener {
    /// Binds `127.0.0.1:<port>`, or an OS-assigned ephemeral port when
    /// `port` is `None`/`Some(0)`.
    pub fn bind(port: Option<u16>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn redirect_url(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.port)
    }

    /// Blocks until a single `GET /callback?code=...` request arrives, or
    /// `timeout` elapses. `std::net::TcpListener::accept` has no
    /// timeout of its own, so the actual accept+parse runs on a helper
    /// thread and this call just waits on a channel with
    /// `recv_timeout` — on a timeout the helper thread is left blocked in
    /// `accept()` (intentionally leaked: this is a short-lived CLI
    /// process that exits shortly after either outcome, so there is
    /// nothing to clean the thread up for).
    pub fn wait_for_code(self, timeout: Duration) -> Result<String, GithubAppSetupError> {
        let listener = self.listener;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outcome = accept_one_callback(&listener);
            let _ = tx.send(outcome);
        });

        match rx.recv_timeout(timeout) {
            Ok(Ok(code)) => Ok(code),
            Ok(Err(message)) => Err(GithubAppSetupError::new(message)),
            Err(_) => Err(GithubAppSetupError::new(format!(
                "timed out after {}s waiting for the GitHub App manifest callback — the \
                 operator did not finish confirming app creation in the browser in time \
                 (GitHub's own window is one hour; re-run with a larger --timeout-secs if \
                 needed)",
                timeout.as_secs()
            ))),
        }
    }
}

fn accept_one_callback(listener: &TcpListener) -> Result<String, String> {
    let (mut stream, _) = listener.accept().map_err(|e| e.to_string())?;
    let head = read_request_head(&mut stream).map_err(|e| e.to_string())?;
    let code = parse_callback_head(&head).map_err(|e| e.to_string())?;
    write_confirmation_response(&mut stream).map_err(|e| e.to_string())?;
    Ok(code)
}

/// Reads raw bytes off `stream` up to the end of the HTTP request head
/// (`\r\n\r\n`) — minimal by design, matching `upload.rs`'s own test-fixture
/// `read_request` approach rather than pulling in a test-only HTTP
/// server/parser dependency. The callback is always a bare `GET` with no
/// body, so headers-only is the whole request.
fn read_request_head(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        // Guards against an unbounded read from a misbehaving peer; no real
        // browser GET request head comes close to this.
        if buf.len() > 64 * 1024 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Parses an HTTP request head's first line for GitHub's redirected
/// `code` query parameter (`GH->>Op: 302 to
/// http://127.0.0.1:<port>/callback?code=...` in auth.md's sequence
/// diagram). Pure, unit-testable without a socket.
fn parse_callback_head(head: &str) -> Result<String, GithubAppSetupError> {
    let request_line = head.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    if method != "GET" {
        return Err(GithubAppSetupError::new(format!(
            "expected a GET /callback request, got \"{method} {target}\""
        )));
    }

    let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
    for pair in query.split('&') {
        if let Some(code) = pair.strip_prefix("code=") {
            if code.is_empty() {
                return Err(GithubAppSetupError::new(
                    "callback request's code parameter was empty",
                ));
            }
            return Ok(code.to_string());
        }
    }

    Err(GithubAppSetupError::new(format!(
        "callback request \"{target}\" carried no code parameter — GitHub may have rejected \
         the manifest, or the operator canceled app creation"
    )))
}

fn write_confirmation_response(stream: &mut TcpStream) -> std::io::Result<()> {
    let body = b"<html><body>cloud-ci setup github-app: code received, continuing in your \
                 terminal. You can close this tab.</body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len(),
    );
    stream.write_all(response.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// Layer 4: opening a real browser — genuinely un-drivable past this call
// ---------------------------------------------------------------------------

/// Opens `path_or_url` in the operator's default browser: `open` on macOS,
/// `xdg-open` on Linux, `cmd /C start` on Windows. No new dependency — no
/// browser-opening crate (e.g. `webbrowser`) was already vendored anywhere
/// in this workspace (checked: nothing in any `Cargo.lock` in the repo),
/// and this three-way OS dispatch is small enough that adding one just for
/// this single call isn't worth it, same posture `setup.rs` already takes
/// toward `wrangler deploy` (a plain `std::process::Command` spawn, not a
/// wrapper crate).
///
/// This genuinely launches a real browser when run for real. Nothing in
/// this module's tests calls it: what it does is launch a process and
/// return — there is no portable, sandboxed-CI-safe way to verify a real
/// GUI browser actually opened and rendered the page, and what happens
/// after it opens (the operator approving app creation) is this module's
/// documented out-of-scope boundary regardless.
pub fn open_in_browser(path_or_url: &str) -> std::io::Result<std::process::ExitStatus> {
    if cfg!(target_os = "macos") {
        ProcessCommand::new("open").arg(path_or_url).status()
    } else if cfg!(target_os = "windows") {
        ProcessCommand::new("cmd")
            .args(["/C", "start", "", path_or_url])
            .status()
    } else {
        ProcessCommand::new("xdg-open").arg(path_or_url).status()
    }
}

// ---------------------------------------------------------------------------
// wrangler.toml writer — extends setup.rs's mutate_allowed_orgs pattern
// ---------------------------------------------------------------------------

/// One `[[secrets_store_secrets]]` binding this command writes, per
/// auth.md's "### Deploy-time configuration" excerpt.
pub struct SecretBindingWrite<'a> {
    pub binding: &'static str,
    pub store_id: &'a str,
    pub secret_name: &'a str,
}

fn upsert_var(vars: &mut Table, key: &str, new_value: &str) {
    let old_decor = vars
        .get(key)
        .and_then(Item::as_value)
        .map(|v| v.decor().clone());
    vars[key] = value(new_value);
    if let (Some(decor), Some(v)) = (old_decor, vars[key].as_value_mut()) {
        *v.decor_mut() = decor;
    }
}

/// Writes `GITHUB_APP_ID`/`GITHUB_APP_CLIENT_ID`/`GITHUB_ALLOWED_ORGS` into
/// `doc`'s `[vars]` table and upserts each of `secrets` into
/// `[[secrets_store_secrets]]` — same `toml_edit` format-preserving
/// round-trip as `setup.rs::mutate_allowed_orgs` (see that function's and
/// this module's own doc comments for why `toml_edit`, not `toml`, is the
/// only defensible choice). An existing `[[secrets_store_secrets]]` entry
/// with a matching `binding` is updated in place (its `store_id`/
/// `secret_name` overwritten); a binding with no existing entry is
/// appended — this makes the writer idempotent across repeated
/// `cloud-ci setup github-app` runs (e.g. a secret rotation re-running
/// this same command) without duplicating entries.
pub fn mutate_wrangler_for_github_app(
    doc: &mut DocumentMut,
    app_id: u64,
    client_id: &str,
    allowed_orgs: &str,
    secrets: &[SecretBindingWrite<'_>],
) -> Result<(), GithubAppSetupError> {
    let vars = doc["vars"]
        .or_insert(toml_edit::table())
        .as_table_mut()
        .ok_or_else(|| GithubAppSetupError::new("`vars` in wrangler.toml is not a table"))?;
    upsert_var(vars, "GITHUB_APP_ID", &app_id.to_string());
    upsert_var(vars, "GITHUB_APP_CLIENT_ID", client_id);
    upsert_var(vars, "GITHUB_ALLOWED_ORGS", allowed_orgs);

    let secrets_item =
        doc["secrets_store_secrets"].or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    let array = secrets_item.as_array_of_tables_mut().ok_or_else(|| {
        GithubAppSetupError::new(
            "`secrets_store_secrets` in wrangler.toml is not an array of tables",
        )
    })?;

    for binding in secrets {
        if let Some(existing) = array
            .iter_mut()
            .find(|t| t.get("binding").and_then(Item::as_str) == Some(binding.binding))
        {
            existing["store_id"] = value(binding.store_id);
            existing["secret_name"] = value(binding.secret_name);
        } else {
            let mut table = Table::new();
            table["binding"] = value(binding.binding);
            table["store_id"] = value(binding.store_id);
            table["secret_name"] = value(binding.secret_name);
            array.push(table);
        }
    }

    Ok(())
}

/// Normalizes `--allowed-orgs`' comma-separated value the same way
/// `setup.rs::parse_logins`/`serialize_logins` do for `allowed-orgs
/// --add`/`--remove`: trims whitespace, drops empty entries from stray
/// commas, keeps the rest in order. This command writes the allowlist
/// directly (no add/remove semantics — it's the first value this
/// deployment's `GITHUB_ALLOWED_ORGS` will ever have), so only
/// normalization, not `setup::Op`, is needed here.
fn normalize_allowed_orgs(raw: &str) -> String {
    raw.split(',')
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Runs the full sequence from `docs/design/auth.md`'s "### GitHub App
/// setup" diagram, minus the two un-drivable steps documented at the top
/// of this module.
pub fn run_github_app(args: &GithubAppArgs) -> Result<(), GithubAppSetupError> {
    let allowed_orgs = normalize_allowed_orgs(&args.allowed_orgs);
    if allowed_orgs.is_empty() {
        return Err(GithubAppSetupError::new(
            "--allowed-orgs must name at least one org/user login",
        ));
    }

    let listener = LoopbackListener::bind(args.port).map_err(|err| {
        GithubAppSetupError::new(format!("could not bind loopback listener: {err}"))
    })?;

    let manifest = build_manifest(
        &args.name,
        &args.deployment_url,
        listener.port(),
        args.public,
    );
    let manifest_json = serde_json::to_string(&manifest)
        .map_err(|err| GithubAppSetupError::new(format!("could not serialize manifest: {err}")))?;
    let form_html = build_manifest_form_html(&manifest_json);

    let form_path = std::env::temp_dir().join(format!(
        "cloud-ci-setup-github-app-{}.html",
        std::process::id()
    ));
    fs::write(&form_path, &form_html)
        .map_err(|err| GithubAppSetupError::new(format!("could not write manifest form: {err}")))?;

    println!(
        "Opening a browser to confirm GitHub App creation (redirect_url: {})",
        listener.redirect_url()
    );
    println!(
        "If no browser opens, open this file yourself: {}",
        form_path.display()
    );
    if let Err(err) = open_in_browser(&form_path.to_string_lossy()) {
        println!("could not auto-open a browser ({err}) — open the file above manually");
    }
    println!(
        "Waiting up to {}s for the operator to confirm app creation in the browser...",
        args.timeout_secs
    );

    let code = listener.wait_for_code(Duration::from_secs(args.timeout_secs))?;
    let _ = fs::remove_file(&form_path);

    let conversion = convert_manifest(GITHUB_API_BASE_URL, &code)?;
    let master_key = generate_master_key()?;

    let cloudflare_api_token = std::env::var("CLOUDFLARE_API_TOKEN").map_err(|_| {
        GithubAppSetupError::new(
            "CLOUDFLARE_API_TOKEN is not set — the same credential `wrangler` already uses",
        )
    })?;

    let secret_specs: [(&'static str, &'static str, &str); 4] = [
        (
            "GITHUB_APP_PRIVATE_KEY",
            "github-app-private-key",
            conversion.pem.as_str(),
        ),
        (
            "GITHUB_APP_CLIENT_SECRET",
            "github-app-client-secret",
            conversion.client_secret.as_str(),
        ),
        (
            "GITHUB_WEBHOOK_SECRET",
            "github-webhook-secret",
            conversion.webhook_secret.as_str(),
        ),
        (
            "CLOUD_CI_MASTER_KEY",
            "cloud-ci-master-key",
            master_key.as_str(),
        ),
    ];

    let mut created: Vec<(&'static str, CreatedSecret)> = Vec::with_capacity(4);
    for (binding, secret_name, secret_value) in secret_specs {
        let result = create_secrets_store_secret(
            CLOUDFLARE_API_BASE_URL,
            &args.cloudflare_account_id,
            &args.secrets_store_id,
            &cloudflare_api_token,
            secret_name,
            secret_value,
        )?;
        created.push((binding, result));
    }

    let bindings: Vec<SecretBindingWrite<'_>> = created
        .iter()
        .map(|(binding, secret)| SecretBindingWrite {
            binding,
            store_id: &secret.store_id,
            secret_name: &secret.secret_name,
        })
        .collect();

    let text = fs::read_to_string(&args.file).map_err(|err| {
        GithubAppSetupError::new(format!("could not read {}: {err}", args.file.display()))
    })?;
    let mut doc: DocumentMut = text.parse().map_err(|err| {
        GithubAppSetupError::new(format!("could not parse {}: {err}", args.file.display()))
    })?;

    mutate_wrangler_for_github_app(
        &mut doc,
        conversion.id,
        &conversion.client_id,
        &allowed_orgs,
        &bindings,
    )?;

    fs::write(&args.file, doc.to_string()).map_err(|err| {
        GithubAppSetupError::new(format!("could not write {}: {err}", args.file.display()))
    })?;

    println!(
        "{}: wrote GITHUB_APP_ID, GITHUB_APP_CLIENT_ID, GITHUB_ALLOWED_ORGS, and 4 \
         secrets_store_secrets bindings",
        args.file.display()
    );

    if args.deploy {
        println!("running: wrangler deploy --config {}", args.file.display());
        let status = ProcessCommand::new("wrangler")
            .arg("deploy")
            .arg("--config")
            .arg(&args.file)
            .status()
            .map_err(|err| {
                GithubAppSetupError::new(format!("could not run wrangler deploy: {err}"))
            })?;
        if !status.success() {
            return Err(GithubAppSetupError::new(format!(
                "wrangler deploy exited with {status}"
            )));
        }
    } else {
        println!(
            "run `wrangler deploy --config {}` to apply this change (or re-run with --deploy)",
            args.file.display()
        );
    }

    println!("App registered and deployed — install it on each allowed org: {allowed_orgs}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};

    // -- build_manifest ------------------------------------------------

    #[test]
    fn build_manifest_defaults_to_private_with_derived_urls() {
        let manifest = build_manifest("cloud-ci (acme)", "https://ci.acme.example/", 54321, false);
        assert_eq!(manifest.name, "cloud-ci (acme)");
        assert_eq!(manifest.url, "https://ci.acme.example");
        assert_eq!(
            manifest.hook_attributes.url,
            "https://ci.acme.example/webhooks/github"
        );
        assert_eq!(manifest.redirect_url, "http://127.0.0.1:54321/callback");
        assert!(!manifest.public);
        assert_eq!(manifest.default_permissions.contents, "read");
        assert_eq!(manifest.default_permissions.actions, "read");
        assert_eq!(
            manifest.default_events,
            vec![
                "push",
                "pull_request",
                "check_suite",
                "check_run",
                "issue_comment",
                "installation",
                "installation_repositories",
                "workflow_run",
            ]
        );
    }

    #[test]
    fn build_manifest_public_flag_flips_public_field() {
        let manifest = build_manifest("cloud-ci (acme)", "https://ci.acme.example", 1, true);
        assert!(manifest.public);
    }

    #[test]
    fn build_manifest_json_matches_auth_md_shape() -> Result<(), String> {
        let manifest = build_manifest("cloud-ci (acme)", "https://ci.acme.example", 1, false);
        let json: serde_json::Value = serde_json::to_value(&manifest).map_err(|e| e.to_string())?;
        assert_eq!(json["name"], "cloud-ci (acme)");
        assert_eq!(
            json["hook_attributes"]["url"],
            "https://ci.acme.example/webhooks/github"
        );
        assert_eq!(json["public"], false);
        assert_eq!(json["default_permissions"]["checks"], "write");
        assert_eq!(json["default_events"][0], "push");
        Ok(())
    }

    // -- build_manifest_form_html ---------------------------------------

    #[test]
    fn manifest_form_html_embeds_escaped_manifest_and_posts_to_github() {
        let manifest_json = r#"{"name":"cloud-ci (acme) & co","public":false}"#;
        let html = build_manifest_form_html(manifest_json);
        assert!(html.contains("action=\"https://github.com/settings/apps/new\""));
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("name=\"manifest\""));
        assert!(html.contains(
            "&quot;name&quot;:&quot;cloud-ci (acme) &amp; co&quot;,&quot;public&quot;:false}"
        ));
        assert!(html.contains(".submit()"));
    }

    // -- generate_master_key ---------------------------------------------

    #[test]
    fn master_key_is_64_hex_chars_and_distinct_across_calls() -> Result<(), String> {
        let a = generate_master_key().map_err(|e| e.to_string())?;
        let b = generate_master_key().map_err(|e| e.to_string())?;
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        Ok(())
    }

    // -- parse_callback_head ----------------------------------------------

    #[test]
    fn parse_callback_head_extracts_code() -> Result<(), String> {
        let head = "GET /callback?code=abc123&state=xyz HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n";
        let code = parse_callback_head(head).map_err(|e| e.to_string())?;
        assert_eq!(code, "abc123");
        Ok(())
    }

    #[test]
    fn parse_callback_head_rejects_missing_code() {
        let head = "GET /callback?state=xyz HTTP/1.1\r\n\r\n";
        let err = match parse_callback_head(head) {
            Ok(code) => format!("unexpectedly parsed code {code}"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("carried no code parameter"), "{err}");
    }

    #[test]
    fn parse_callback_head_rejects_non_get() {
        let head = "POST /callback HTTP/1.1\r\n\r\n";
        let err = match parse_callback_head(head) {
            Ok(code) => format!("unexpectedly parsed code {code}"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("expected a GET"), "{err}");
    }

    #[test]
    fn parse_callback_head_rejects_empty_code() {
        let head = "GET /callback?code= HTTP/1.1\r\n\r\n";
        let err = match parse_callback_head(head) {
            Ok(code) => format!("unexpectedly parsed code {code}"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("empty"), "{err}");
    }

    // -- LoopbackListener, driven over a real socket ----------------------

    #[test]
    fn loopback_listener_receives_a_synthetic_callback_over_a_real_socket() -> Result<(), String> {
        let listener = LoopbackListener::bind(None).map_err(|e| e.to_string())?;
        let port = listener.port();
        assert_eq!(
            listener.redirect_url(),
            format!("http://127.0.0.1:{port}/callback")
        );

        thread::spawn(move || {
            let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
                return;
            };
            let _ = stream.write_all(
                b"GET /callback?code=synthetic-code-123 HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n",
            );
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
        });

        let code = listener
            .wait_for_code(Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        assert_eq!(code, "synthetic-code-123");
        Ok(())
    }

    #[test]
    fn loopback_listener_times_out_with_no_callback() -> Result<(), String> {
        let listener = LoopbackListener::bind(None).map_err(|e| e.to_string())?;
        let err = match listener.wait_for_code(Duration::from_millis(50)) {
            Ok(code) => return Err(format!("expected a timeout, got code {code}")),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("timed out"), "{err}");
        Ok(())
    }

    // -- convert_manifest, against a local fixture server -----------------

    fn fixture_github_response() -> ManifestConversion {
        ManifestConversion {
            id: 123_456,
            pem: "FIXTURE-PEM-NOT-A-REAL-KEY-fixture-fixture-fixture".to_string(),
            webhook_secret: "fixture-webhook-secret".to_string(),
            client_id: "Iv1.fixture".to_string(),
            client_secret: "fixture-client-secret".to_string(),
        }
    }

    /// Reads one real HTTP/1.1 request off `stream` for the fixture
    /// server below, returning its first line. Tracks the exact header/
    /// body boundary within the buffered bytes (unlike the production
    /// `read_request_head`, which only needs the request line for a
    /// bodyless `GET /callback`) so a POST's declared `content-length`
    /// body is drained correctly even when the client's single write
    /// already delivered header and body bytes together — same indexed
    /// approach `upload.rs`'s own test-fixture `read_request` uses.
    fn read_request_line_draining_body(stream: &mut TcpStream) -> std::io::Result<String> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let n = stream.read(&mut chunk)?;
            if n == 0 {
                break buf.len();
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };

        let header_text = String::from_utf8_lossy(&buf[..header_end.min(buf.len())]).into_owned();
        let request_line = header_text.lines().next().unwrap_or_default().to_string();

        let content_length: usize = header_text
            .lines()
            .find_map(|l| {
                l.to_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().to_string())
            })
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let mut body_so_far = buf.len().saturating_sub(header_end);
        while body_so_far < content_length {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => body_so_far += n,
                Err(_) => break,
            }
        }

        Ok(request_line)
    }

    fn start_single_request_fixture(
        status: u16,
        body: Vec<u8>,
    ) -> std::io::Result<(String, Arc<Mutex<Option<String>>>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let captured_path = Arc::new(Mutex::new(None));
        let captured_for_thread = Arc::clone(&captured_path);

        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let Ok(request_line) = read_request_line_draining_body(&mut stream) else {
                return;
            };
            if let Ok(mut guard) = captured_for_thread.lock() {
                *guard = Some(request_line);
            }
            let response = format!(
                "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        });

        Ok((format!("http://{addr}"), captured_path))
    }

    #[test]
    fn convert_manifest_parses_a_realistic_fixture_response() -> Result<(), String> {
        let fixture = fixture_github_response();
        let body = serde_json::to_vec(&serde_json::json!({
            "id": fixture.id,
            "slug": "cloud-ci-acme",
            "pem": fixture.pem,
            "webhook_secret": fixture.webhook_secret,
            "client_id": fixture.client_id,
            "client_secret": fixture.client_secret,
        }))
        .map_err(|e| e.to_string())?;
        let (base_url, captured_path) =
            start_single_request_fixture(200, body).map_err(|e| e.to_string())?;

        let result = convert_manifest(&base_url, "the-code").map_err(|e| e.to_string())?;
        assert_eq!(result, fixture);

        let path = captured_path
            .lock()
            .map_err(|_| "captured path poisoned".to_string())?
            .clone()
            .ok_or_else(|| "no request captured".to_string())?;
        assert!(
            path.contains("POST /app-manifests/the-code/conversions"),
            "{path}"
        );
        Ok(())
    }

    #[test]
    fn convert_manifest_surfaces_a_non_2xx_response() -> Result<(), String> {
        let (base_url, _) =
            start_single_request_fixture(404, br#"{"message":"Not Found"}"#.to_vec())
                .map_err(|e| e.to_string())?;
        let err = match convert_manifest(&base_url, "bad-code") {
            Ok(_) => return Err("expected an error response".to_string()),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("http 404"), "{err}");
        Ok(())
    }

    // -- create_secrets_store_secret, against a local fixture server ------

    #[test]
    fn create_secrets_store_secret_parses_a_realistic_fixture_response() -> Result<(), String> {
        let body = serde_json::to_vec(&serde_json::json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [{
                "id": "secret-id-1",
                "created": "2026-10-02T00:00:00Z",
                "modified": "2026-10-02T00:00:00Z",
                "name": "github-app-private-key",
                "status": "active",
                "store_id": "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
            }],
        }))
        .map_err(|e| e.to_string())?;
        let (base_url, captured_path) =
            start_single_request_fixture(200, body).map_err(|e| e.to_string())?;

        let created = create_secrets_store_secret(
            &base_url,
            "account-1",
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
            "cf-api-token",
            "github-app-private-key",
            "FIXTURE-PEM-NOT-A-REAL-KEY-fixture-fixture-fixture",
        )
        .map_err(|e| e.to_string())?;

        assert_eq!(created.store_id, "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4");
        assert_eq!(created.secret_name, "github-app-private-key");

        let path = captured_path
            .lock()
            .map_err(|_| "captured path poisoned".to_string())?
            .clone()
            .ok_or_else(|| "no request captured".to_string())?;
        assert!(
            path.contains("POST /accounts/account-1/secrets_store/stores/a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4/secrets"),
            "{path}"
        );
        Ok(())
    }

    #[test]
    fn create_secrets_store_secret_surfaces_a_cloudflare_level_failure() -> Result<(), String> {
        let body = serde_json::to_vec(&serde_json::json!({
            "success": false,
            "errors": [{"code": 10000, "message": "Authentication error"}],
            "messages": [],
            "result": null,
        }))
        .map_err(|e| e.to_string())?;
        let (base_url, _) = start_single_request_fixture(200, body).map_err(|e| e.to_string())?;

        let err = match create_secrets_store_secret(
            &base_url,
            "account-1",
            "store-1",
            "bad-token",
            "github-app-private-key",
            "value",
        ) {
            Ok(_) => return Err("expected a cloudflare-level failure".to_string()),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("Authentication error"), "{err}");
        Ok(())
    }

    #[test]
    fn create_secrets_store_secret_non_2xx_never_leaks_raw_response_body() -> Result<(), String> {
        // Simulates Cloudflare echoing a submitted request field (the
        // secret's own plaintext value, standing in for the canary below)
        // back inside a non-2xx response's raw body alongside a
        // structured error — a real, plausible REST API behavior. The
        // error message must never contain the raw body verbatim (it may
        // legitimately surface Cloudflare's own structured code/message,
        // which carries no secret material here).
        const CANARY: &str = "CANARY-SECRET-VALUE-fixture-should-never-leak";
        let body = serde_json::to_vec(&serde_json::json!({
            "success": false,
            "errors": [{"code": 1004, "message": "invalid value"}],
            "messages": [],
            "result": null,
            "echoed_request_value": CANARY,
        }))
        .map_err(|e| e.to_string())?;
        let (base_url, _) = start_single_request_fixture(400, body).map_err(|e| e.to_string())?;

        let err = match create_secrets_store_secret(
            &base_url,
            "account-1",
            "store-1",
            "bad-token",
            "github-app-private-key",
            CANARY,
        ) {
            Ok(_) => return Err("expected a non-2xx failure".to_string()),
            Err(err) => err.to_string(),
        };
        assert!(
            !err.contains(CANARY),
            "error message leaked the raw response body: {err}"
        );
        assert!(err.contains("400"), "{err}");
        assert!(err.contains("invalid value"), "{err}");
        Ok(())
    }

    #[test]
    fn create_secrets_store_secret_non_2xx_redacts_secret_embedded_in_error_message()
    -> Result<(), String> {
        // Here the secret value appears *inside* Cloudflare's own
        // structured error message text itself (not just elsewhere in the
        // raw body) — the fix must redact the fully-assembled message, not
        // just omit the raw body.
        const CANARY: &str = "CANARY-SECRET-VALUE-fixture-should-never-leak";
        let body = serde_json::to_vec(&serde_json::json!({
            "success": false,
            "errors": [{"code": 1004, "message": format!("invalid value: {CANARY}")}],
            "messages": [],
            "result": null,
        }))
        .map_err(|e| e.to_string())?;
        let (base_url, _) = start_single_request_fixture(400, body).map_err(|e| e.to_string())?;

        let err = match create_secrets_store_secret(
            &base_url,
            "account-1",
            "store-1",
            "bad-token",
            "github-app-private-key",
            CANARY,
        ) {
            Ok(_) => return Err("expected a non-2xx failure".to_string()),
            Err(err) => err.to_string(),
        };
        assert!(
            !err.contains(CANARY),
            "error message leaked the secret embedded in an error message: {err}"
        );
        assert!(err.contains("[REDACTED]"), "{err}");
        assert!(err.contains("400"), "{err}");
        Ok(())
    }

    #[test]
    fn create_secrets_store_secret_2xx_reported_failure_redacts_secret_embedded_in_error_message()
    -> Result<(), String> {
        // Same leak shape as the non-2xx case above, but on the 2xx
        // "success": false path, which builds its error message the same
        // way and needs the same redaction.
        const CANARY: &str = "CANARY-SECRET-VALUE-fixture-should-never-leak";
        let body = serde_json::to_vec(&serde_json::json!({
            "success": false,
            "errors": [{"code": 1004, "message": format!("invalid value: {CANARY}")}],
            "messages": [],
            "result": null,
        }))
        .map_err(|e| e.to_string())?;
        let (base_url, _) = start_single_request_fixture(200, body).map_err(|e| e.to_string())?;

        let err = match create_secrets_store_secret(
            &base_url,
            "account-1",
            "store-1",
            "bad-token",
            "github-app-private-key",
            CANARY,
        ) {
            Ok(_) => return Err("expected a cloudflare-level failure".to_string()),
            Err(err) => err.to_string(),
        };
        assert!(
            !err.contains(CANARY),
            "error message leaked the secret embedded in an error message: {err}"
        );
        assert!(err.contains("[REDACTED]"), "{err}");
        Ok(())
    }

    #[test]
    fn create_secrets_store_secret_non_2xx_without_structured_errors_omits_body()
    -> Result<(), String> {
        // Cloudflare's non-2xx response doesn't even parse as the
        // success-path's `SecretCreateApiResponse` shape here (e.g. a
        // proxy-level error page) — the fallback message must still never
        // include the raw bytes.
        const CANARY: &str = "CANARY-UNSTRUCTURED-fixture-should-never-leak";
        let (base_url, _) = start_single_request_fixture(
            502,
            format!("<html>upstream error: {CANARY}</html>").into_bytes(),
        )
        .map_err(|e| e.to_string())?;

        let err = match create_secrets_store_secret(
            &base_url,
            "account-1",
            "store-1",
            "bad-token",
            "github-app-private-key",
            "value",
        ) {
            Ok(_) => return Err("expected a non-2xx failure".to_string()),
            Err(err) => err.to_string(),
        };
        assert!(
            !err.contains(CANARY),
            "error message leaked the raw response body: {err}"
        );
        assert!(err.contains("502"), "{err}");
        Ok(())
    }

    // -- mutate_wrangler_for_github_app ------------------------------------

    const FIXTURE_WRANGLER_TOML: &str = r#"name = "cloud-ci"
main = "build/index.js"

# Not secret: public identifiers.
[vars]
GITHUB_APP_ID = ""
GITHUB_APP_CLIENT_ID = ""
GITHUB_ALLOWED_ORGS = ""
CLOUD_CI_INGEST_AUDIENCE = ""

[[d1_databases]]
binding = "DB"
database_name = "cloud-ci"
"#;

    fn fixture_bindings() -> Vec<(String, String)> {
        vec![
            ("github-app-private-key".to_string(), "aaaa".to_string()),
            ("github-app-client-secret".to_string(), "bbbb".to_string()),
            ("github-webhook-secret".to_string(), "cccc".to_string()),
            ("cloud-ci-master-key".to_string(), "dddd".to_string()),
        ]
    }

    #[test]
    fn mutate_wrangler_writes_vars_and_appends_secret_bindings_preserving_rest()
    -> Result<(), String> {
        let mut doc: DocumentMut = FIXTURE_WRANGLER_TOML
            .parse()
            .map_err(|e: toml_edit::TomlError| e.to_string())?;
        let store_ids = fixture_bindings();
        let bindings = vec![
            SecretBindingWrite {
                binding: "GITHUB_APP_PRIVATE_KEY",
                store_id: &store_ids[0].1,
                secret_name: &store_ids[0].0,
            },
            SecretBindingWrite {
                binding: "GITHUB_APP_CLIENT_SECRET",
                store_id: &store_ids[1].1,
                secret_name: &store_ids[1].0,
            },
            SecretBindingWrite {
                binding: "GITHUB_WEBHOOK_SECRET",
                store_id: &store_ids[2].1,
                secret_name: &store_ids[2].0,
            },
            SecretBindingWrite {
                binding: "CLOUD_CI_MASTER_KEY",
                store_id: &store_ids[3].1,
                secret_name: &store_ids[3].0,
            },
        ];

        mutate_wrangler_for_github_app(
            &mut doc,
            999_999,
            "Iv1.client",
            "acme-corp,acme-labs",
            &bindings,
        )
        .map_err(|e| e.to_string())?;

        let out = doc.to_string();
        assert!(out.contains("GITHUB_APP_ID = \"999999\""));
        assert!(out.contains("GITHUB_APP_CLIENT_ID = \"Iv1.client\""));
        assert!(out.contains("GITHUB_ALLOWED_ORGS = \"acme-corp,acme-labs\""));
        // Untouched keys/comments/tables survive the round-trip.
        assert!(out.contains("CLOUD_CI_INGEST_AUDIENCE = \"\""));
        assert!(out.contains("# Not secret: public identifiers."));
        assert!(out.contains("[[d1_databases]]"));
        assert!(out.contains("database_name = \"cloud-ci\""));
        // All four secrets_store_secrets entries present.
        for binding in [
            "GITHUB_APP_PRIVATE_KEY",
            "GITHUB_APP_CLIENT_SECRET",
            "GITHUB_WEBHOOK_SECRET",
            "CLOUD_CI_MASTER_KEY",
        ] {
            assert!(out.contains(&format!("binding = \"{binding}\"")), "{out}");
        }
        assert!(out.contains("store_id = \"aaaa\""));
        assert!(out.contains("secret_name = \"github-app-private-key\""));
        Ok(())
    }

    #[test]
    fn mutate_wrangler_updates_existing_binding_in_place_without_duplicating() -> Result<(), String>
    {
        let mut doc: DocumentMut = FIXTURE_WRANGLER_TOML
            .parse()
            .map_err(|e: toml_edit::TomlError| e.to_string())?;
        let first = vec![SecretBindingWrite {
            binding: "GITHUB_APP_PRIVATE_KEY",
            store_id: "store-old",
            secret_name: "github-app-private-key",
        }];
        mutate_wrangler_for_github_app(&mut doc, 1, "client-1", "acme-corp", &first)
            .map_err(|e| e.to_string())?;

        let second = vec![SecretBindingWrite {
            binding: "GITHUB_APP_PRIVATE_KEY",
            store_id: "store-new",
            secret_name: "github-app-private-key",
        }];
        mutate_wrangler_for_github_app(&mut doc, 1, "client-1", "acme-corp", &second)
            .map_err(|e| e.to_string())?;

        let out = doc.to_string();
        assert_eq!(
            out.matches("binding = \"GITHUB_APP_PRIVATE_KEY\"").count(),
            1
        );
        assert!(out.contains("store_id = \"store-new\""));
        assert!(!out.contains("store_id = \"store-old\""));
        Ok(())
    }

    // -- normalize_allowed_orgs --------------------------------------------

    #[test]
    fn normalize_allowed_orgs_trims_and_drops_blanks() {
        assert_eq!(
            normalize_allowed_orgs(" acme-corp ,, acme-labs ,"),
            "acme-corp,acme-labs"
        );
    }
}
