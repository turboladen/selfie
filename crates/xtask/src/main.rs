//! Verification instruments for this repository. See `README.md` beside this
//! crate for what each command does and what its verdicts mean.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod cargo;
#[cfg(test)]
mod control;
mod git;
mod mutate;
mod percommit;
mod proc;
mod workdir;

#[derive(Parser)]
#[command(about = "Verification instruments for the selfie workspace")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run `just clippy` on every commit below the tip, each in its own archive.
    Percommit(percommit::Args),
    /// Apply each mutation in a spec in its own archive and score the named tests.
    Mutate(mutate::Args),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = proc::kill_groups_on_interrupt().and_then(|()| match &cli.command {
        Cmd::Percommit(args) => percommit::run(args),
        Cmd::Mutate(args) => mutate::run(args),
    });
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}
