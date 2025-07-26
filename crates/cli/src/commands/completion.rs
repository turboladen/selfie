//! Shell completion generation for the selfie CLI
//!
//! This module provides functionality to generate shell completion scripts
//! for various shells including bash, zsh, fish, and `PowerShell`. The completions
//! enable tab completion for commands, subcommands, options, and flags.
//!
//! # Usage
//!
//! Users can generate completion scripts and install them for their shell:
//!
//! ```bash
//! # Generate bash completions
//! selfie completion bash > ~/.local/share/bash-completion/completions/selfie
//!
//! # Generate zsh completions
//! selfie completion zsh > ~/.zfunc/_selfie
//!
//! # Generate fish completions
//! selfie completion fish > ~/.config/fish/completions/selfie.fish
//! ```

use std::io;

use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::ClapCli;

/// Generate shell completion script and write to stdout
///
/// This function creates a completion script for the specified shell
/// and outputs it to stdout. The generated script provides tab completion
/// for all selfie commands, subcommands, options, and flags.
///
/// # Arguments
///
/// * `shell` - The target shell to generate completions for
///
/// # Examples
///
/// ```bash
/// # Generate bash completions
/// selfie completion bash
///
/// # Generate zsh completions
/// selfie completion zsh
/// ```
///
/// # Shell Installation
///
/// After generating the completion script, users need to install it
/// in the appropriate location for their shell:
///
/// ## Bash
/// ```bash
/// selfie completion bash > ~/.local/share/bash-completion/completions/selfie
/// # or
/// selfie completion bash > /usr/local/share/bash-completion/completions/selfie
/// ```
///
/// ## Zsh
/// ```bash
/// selfie completion zsh > ~/.zfunc/_selfie
/// # Then add to ~/.zshrc: fpath=(~/.zfunc $fpath)
/// ```
///
/// ## Fish
/// ```bash
/// selfie completion fish > ~/.config/fish/completions/selfie.fish
/// ```
///
/// ## `PowerShell`
/// ```powershell
/// selfie completion powershell > selfie.ps1
/// # Then source the file in your PowerShell profile
/// ```
pub fn generate_completion(shell: Shell) {
    let mut cmd = ClapCli::command();
    let cmd_name = env!("CARGO_BIN_NAME"); // Use the binary name from Cargo

    generate(shell, &mut cmd, cmd_name, &mut io::stdout());
}
