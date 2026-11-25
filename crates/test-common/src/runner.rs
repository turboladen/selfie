//! Command outputs for tests that build a runner's answer by hand.
//!
//! The scripted runner itself is `selfie::commands::FakeCommandRunner`, which
//! this crate re-exports.

use std::time::Duration;

use selfie::commands::CommandOutput;

/// A pipe's worth of output past the bound: 3000 bytes of filler, a
/// `TOKEN=<secret>` line, 3000 more, then `last` on a line of its own.
///
/// The bound elides the secret and keeps `last`, so a test asserts the first is
/// absent and the second, its control, present.
#[must_use]
pub fn secret_between_filler(secret: &str, last: &str) -> Vec<u8> {
    let filler = "x".repeat(3_000);
    format!("{filler}\nTOKEN={secret}\n{filler}\n{last}\n").into_bytes()
}

/// A `CommandOutput` for a command that exited with `exit_code` and wrote
/// `stdout` and `stderr`.
#[must_use]
pub fn command_output(exit_code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> CommandOutput {
    use std::os::unix::process::ExitStatusExt as _;
    // Wait status encodes the exit code in the high byte.
    let status = std::process::ExitStatus::from_raw(exit_code << 8);

    CommandOutput::from_parts(status, stdout, stderr, Duration::ZERO)
}
