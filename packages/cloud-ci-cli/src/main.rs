mod cli;
mod connect_client;
mod identity;
mod scope;
mod upload;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};
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
    }
}
