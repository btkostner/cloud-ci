mod agent;
mod cli;
mod connect_client;
mod identity;
mod lint;
mod scope;
mod setup;
mod setup_github_app;
mod setup_wizard;
mod split;
mod upload;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command, SetupCommand};
use identity::RealEnv;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Upload(args) => match upload::run(&args, &RealEnv) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("cloud-ci upload: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Split(args) => match split::run(&args, &RealEnv) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("cloud-ci split: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Lint(args) => match lint::run(&args) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(err) => {
                eprintln!("cloud-ci lint: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Agent(args) => match agent::run(&args, &RealEnv) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("cloud-ci agent: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Setup(args) => match args.command {
            Some(SetupCommand::AllowedOrgs(a)) => match setup::run_allowed_orgs(&a) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("cloud-ci setup allowed-orgs: {err}");
                    ExitCode::FAILURE
                }
            },
            Some(SetupCommand::GithubApp(a)) => match setup_github_app::run_github_app(&a) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("cloud-ci setup github-app: {err}");
                    ExitCode::FAILURE
                }
            },
            None => {
                let exe = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.to_str().map(str::to_string))
                    .unwrap_or_else(|| "cloud-ci".to_string());
                let report = setup_wizard::run_wizard(
                    &setup_wizard::RealRunner,
                    &setup_wizard::RealFs,
                    &args.wizard,
                    &exe,
                );
                print!("{}", report.render(args.wizard.dry_run));
                if report.failed() {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
        },
    }
}
