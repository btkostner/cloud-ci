//! Orchestrates `cloud-ci split`: expands `--files`, computes the shard
//! assignment via `cloud_ci_core::split` (the pure algorithm library, see
//! that crate's docs), and prints `--index`'s file list, one per line, to
//! stdout. Per `docs/design/parallelization.md`'s "`cloud-ci split` (also
//! usable from BYO CI)".
//!
//! # `--strategy timing` this round
//!
//! `timing`'s real data source is the `test_stats` D1 table
//! (`docs/design/analytics.md`), read via a point lookup per matched item.
//! That table has no migration yet, and `cloud-ci-worker` has no query
//! endpoint to serve it — both are out of scope for this round (they depend
//! on the full analytics/Analytics-Engine/rollup pipeline named in
//! parallelization.md's Non-goals). This command therefore always looks up
//! duration history through `cloud_ci_core::split::NoHistoryLookup`, which
//! reports every item as unknown. Per that crate's "Fallback when no
//! history exists" documentation, when *no* item has a known duration
//! `timing` degrades fully to `file` round-robin order — which is exactly
//! what happens on every invocation this round, since no real lookup is
//! wired in yet. This is temporary: once the `test_stats` query endpoint
//! exists, swapping `NoHistoryLookup` for a real `HistoryLookup`
//! implementation (reusing the same `--token`/OIDC credential this command
//! already resolves below) requires no change to the algorithm itself.
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
//! # `--token`/OIDC credential flags
//!
//! Accepted and resolved via the same precedence `cloud-ci upload` uses
//! (`crate::upload::resolve_credential`: explicit `--token` >
//! `CLOUD_CI_TOKEN` > GitHub Actions OIDC > none), matching this command's
//! documented CLI surface (`docs/design/parallelization.md`: "`cloud-ci
//! split` authenticates with the same machine credentials as `cloud-ci
//! upload`... to read historical timing"). The resolved credential is
//! intentionally unused this round — there is nothing to call with it yet
//! (see above) — but it is accepted-but-currently-unused plumbing for the
//! future `test_stats` lookup, not dead code to delete.

use cloud_ci_core::split::{self, Item, NoHistoryLookup, ShardCountSpec, Strategy};

use crate::cli::{Granularity, SplitArgs, SplitStrategy};
use crate::identity::EnvSource;
use crate::upload::{UploadError, expand_glob, resolve_credential};

pub fn run(args: &SplitArgs, env: &dyn EnvSource) -> Result<(), UploadError> {
    if args.granularity == Granularity::Test {
        return Err(UploadError::new(
            "--granularity test",
            "not yet implemented: per-test splitting needs a framework-aware static test \
             enumerator that doesn't exist in this codebase yet (cloud-ci-reports only parses \
             post-run CI output, not test source files); use --granularity file",
        ));
    }

    // Resolved but unused this round — see module docs' "`--token`/OIDC
    // credential flags" section.
    let _credential = resolve_credential(args.token.as_deref(), env)
        .map_err(|message| UploadError::new("resolve credential", message))?;

    let matched = expand_glob(&args.files, "--files")?;
    let names: Vec<String> = matched
        .into_iter()
        .map(|file| file.path.to_string_lossy().into_owned())
        .collect();

    let lookup = NoHistoryLookup;
    let items: Vec<Item> = split::items_from_names(&names, &lookup);

    let strategy = match args.strategy {
        SplitStrategy::Timing => Strategy::Timing,
        SplitStrategy::File => Strategy::File,
        SplitStrategy::Count => Strategy::Count,
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

    use super::*;
    use crate::cli::{Cli, Command};
    use crate::identity::MapEnv;
    use clap::Parser;

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
}
