mod cli;
mod identity;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};
use identity::{RealEnv, resolve_run_identity};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Upload(args) => {
            let identity = match resolve_run_identity(&args.run_identity_flags(), &RealEnv) {
                Ok(identity) => identity,
                Err(missing) => {
                    eprintln!(
                        "cloud-ci upload: missing required run identity: {}",
                        missing.join(", ")
                    );
                    return ExitCode::FAILURE;
                }
            };

            println!(
                "cloud-ci upload: job={} repo_id={} sha={} run_key={} attempt={} server_url={} reports={} sites={} checks={}",
                args.job,
                identity.repo_id,
                identity.sha,
                identity.run_key,
                identity.attempt,
                identity.server_url,
                args.reports.len(),
                args.sites.len(),
                args.checks.len(),
            );
            ExitCode::SUCCESS
        }
    }
}
