//! Connect protocol unary codec, independent of the Workers runtime so it is testable natively.

use buffa::Message;
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Proto,
    Json,
}

impl Codec {
    pub fn from_content_type(content_type: &str) -> Option<Self> {
        let media = content_type.split(';').next().unwrap_or_default().trim();
        if media.eq_ignore_ascii_case("application/proto") {
            Some(Self::Proto)
        } else if media.eq_ignore_ascii_case("application/json") {
            Some(Self::Json)
        } else {
            None
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Proto => "application/proto",
            Self::Json => "application/json",
        }
    }

    pub fn decode<M: Message + DeserializeOwned>(self, body: &[u8]) -> Result<M, ConnectError> {
        match self {
            Self::Proto => M::decode_from_slice(body)
                .map_err(|e| ConnectError::new(Code::InvalidArgument, e.to_string())),
            Self::Json => serde_json::from_slice(body)
                .map_err(|e| ConnectError::new(Code::InvalidArgument, e.to_string())),
        }
    }

    pub fn encode<M: Message + Serialize>(self, message: &M) -> Result<Vec<u8>, ConnectError> {
        match self {
            Self::Proto => Ok(message.encode_to_vec()),
            Self::Json => serde_json::to_vec(message)
                .map_err(|e| ConnectError::new(Code::Internal, e.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    InvalidArgument,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    FailedPrecondition,
    Unimplemented,
    Internal,
    Unavailable,
    Unauthenticated,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::NotFound => "not_found",
            Self::AlreadyExists => "already_exists",
            Self::PermissionDenied => "permission_denied",
            Self::FailedPrecondition => "failed_precondition",
            Self::Unimplemented => "unimplemented",
            Self::Internal => "internal",
            Self::Unavailable => "unavailable",
            Self::Unauthenticated => "unauthenticated",
        }
    }

    pub fn http_status(self) -> u16 {
        match self {
            Self::InvalidArgument | Self::FailedPrecondition => 400,
            Self::Unauthenticated => 401,
            Self::PermissionDenied => 403,
            Self::NotFound => 404,
            Self::AlreadyExists => 409,
            Self::Internal => 500,
            Self::Unimplemented => 501,
            Self::Unavailable => 503,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectError {
    pub code: Code,
    pub message: String,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

impl ConnectError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Connect errors are always JSON, whatever the request codec.
    pub fn body(&self) -> Vec<u8> {
        serde_json::to_vec(&ErrorBody {
            code: self.code.as_str(),
            message: &self.message,
        })
        .unwrap_or_default()
    }
}

/// Validates the unary request headers and returns the codec to use for both directions.
pub fn negotiate(
    content_type: Option<&str>,
    content_encoding: Option<&str>,
) -> Result<Codec, NegotiationError> {
    let codec = content_type
        .and_then(Codec::from_content_type)
        .ok_or(NegotiationError::UnsupportedMediaType)?;
    match content_encoding {
        None => Ok(codec),
        Some(enc) if enc.eq_ignore_ascii_case("identity") => Ok(codec),
        Some(enc) => Err(NegotiationError::Connect(ConnectError::new(
            Code::Unimplemented,
            format!("unsupported content-encoding {enc}"),
        ))),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum NegotiationError {
    /// Connect requires a bare HTTP 415 for unknown codecs.
    UnsupportedMediaType,
    Connect(ConnectError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloud_ci_proto::ingest::v1::{BeginRunRequest, RunKey, Trigger};

    fn sample() -> BeginRunRequest {
        BeginRunRequest {
            key: RunKey {
                repo_id: 1_296_269,
                sha: "6dcb09b5b57875f334f61aebed695e2e4193db5e".into(),
                run_key: "gha/42".into(),
                attempt: 2,
                ..Default::default()
            }
            .into(),
            trigger: Trigger::TRIGGER_PULL_REQUEST.into(),
            expect_jobs: vec!["unit".into(), "e2e".into()],
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_both_codecs() -> Result<(), ConnectError> {
        for codec in [Codec::Proto, Codec::Json] {
            let bytes = codec.encode(&sample())?;
            let decoded: BeginRunRequest = codec.decode(&bytes)?;
            assert_eq!(decoded, sample(), "{codec:?}");
        }
        Ok(())
    }

    #[test]
    fn json_uses_proto3_field_names_and_enum_strings() -> Result<(), ConnectError> {
        let decoded: BeginRunRequest = Codec::Json.decode(
            br#"{"key":{"repoId":"1296269","sha":"abc","runKey":"gha/1","attempt":1},"trigger":"TRIGGER_PUSH","timeout":"90s"}"#,
        )?;
        assert_eq!(decoded.trigger, Trigger::TRIGGER_PUSH);
        assert_eq!(decoded.key.repo_id, 1_296_269);
        assert_eq!(decoded.timeout.seconds, 90);
        Ok(())
    }

    #[test]
    fn malformed_body_is_invalid_argument() {
        let err = Codec::Proto
            .decode::<BeginRunRequest>(&[0xff, 0xff])
            .err()
            .map(|e| e.code);
        assert_eq!(err, Some(Code::InvalidArgument));
    }

    #[test]
    fn negotiation() {
        assert_eq!(
            negotiate(Some("application/json; charset=utf-8"), None),
            Ok(Codec::Json)
        );
        assert_eq!(
            negotiate(Some("application/grpc"), None),
            Err(NegotiationError::UnsupportedMediaType)
        );
        assert!(matches!(
            negotiate(Some("application/proto"), Some("gzip")),
            Err(NegotiationError::Connect(ConnectError {
                code: Code::Unimplemented,
                ..
            }))
        ));
    }

    #[test]
    fn http_status_matches_connect_spec() {
        // https://connectrpc.com/docs/protocol/#error-codes
        assert_eq!(Code::InvalidArgument.http_status(), 400);
        assert_eq!(Code::FailedPrecondition.http_status(), 400);
        assert_eq!(Code::Unauthenticated.http_status(), 401);
        assert_eq!(Code::PermissionDenied.http_status(), 403);
        assert_eq!(Code::NotFound.http_status(), 404);
        assert_eq!(Code::AlreadyExists.http_status(), 409);
        assert_eq!(Code::Internal.http_status(), 500);
        assert_eq!(Code::Unimplemented.http_status(), 501);
        assert_eq!(Code::Unavailable.http_status(), 503);
    }
}
