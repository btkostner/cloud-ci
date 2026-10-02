pub mod connect;

use cloud_ci_proto::ingest::v1::{
    BeginRunRequest, CompleteShardRequest, CompleteUploadRequest, CreateUploadRequest,
    GetRunRequest, StartJobRequest, SubmitReportRequest,
};
use connect::{Code, Codec, ConnectError, NegotiationError, negotiate};
use worker::{Context, Env, Headers, Method, Request, Response, Result, event};

#[event(fetch)]
async fn fetch(mut req: Request, _env: Env, _ctx: Context) -> Result<Response> {
    if req.method() != Method::Post {
        return Response::error("method not allowed", 405);
    }
    let headers = req.headers();
    let codec = match negotiate(
        headers.get("content-type")?.as_deref(),
        headers.get("content-encoding")?.as_deref(),
    ) {
        Ok(codec) => codec,
        Err(NegotiationError::UnsupportedMediaType) => {
            return Response::error("unsupported media type", 415);
        }
        Err(NegotiationError::Connect(err)) => return connect_error(&err),
    };
    let body = req.bytes().await?;
    connect_error(&route(&req.path(), codec, &body))
}

/// Hand-routes each `cloud_ci.ingest.v1.IngestService` procedure to its request type,
/// per ADR 0002 (Connect over `fetch`, no Tower server). Every procedure decodes its
/// request body before reporting `unimplemented`, proving buffa's JSON and binary
/// decoders work correctly once compiled to `wasm32-unknown-unknown`, even though
/// `RunCoordinator` doesn't exist yet to act on the decoded message or encode a
/// typed response.
fn route(path: &str, codec: Codec, body: &[u8]) -> ConnectError {
    match path {
        "/cloud_ci.ingest.v1.IngestService/BeginRun" => {
            decode_then_unimplemented::<BeginRunRequest>("BeginRun", codec, body)
        }
        "/cloud_ci.ingest.v1.IngestService/StartJob" => {
            decode_then_unimplemented::<StartJobRequest>("StartJob", codec, body)
        }
        "/cloud_ci.ingest.v1.IngestService/CreateUpload" => {
            decode_then_unimplemented::<CreateUploadRequest>("CreateUpload", codec, body)
        }
        "/cloud_ci.ingest.v1.IngestService/CompleteUpload" => {
            decode_then_unimplemented::<CompleteUploadRequest>("CompleteUpload", codec, body)
        }
        "/cloud_ci.ingest.v1.IngestService/SubmitReport" => {
            decode_then_unimplemented::<SubmitReportRequest>("SubmitReport", codec, body)
        }
        "/cloud_ci.ingest.v1.IngestService/CompleteShard" => {
            decode_then_unimplemented::<CompleteShardRequest>("CompleteShard", codec, body)
        }
        "/cloud_ci.ingest.v1.IngestService/GetRun" => {
            decode_then_unimplemented::<GetRunRequest>("GetRun", codec, body)
        }
        _ => ConnectError::new(Code::Unimplemented, format!("unknown procedure {path}")),
    }
}

fn decode_then_unimplemented<M: buffa::Message + serde::de::DeserializeOwned>(
    name: &str,
    codec: Codec,
    body: &[u8],
) -> ConnectError {
    match codec.decode::<M>(body) {
        Ok(_) => ConnectError::new(Code::Unimplemented, format!("{name} is not implemented")),
        Err(err) => err,
    }
}

fn connect_error(err: &ConnectError) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    Ok(Response::from_bytes(err.body())?
        .with_status(err.code.http_status())
        .with_headers(headers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloud_ci_proto::ingest::v1::RunKey;

    #[test]
    fn known_procedure_decodes_both_codecs_before_reporting_unimplemented()
    -> std::result::Result<(), ConnectError> {
        let req = BeginRunRequest {
            key: RunKey {
                repo_id: 1_296_269,
                sha: "6dcb09b5b57875f334f61aebed695e2e4193db5e".into(),
                run_key: "gha/42".into(),
                attempt: 1,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let path = "/cloud_ci.ingest.v1.IngestService/BeginRun";
        for codec in [Codec::Proto, Codec::Json] {
            let body = codec.encode(&req)?;
            let err = route(path, codec, &body);
            assert_eq!(err.code, Code::Unimplemented, "{codec:?}");
        }
        Ok(())
    }

    #[test]
    fn known_procedure_with_malformed_body_is_invalid_argument() {
        let err = route(
            "/cloud_ci.ingest.v1.IngestService/BeginRun",
            Codec::Proto,
            &[0xff, 0xff],
        );
        assert_eq!(err.code, Code::InvalidArgument);
    }

    #[test]
    fn unknown_procedure_is_unimplemented() {
        let err = route("/cloud_ci.ingest.v1.IngestService/Nope", Codec::Json, b"{}");
        assert_eq!(err.code, Code::Unimplemented);
    }
}
