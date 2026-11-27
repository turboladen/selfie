// How much a streamed command makes the runner allocate, measured by a
// counting global allocator. It lives in a binary of its own because the
// allocator is process-wide: any other test running beside it would count too.

use std::{path::Path, time::Duration};

use peak_alloc::PeakAlloc;
use selfie::commands::{CommandRunner, ShellCommandRunner};
use tokio_util::sync::CancellationToken;

#[global_allocator]
static ALLOCATOR: PeakAlloc = PeakAlloc;

// Ten times the bound below on each pipe, so a runner that kept either pipe
// whole would allocate far past it.
const PIPE_BYTES: usize = 10 * 1024 * 1024;

// Generous beside the ~8 KB of windows, since frames, the shell and tokio
// allocate too (about 45 KB at peak in all), and far under one pipe.
const PEAK_BOUND: usize = 1024 * 1024;

#[tokio::test]
async fn streaming_ten_megabytes_per_pipe_allocates_under_a_megabyte() {
    let runner =
        ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));
    // One slot, and a reader that drops each chunk, so the channel holds at
    // most a frame or two and the peak is the runner's.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let reader = tokio::spawn(async move {
        let mut relayed = 0;
        while let Some(chunk) = rx.recv().await {
            relayed += match chunk {
                selfie::commands::OutputChunk::Stdout(text)
                | selfie::commands::OutputChunk::Stderr(text) => text.len(),
            };
        }
        relayed
    });
    let script = format!(
        "head -c {PIPE_BYTES} /dev/zero | tr '\\0' a; head -c {PIPE_BYTES} /dev/zero | tr '\\0' b >&2"
    );

    let before = ALLOCATOR.current_usage();
    ALLOCATOR.reset_peak_usage();
    let output = runner
        .execute_streaming(
            &script,
            Path::new("."),
            Duration::from_secs(30),
            tx,
            &CancellationToken::new(),
        )
        .await
        .expect("command failed");
    let peak = ALLOCATOR.peak_usage() - before;

    assert!(output.is_success(), "exit code {:?}", output.code());
    // Positive control: both pipes really went through the runner.
    assert_eq!(reader.await.unwrap(), 2 * PIPE_BYTES);
    assert!(peak < PEAK_BOUND, "{peak} bytes allocated at peak");
}
