mod agent;
mod cli;
mod connect_client;
mod identity;
mod lint;
mod scope;
mod setup;
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
        Command::Setup(SetupCommand::AllowedOrgs(args)) => match setup::run_allowed_orgs(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("cloud-ci setup allowed-orgs: {err}");
                ExitCode::FAILURE
            }
        },
    }
}
