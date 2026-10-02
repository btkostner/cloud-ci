//! Client-side mirror of `cloud-ci-worker`'s hand-rolled Connect protocol
//! codec (`packages/cloud-ci-worker/src/connect.rs`): the same two content
//! types (`application/proto`/`application/json`), the same procedure path
//! scheme (`POST {base_url}/cloud_ci.ingest.v1.IngestService/<Method>`), and
//! the same Connect error JSON shape (`{"code": ..., "message": ...}`) that
//! `ConnectError::body()` emits on the Worker, so the CLI talks to a real
//! deployment correctly.
//!
//! HTTP client: `ureq`, blocking. `cloud-ci upload` runs a short, strictly
//! sequential chain of RPCs (`BeginRun` -> `StartJob` -> ... ->
//! `CompleteShard`); there is no concurrency within one invocation to
//! exploit, so a blocking client keeps the dependency tree and the call
//! sites simple instead of pulling in an async runtime to await one call at
//! a time.

use std::fmt;
use std::time::Duration;

use buffa::Message;
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Proto,
    Json,
}

impl Codec {
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Proto => "application/proto",
            Self::Json => "application/json",
        }
    }

    fn encode<M: Message + Serialize>(self, message: &M) -> Result<Vec<u8>, CallError> {
        match self {
            Self::Proto => Ok(message.encode_to_vec()),
            Self::Json => serde_json::to_vec(message).map_err(|e| CallError::Encode(e.to_string())),
        }
    }

    fn decode<M: Message + DeserializeOwned>(self, body: &[u8]) -> Result<M, CallError> {
        match self {
            Self::Proto => M::decode_from_slice(body).map_err(|e| CallError::Decode(e.to_string())),
            Self::Json => {
                serde_json::from_slice(body).map_err(|e| CallError::Decode(e.to_string()))
            }
        }
    }
}

/// The Connect error JSON body shape `cloud-ci-worker`'s `ConnectError::body()`
/// emits: `{"code": "...", "message": "..."}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ConnectError {
    pub code: String,
    pub message: String,
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ConnectError {}

#[derive(Debug)]
pub enum CallError {
    /// The server responded with a non-2xx status and a Connect error body.
    Connect(ConnectError),
    /// A non-2xx status whose body was not valid Connect error JSON.
    MalformedError {
        status: u16,
        body: String,
    },
    /// Could not reach the server, or the connection failed mid-request.
    Transport(String),
    Encode(String),
    Decode(String),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => write!(f, "{err}"),
            Self::MalformedError { status, body } => write!(f, "http {status}: {body}"),
            Self::Transport(msg) => write!(f, "transport error: {msg}"),
            Self::Encode(msg) => write!(f, "failed to encode request: {msg}"),
            Self::Decode(msg) => write!(f, "failed to decode response: {msg}"),
        }
    }
}

impl std::error::Error for CallError {}

/// A Connect unary RPC client bound to one deployment base URL.
pub struct Client {
    base_url: String,
    codec: Codec,
    token: Option<String>,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(base_url: impl Into<String>, codec: Codec, token: Option<String>) -> Self {
        let config = ureq::Agent::config_builder()
            // Status codes are inspected by hand in `call` so a Connect
            // error body on a 4xx/5xx response can be decoded instead of
            // being discarded by ureq's default error handling.
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build();
        Self {
            base_url: base_url.into(),
            codec,
            token,
            agent: config.into(),
        }
    }

    /// Calls one `cloud_ci.ingest.v1.IngestService` unary procedure, for
    /// example `call::<BeginRunRequest, BeginRunResponse>("BeginRun", &req)`.
    pub fn call<Req, Resp>(&self, procedure: &str, request: &Req) -> Result<Resp, CallError>
    where
        Req: Message + Serialize,
        Resp: Message + DeserializeOwned,
    {
        let url = format!(
            "{}/cloud_ci.ingest.v1.IngestService/{procedure}",
            self.base_url.trim_end_matches('/'),
        );
        let body = self.codec.encode(request)?;

        let mut builder = self
            .agent
            .post(&url)
            .header("content-type", self.codec.content_type());
        if let Some(token) = &self.token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }

        let mut response = builder
            .send(body)
            .map_err(|e| CallError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let response_body = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| CallError::Transport(e.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(
                match serde_json::from_slice::<ConnectError>(&response_body) {
                    Ok(err) => CallError::Connect(err),
                    Err(_) => CallError::MalformedError {
                        status,
                        body: String::from_utf8_lossy(&response_body).into_owned(),
                    },
                },
            );
        }

        self.codec.decode(&response_body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloud_ci_proto::ingest::v1::{BeginRunRequest, BeginRunResponse, RunKey};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Starts a one-shot raw TCP server that reads an HTTP/1.1 request off
    /// the wire (ignoring it) and writes back a fixed response, so the
    /// client's HTTP-error path is proven against a real socket instead of
    /// only against in-process JSON decoding.
    fn serve_once(status_line: &str, body: &str) -> std::io::Result<String> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let status_line = status_line.to_string();
        let body = body.to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                // Drain whatever the client already sent; the test doesn't
                // need to parse it, only avoid a connection reset before the
                // response is written.
                let _ = stream.read(&mut buf);
                let response = format!(
                    "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Ok(format!("http://{addr}"))
    }

    fn sample_request() -> BeginRunRequest {
        BeginRunRequest {
            key: RunKey {
                repo_id: 1,
                sha: "abc".into(),
                run_key: "gha/1".into(),
                attempt: 1,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        }
    }

    #[test]
    fn surfaces_a_real_connect_error_response_over_http() -> Result<(), String> {
        let base_url = serve_once(
            "HTTP/1.1 501 Not Implemented",
            r#"{"code":"unimplemented","message":"BeginRun is not implemented"}"#,
        )
        .map_err(|e| e.to_string())?;
        let client = Client::new(base_url, Codec::Json, None);

        match client.call::<BeginRunRequest, BeginRunResponse>("BeginRun", &sample_request()) {
            Err(CallError::Connect(ConnectError { code, message })) => {
                assert_eq!(code, "unimplemented");
                assert_eq!(message, "BeginRun is not implemented");
                Ok(())
            }
            other => Err(format!("expected CallError::Connect, got {other:?}")),
        }
    }

    #[test]
    fn surfaces_malformed_error_bodies_without_panicking() -> Result<(), String> {
        let base_url = serve_once("HTTP/1.1 500 Internal Server Error", "not json")
            .map_err(|e| e.to_string())?;
        let client = Client::new(base_url, Codec::Json, None);

        match client.call::<BeginRunRequest, BeginRunResponse>("BeginRun", &sample_request()) {
            Err(CallError::MalformedError { status, body }) => {
                assert_eq!(status, 500);
                assert_eq!(body, "not json");
                Ok(())
            }
            other => Err(format!("expected CallError::MalformedError, got {other:?}")),
        }
    }
}
