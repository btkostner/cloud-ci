//! Orchestrates `cloud-ci split`: expands `--files`, computes the shard
//! assignment via `cloud_ci_core::split` (the pure algorithm library, see
//! that crate's docs), and prints `--index`'s file list, one per line, to
//! stdout. Per `docs/design/parallelization.md`'s "`cloud-ci split` (also
//! usable from BYO CI)".
//!
//! # `--strategy timing`
//!
//! `timing`'s real data source is the `test_stats` D1 table
//! (`docs/design/analytics.md`), read via the `GetTestTimings` RPC
//! (`cloud_ci_proto::ingest::v1`), one point lookup per invocation
//! covering every matched file. [`RemoteHistoryLookup`] below implements
//! `cloud_ci_core::split::HistoryLookup` by calling that RPC once (via
//! `crate::connect_client::Client`, the same Connect client
//! `cloud-ci upload` uses) and serving every later [`HistoryLookup::duration_ms`]
//! call from the response already in memory. Per that crate's "Fallback
//! when no history exists" documentation, a file `GetTestTimings` has no
//! entry for (new repo, or a file never seen before) is treated as
//! "unknown duration", not zero — `cloud_ci_core::split`'s median
//! imputation (or, if *no* matched file has any history, a full degrade
//! to `file` round-robin order) handles it from there with no change to
//! this module. `--strategy file`/`--strategy count` never call
//! `GetTestTimings` at all: they have no use for historical data, so
//! [`run`] only resolves `--repo-id`/`--server-url`/credential and builds
//! a [`RemoteHistoryLookup`] when `--strategy timing` is selected.
//!
//! # `--granularity test`
//!
//! Not yet implemented. `cloud-ci split` runs *before* any test framework
//! executes, so there is no CI report to parse for individual test names —
//! `cloud-ci-reports`' parsers (`junit`/`lcov`/`playwright`/`vitest`) all
//! consume a framework's *output*, not its source files, so they cannot
//! enumerate tests ahead of a run. Per-test enumeration would need a
//! framework-aware static parser (e.g. walking a Playwright/Vitest spec
//! file's AST for `test(...)`/`it(...)` calls) that does not exist in this
//! codebase. `--granularity test` is rejected with a clear "not yet
//! implemented" error rather than silently behaving like `file`.
//!
//! # `--token`/`--repo-id`/`--server-url`/OIDC credential flags
//!
//! Resolved via the same precedence `cloud-ci upload` uses
//! (`crate::upload::resolve_credential` for the bearer credential;
//! `crate::identity::resolve_repo_id`/`resolve_server_url` for the other
//! two, factored out of `crate::identity::resolve_run_identity` since
//! `cloud-ci split` has no use for the rest of the run-identity tuple
//! `sha`/`run_key`/`attempt`), matching this command's documented CLI
//! surface (`docs/design/parallelization.md`: "`cloud-ci split`
//! authenticates with the same machine credentials as `cloud-ci
//! upload`... to read historical timing"). Required only for `--strategy
//! timing`; `file`/`count` never touch any of the three.

use std::collections::HashMap;

use cloud_ci_core::split::{self, HistoryLookup, Item, NoHistoryLookup, ShardCountSpec, Strategy};
use cloud_ci_proto::ingest::v1::GetTestTimingsRequest;

use crate::cli::{Granularity, SplitArgs, SplitStrategy};
use crate::connect_client::{Client, Codec};
use crate::identity::{EnvSource, resolve_repo_id, resolve_server_url};
use crate::upload::{UploadError, expand_glob, resolve_credential};

/// A [`HistoryLookup`] backed by one `GetTestTimings` RPC call
/// (`cloud_ci_proto::ingest::v1`), per this module's "`--strategy timing`"
/// doc section. The call happens once, eagerly, in [`RemoteHistoryLookup::fetch`];
/// [`HistoryLookup::duration_ms`] itself is a synchronous in-memory map
/// lookup, matching the trait's synchronous signature
/// (`cloud_ci_core::split::HistoryLookup` has no `async` variant, by
/// design — see that crate's docs on why the algorithm itself stays pure
/// I/O-free).
pub(crate) struct RemoteHistoryLookup {
    durations_ms: HashMap<String, u64>,
}

impl RemoteHistoryLookup {
    /// Calls `GetTestTimings` for `repo_id`/`file_paths` and buffers the
    /// response. Files absent from the response (no `test_stats` history)
    /// simply have no entry in `durations_ms`, matching
    /// `cloud_ci_core::split::HistoryLookup`'s "unknown duration" contract.
    fn fetch(
        client: &Client,
        repo_id: u64,
        file_paths: &[String],
    ) -> Result<Self, crate::connect_client::CallError> {
        let request = GetTestTimingsRequest {
            repo_id,
            file_paths: file_paths.to_vec(),
            ..Default::default()
        };
        let response: cloud_ci_proto::ingest::v1::GetTestTimingsResponse =
            client.call("GetTestTimings", &request)?;
        let durations_ms = response
            .timings
            .into_iter()
            .map(|timing| (timing.file_path, timing.duration_ms))
            .collect();
        Ok(Self { durations_ms })
    }
}

impl HistoryLookup for RemoteHistoryLookup {
    fn duration_ms(&self, item_name: &str) -> Option<u64> {
        self.durations_ms.get(item_name).copied()
    }
}

pub fn run(args: &SplitArgs, env: &dyn EnvSource) -> Result<(), UploadError> {
    if args.granularity == Granularity::Test {
        return Err(UploadError::new(
            "--granularity test",
            "not yet implemented: per-test splitting needs a framework-aware static test \
             enumerator that doesn't exist in this codebase yet (cloud-ci-reports only parses \
             post-run CI output, not test source files); use --granularity file",
        ));
    }

    let credential = resolve_credential(args.token.as_deref(), env)
        .map_err(|message| UploadError::new("resolve credential", message))?;

    let matched = expand_glob(&args.files, "--files")?;
    let names: Vec<String> = matched
        .into_iter()
        .map(|file| file.path.to_string_lossy().into_owned())
        .collect();

    let strategy = match args.strategy {
        SplitStrategy::Timing => Strategy::Timing,
        SplitStrategy::File => Strategy::File,
        SplitStrategy::Count => Strategy::Count,
    };

    let items: Vec<Item> = if strategy == Strategy::Timing {
        let repo_id = resolve_repo_id(args.repo_id, env).ok_or_else(|| {
            UploadError::new(
                "--repo-id",
                "required for --strategy timing (or CLOUD_CI_REPO_ID)",
            )
        })?;
        let server_url = resolve_server_url(args.server_url.clone(), env).ok_or_else(|| {
            UploadError::new(
                "--server-url",
                "required for --strategy timing (or CLOUD_CI_SERVER_URL)",
            )
        })?;
        let client = Client::new(server_url, Codec::Json, credential);
        let lookup = RemoteHistoryLookup::fetch(&client, repo_id, &names)
            .map_err(|err| UploadError::new("GetTestTimings", err.to_string()))?;
        split::items_from_names(&names, &lookup)
    } else {
        split::items_from_names(&names, &NoHistoryLookup)
    };

    let shard_count =
        split::resolve_shard_count(strategy, ShardCountSpec::Fixed(args.shards), &items)
            .map_err(|err| UploadError::new("resolve shard count", err.to_string()))?;

    if args.index < 1 || args.index > shard_count {
        return Err(UploadError::new(
            "--index",
            format!(
                "--index {} out of range: resolved shard count is {} (--shards {} was clamped \
                 to the 1..64 platform range)",
                args.index, shard_count, args.shards
            ),
        ));
    }

    let assignment = split::assign(strategy, &items, shard_count);
    let shard_files = &assignment[(args.index - 1) as usize];
    for name in shard_files {
        println!("{name}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;
    use crate::cli::{Cli, Command};
    use crate::identity::MapEnv;
    use clap::Parser;
    use cloud_ci_proto::ingest::v1::{FileTiming, GetTestTimingsResponse};

    fn scratch_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cloud-ci-cli-split-test-{label}-{}",
            std::process::id()
        ))
    }

    fn parse(argv: &[&str]) -> SplitArgs {
        let cli = Cli::parse_from(argv);
        let Command::Split(args) = cli.command else {
            unreachable!("expected Command::Split");
        };
        args
    }

    #[test]
    fn parses_documented_shape() {
        let args = parse(&[
            "cloud-ci",
            "split",
            "--strategy",
            "timing",
            "--shards",
            "4",
            "--index",
            "2",
            "--files",
            "tests/**/*.spec.ts",
        ]);
        assert_eq!(args.strategy, SplitStrategy::Timing);
        assert_eq!(args.shards, 4);
        assert_eq!(args.index, 2);
        assert_eq!(args.files, "tests/**/*.spec.ts");
        assert_eq!(args.granularity, Granularity::File);
    }

    #[test]
    fn granularity_test_is_rejected() {
        let tmp = scratch_dir("granularity");
        let _ = fs::create_dir_all(&tmp);
        let _ = fs::write(tmp.join("a.spec.ts"), "");

        let args = parse(&[
            "cloud-ci",
            "split",
            "--strategy",
            "file",
            "--shards",
            "2",
            "--index",
            "1",
            "--files",
            &tmp.join("*.spec.ts").to_string_lossy(),
            "--granularity",
            "test",
        ]);
        match run(&args, &MapEnv::new(&[])) {
            Err(err) => assert!(
                err.to_string().contains("not yet implemented"),
                "got: {err}"
            ),
            Ok(()) => unreachable!("expected --granularity test to be rejected"),
        }

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn glob_matching_no_files_is_an_error() {
        let tmp = scratch_dir("no-match");
        let _ = fs::create_dir_all(&tmp);

        let args = parse(&[
            "cloud-ci",
            "split",
            "--strategy",
            "file",
            "--shards",
            "2",
            "--index",
            "1",
            "--files",
            &tmp.join("*.nonexistent").to_string_lossy(),
        ]);
        match run(&args, &MapEnv::new(&[])) {
            Err(err) => assert!(
                err.to_string().contains("glob matched no files"),
                "got: {err}"
            ),
            Ok(()) => unreachable!("expected a no-match glob error"),
        }

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn index_out_of_range_is_an_error() {
        let tmp = scratch_dir("index-range");
        let _ = fs::create_dir_all(&tmp);
        let _ = fs::write(tmp.join("a.spec.ts"), "");

        let args = parse(&[
            "cloud-ci",
            "split",
            "--strategy",
            "file",
            "--shards",
            "2",
            "--index",
            "3",
            "--files",
            &tmp.join("*.spec.ts").to_string_lossy(),
        ]);
        assert!(run(&args, &MapEnv::new(&[])).is_err());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prints_resolved_shard_files_sorted_by_path() -> Result<(), UploadError> {
        let tmp = scratch_dir("select");
        let _ = fs::create_dir_all(&tmp);
        for name in ["c.spec.ts", "a.spec.ts", "b.spec.ts", "d.spec.ts"] {
            let _ = fs::write(tmp.join(name), "");
        }

        let args = parse(&[
            "cloud-ci",
            "split",
            "--strategy",
            "file",
            "--shards",
            "2",
            "--index",
            "1",
            "--files",
            &tmp.join("*.spec.ts").to_string_lossy(),
        ]);

        // Mirror `run`'s glob expansion directly to assert the matched set
        // without depending on captured stdout.
        let matched = expand_glob(&args.files, "--files")?;
        let mut names: Vec<String> = matched
            .into_iter()
            .map(|f| f.path.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                tmp.join("a.spec.ts").to_string_lossy().into_owned(),
                tmp.join("b.spec.ts").to_string_lossy().into_owned(),
                tmp.join("c.spec.ts").to_string_lossy().into_owned(),
                tmp.join("d.spec.ts").to_string_lossy().into_owned(),
            ]
        );
        // position % 2: a->0, b->1, c->0, d->1 -> shard 1 (index 1) gets a, c
        run(&args, &MapEnv::new(&[]))?;

        let _ = fs::remove_dir_all(&tmp);
        Ok(())
    }

    /// Starts a one-shot fixture server that accepts one `GetTestTimings`
    /// POST and replies with `response` as JSON — same minimal raw-socket
    /// approach as `connect_client.rs`'s own `serve_once` and
    /// `upload.rs`'s fixture server, kept local to this module rather than
    /// reused across crates since each caller's response shape differs.
    fn serve_test_timings(response: &GetTestTimingsResponse) -> std::io::Result<String> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let body = serde_json::to_vec(response).unwrap_or_default();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Read the full request -- headers plus its `Content-Length`
                // body -- before responding. A single, unlooped `read()`
                // (the prior version of this fixture) can return only the
                // headers under scheduling load: directly observed a
                // 202-byte read with no JSON body present, because the
                // client's header and body writes landed in separate TCP
                // segments. Responding and dropping the socket right after
                // that short read, while the client is still writing the
                // rest of the request, leaves unread data sitting in the
                // kernel receive buffer at close time; the OS then answers
                // with a reset instead of a clean FIN, which surfaces to the
                // client as a transport error (observed on macOS: `io:
                // Invalid argument (os error 22)`) -- intermittently, only
                // under load, never in isolation. Draining the declared
                // `Content-Length` before writing the response and closing
                // makes this deterministic regardless of segmentation.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    let Ok(n) = stream.read(&mut chunk) else {
                        break None;
                    };
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(pos + 4);
                    }
                };

                if let Some(header_end) = header_end {
                    let content_length: usize = String::from_utf8_lossy(&buf[..header_end])
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().to_string())
                        })
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);

                    let mut received = buf.len() - header_end;
                    while received < content_length {
                        let Ok(n) = stream.read(&mut chunk) else {
                            break;
                        };
                        if n == 0 {
                            break;
                        }
                        received += n;
                    }

                    let header = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&body);
                    let _ = stream.flush();
                }
            }
        });
        Ok(format!("http://{addr}"))
    }

    /// Proves `RemoteHistoryLookup` fetches real durations for known files
    /// (served by a real HTTP fixture, not an in-process stub) and reports
    /// `None` — not zero — for files `GetTestTimings` has no row for, and
    /// that feeding that lookup into `cloud_ci_core::split`'s real
    /// algorithm produces LPT-bin-packed, timing-aware groupings a plain
    /// `file` round-robin split would never produce for the same input.
    #[test]
    fn timing_strategy_uses_remote_history_with_median_imputation_for_unknown_files()
    -> Result<(), String> {
        let tmp = scratch_dir("timing-remote");
        let _ = fs::create_dir_all(&tmp);
        for name in ["a.spec.ts", "b.spec.ts", "c.spec.ts", "d.spec.ts"] {
            let _ = fs::write(tmp.join(name), "");
        }
        let a = tmp.join("a.spec.ts").to_string_lossy().into_owned();
        let b = tmp.join("b.spec.ts").to_string_lossy().into_owned();
        let c = tmp.join("c.spec.ts").to_string_lossy().into_owned();
        let d = tmp.join("d.spec.ts").to_string_lossy().into_owned();

        // `a` is light, `b` is heavy; `c`/`d` have no `test_stats` history
        // at all, so they are omitted from the fixture response entirely.
        let response = GetTestTimingsResponse {
            timings: vec![
                FileTiming {
                    file_path: a.clone(),
                    duration_ms: 100,
                    ..Default::default()
                },
                FileTiming {
                    file_path: b.clone(),
                    duration_ms: 9000,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let base_url = serve_test_timings(&response).map_err(|e| e.to_string())?;

        let names = vec![a.clone(), b.clone(), c.clone(), d.clone()];
        let client = Client::new(base_url, Codec::Json, None);
        let lookup = RemoteHistoryLookup::fetch(&client, 42, &names).map_err(|e| e.to_string())?;

        assert_eq!(lookup.duration_ms(&a), Some(100));
        assert_eq!(lookup.duration_ms(&b), Some(9000));
        assert_eq!(
            lookup.duration_ms(&c),
            None,
            "a file with no test_stats row must be unknown, not zero"
        );
        assert_eq!(lookup.duration_ms(&d), None);

        let items = split::items_from_names(&names, &lookup);
        let shard_count =
            split::resolve_shard_count(Strategy::Timing, ShardCountSpec::Fixed(2), &items)
                .map_err(|e| e.to_string())?;
        let assignment = split::assign(Strategy::Timing, &items, shard_count);
        assert_eq!(assignment.len(), 2);

        // Median imputation gives both unknown files (c, d) the median of
        // the known durations (100, 9000 -> 4550 each); LPT then keeps the
        // heaviest file (b, 9000ms) with the lightest (a, 100ms) in one
        // shard and both imputed-median files (c, d) together in the
        // other. Plain `file` round-robin on this same four-item input
        // would instead alternate by path (a,c | b,d) -- a different,
        // non-timing-aware grouping -- so this assertion only passes when
        // the real timing data actually drove bin-packing.
        let mut shard_with_b = assignment
            .iter()
            .find(|s| s.contains(&b))
            .ok_or_else(|| "b must be assigned somewhere".to_string())?
            .clone();
        shard_with_b.sort();
        let mut expected_with_b = vec![a.clone(), b.clone()];
        expected_with_b.sort();
        assert_eq!(shard_with_b, expected_with_b);

        let mut other_shard = assignment
            .iter()
            .find(|s| !s.contains(&b))
            .ok_or_else(|| "the other shard must exist".to_string())?
            .clone();
        other_shard.sort();
        let mut expected_other = vec![c.clone(), d.clone()];
        expected_other.sort();
        assert_eq!(other_shard, expected_other);

        let _ = fs::remove_dir_all(&tmp);
        Ok(())
    }
}
