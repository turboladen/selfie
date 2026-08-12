//! Static shell completion scripts for the selfie CLI.
//!
//! These complete commands, subcommands, options and flags, but not package
//! names. Package names need dynamic completions, which clap serves from the
//! `COMPLETE` env var without going through this module at all.
//!
//! `docs/getting-started.md` has the per-shell install paths for both.

use std::io;

use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::ClapCli;

/// Write a completion script for `shell` to stdout.
pub fn generate_completion(shell: Shell) {
    let mut cmd = ClapCli::command();
    let cmd_name = env!("CARGO_BIN_NAME");

    generate(shell, &mut cmd, cmd_name, &mut io::stdout());
}
