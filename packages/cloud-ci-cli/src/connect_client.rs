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
    /// Binary wire format. Not yet selected by any `cloud-ci` flag (the CLI
    /// always uses `Json` for debuggability), but a real, tested encoding
    /// this client supports for future use.
    #[allow(dead_code)]
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

    /// Like `serve_once`, but writes a raw binary body with an arbitrary
    /// `Content-Type`, so a real binary (`application/proto`) response can
    /// be proven over an actual socket instead of only through in-process
    /// `Codec::Proto` encode/decode (`proto_codec_round_trips_requests`,
    /// which never touches HTTP) or `Codec::Json` (every other test in this
    /// file, and `cloud-ci-worker`'s `full_upload_sequence_executes_against_a_real_http_fixture`,
    /// which only ever drives `Codec::Json`).
    fn serve_once_binary(
        status_line: &str,
        content_type: &str,
        body: &[u8],
    ) -> std::io::Result<String> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let status_line = status_line.to_string();
        let content_type = content_type.to_string();
        let body = body.to_vec();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let header = format!(
                    "{status_line}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
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
    fn proto_codec_round_trips_requests() -> Result<(), String> {
        let encoded = Codec::Proto
            .encode(&sample_request())
            .map_err(|e| e.to_string())?;
        let decoded: BeginRunRequest = Codec::Proto.decode(&encoded).map_err(|e| e.to_string())?;
        assert_eq!(decoded, sample_request());
        assert_eq!(Codec::Proto.content_type(), "application/proto");
        Ok(())
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

    /// Proves `Codec::Proto` round-trips a successful response over a real
    /// socket end to end: the client sends a binary request with
    /// `Content-Type: application/proto`, a server stand-in replies with a
    /// binary-encoded real generated `BeginRunResponse`, and the client
    /// decodes it correctly. `proto_codec_round_trips_requests` above only
    /// exercises `Codec::encode`/`decode` in-process; every HTTP-backed test
    /// in this file and in `cloud-ci-worker`'s
    /// `full_upload_sequence_executes_against_a_real_http_fixture` only ever
    /// uses `Codec::Json` (the CLI's actual production choice). This closes
    /// that gap for `docs/roadmap.md`'s "buffa on wasm32" spike, which asks
    /// for a round trip of both codecs, not just the one the CLI ships with.
    #[test]
    fn client_round_trips_a_successful_binary_response_over_http() -> Result<(), String> {
        let expected = BeginRunResponse {
            run_id: "run-1".into(),
            ingest_token: "ingest-token-1".into(),
            ..Default::default()
        };
        let body = Codec::Proto.encode(&expected).map_err(|e| e.to_string())?;
        let base_url = serve_once_binary("HTTP/1.1 200 OK", "application/proto", &body)
            .map_err(|e| e.to_string())?;
        let client = Client::new(base_url, Codec::Proto, None);

        let response = client
            .call::<BeginRunRequest, BeginRunResponse>("BeginRun", &sample_request())
            .map_err(|e| e.to_string())?;
        assert_eq!(response, expected);
        Ok(())
    }
}
