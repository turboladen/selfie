// Each stream is colored by its own decision. Each run puts one stream on a
// pseudoterminal and the other on a pipe, so console gives colors to the one
// that is a terminal; a line the binary writes carries selfie's styling only
// on that stream.

pub mod common;

use std::{
    fs::File,
    io::{ErrorKind, Read},
    process::Stdio,
    thread::sleep,
    time::{Duration, Instant},
};

use common::{
    FAILED, RunOutput, SELFIE_ENV, sandboxed_std_command, setup_default_test_config, styled,
    write_package,
};
use nix::{
    fcntl::{FcntlArg, OFlag, fcntl},
    pty::openpty,
};
use tempfile::TempDir;

// Long enough for a debug binary on a loaded machine to finish.
const DEADLINE: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

// Run `selfie` with `args` in `temp`'s sandbox, `terminal` on a
// pseudoterminal and the other stream on a pipe, with `env` added.
fn run(temp: &TempDir, args: &[&str], terminal: Stream, env: &[(&str, &str)]) -> RunOutput {
    let pty = openpty(None, None).unwrap();
    fcntl(&pty.master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();
    let mut command = sandboxed_std_command(temp);
    // console counts a terminal without TERM as dumb, and gives it no colors.
    command
        .args(args)
        .env("TERM", "xterm-256color")
        .envs(env.iter().copied())
        .stdin(Stdio::null());
    match terminal {
        Stream::Stdout => command
            .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
            .stderr(Stdio::piped()),
        Stream::Stderr => command
            .stdout(Stdio::piped())
            .stderr(Stdio::from(pty.slave.try_clone().unwrap())),
    };
    let mut child = command.spawn().unwrap();
    // Only the child holds the terminal now, so its end shows on the master.
    drop(pty.slave);
    let mut master = File::from(pty.master);
    // The pipe is read while the child runs, on a thread of its own, so a run
    // that writes more than a pipe holds does not block waiting for a reader.
    // Only the stream left on a pipe has a handle here.
    let mut pipe: Box<dyn Read + Send> = match (child.stdout.take(), child.stderr.take()) {
        (Some(stdout), None) => Box::new(stdout),
        (None, Some(stderr)) => Box::new(stderr),
        handles => panic!("expected one piped stream, got {handles:?}"),
    };
    let reader = std::thread::spawn(move || {
        let mut on_pipe = Vec::new();
        pipe.read_to_end(&mut on_pipe).unwrap();
        on_pipe
    });

    let mut on_terminal = Vec::new();
    let start = Instant::now();
    let status = loop {
        drain(&mut master, &mut on_terminal);
        if let Some(status) = child.try_wait().unwrap() {
            drain(&mut master, &mut on_terminal);
            break status;
        }
        if start.elapsed() > DEADLINE {
            let _ = child.kill();
            panic!(
                "the run never ended; it wrote:\n{}",
                String::from_utf8_lossy(&on_terminal)
            );
        }
        sleep(Duration::from_millis(10));
    };
    let on_pipe = reader.join().unwrap();
    let on_terminal = String::from_utf8_lossy(&on_terminal).into_owned();
    let on_pipe = String::from_utf8_lossy(&on_pipe).into_owned();
    let (stdout, stderr) = match terminal {
        Stream::Stdout => (on_terminal, on_pipe),
        Stream::Stderr => (on_pipe, on_terminal),
    };
    assert!(
        !stderr
            .chars()
            .any(|c| ('\u{2800}'..='\u{28ff}').contains(&c)),
        "the run drew a spinner, so it waited on something:\n{stderr}"
    );
    RunOutput {
        code: status.code(),
        stdout,
        stderr,
    }
}

fn drain(master: &mut File, into: &mut Vec<u8>) {
    let mut buf = [0_u8; 4096];
    loop {
        match master.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => into.extend_from_slice(&buf[..n]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            // Linux reports a closed slave side as EIO.
            Err(_) => return,
        }
    }
}

// A sandbox with one valid spec, `tool`, which waits on nothing: it has no
// check command, and `spec validate` runs none.
fn sandbox() -> TempDir {
    let temp = setup_default_test_config();
    write_package(&temp, "tool", "true", None, &[]);
    temp
}

// `spec validate tool` answers on stdout with a ✓ result, and `spec info
// nope` refuses on stderr with a ✗ error.
const ANSWER: &[&str] = &["spec", "validate", "tool"];
const REFUSAL: &[&str] = &["spec", "info", "nope"];

#[test]
fn stdout_on_a_terminal_is_styled_and_stderr_on_a_pipe_is_not() {
    let temp = sandbox();

    let answer = run(&temp, ANSWER, Stream::Stdout, &[]);
    let refusal = run(&temp, REFUSAL, Stream::Stdout, &[]);

    assert_eq!(answer.code, Some(0), "{answer:?}");
    assert!(
        answer.stdout.contains('✓') && styled(&answer.stdout),
        "{answer:?}"
    );
    assert_eq!(refusal.code, Some(FAILED), "{refusal:?}");
    assert!(refusal.stderr.contains('✗'), "{refusal:?}");
    assert!(!styled(&refusal.stderr), "{refusal:?}");
}

#[test]
fn stderr_on_a_terminal_is_styled_and_stdout_on_a_pipe_is_not() {
    let temp = sandbox();

    let answer = run(&temp, ANSWER, Stream::Stderr, &[]);
    let refusal = run(&temp, REFUSAL, Stream::Stderr, &[]);

    assert_eq!(answer.code, Some(0), "{answer:?}");
    assert!(answer.stdout.contains('✓'), "{answer:?}");
    assert!(!styled(&answer.stdout), "{answer:?}");
    assert_eq!(refusal.code, Some(FAILED), "{refusal:?}");
    assert!(
        refusal.stderr.contains('✗') && styled(&refusal.stderr),
        "{refusal:?}"
    );
}

// A run that writes more than a pipe holds to the piped stream still ends,
// since the pipe is read while the run writes.
#[test]
fn a_run_writing_more_than_a_pipe_holds_ends() {
    let temp = sandbox();
    let description = "d".repeat(100);
    for index in 0..1000 {
        std::fs::write(
            temp.path()
                .join("packages")
                .join(format!("tool{index}.yaml")),
            format!(
                "name: tool{index}\ndescription: {description}\nenvironments:\n  \
                 {SELFIE_ENV}:\n    install: \"true\"\n"
            ),
        )
        .unwrap();
    }

    let listed = run(&temp, &["spec", "list"], Stream::Stderr, &[]);

    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert!(
        listed.stdout.len() > 64 * 1024,
        "{} bytes",
        listed.stdout.len()
    );
}

// `CLICOLOR_FORCE` gives both streams colors, pipe or not, and `--no-color`
// still turns them off.
#[test]
fn clicolor_force_styles_a_pipe_unless_colors_are_off() {
    let temp = sandbox();
    let force = [("CLICOLOR_FORCE", "1")];

    let forced = run(&temp, REFUSAL, Stream::Stdout, &force);
    let answer = run(&temp, ANSWER, Stream::Stderr, &force);
    let mut off = vec!["--no-color"];
    off.extend_from_slice(REFUSAL);
    let refused_plain = run(&temp, &off, Stream::Stderr, &force);

    assert!(styled(&forced.stderr), "{forced:?}");
    assert!(styled(&answer.stdout), "{answer:?}");
    assert!(refused_plain.stderr.contains('✗'), "{refused_plain:?}");
    assert!(!styled(&refused_plain.stderr), "{refused_plain:?}");
}

// The `--verbose` log is written by tracing on stderr, and follows stderr's
// colors: none on a pipe, or with `--no-color` on a terminal, and tracing's
// own on a terminal.
#[test]
fn the_verbose_log_follows_stderrs_colors() {
    let temp = sandbox();
    let verbose: Vec<&str> = ["--verbose"].iter().chain(ANSWER).copied().collect();
    let mut plain: Vec<&str> = vec!["--no-color"];
    plain.extend_from_slice(&verbose);

    let piped = run(&temp, &verbose, Stream::Stdout, &[]);
    let terminal = run(&temp, &verbose, Stream::Stderr, &[]);
    let off = run(&temp, &plain, Stream::Stderr, &[]);

    // Whether any of the log's own lines carries color.
    let log_styled = |written: &RunOutput| {
        assert!(written.stderr.contains("DEBUG"), "no log: {written:?}");
        written
            .stderr
            .lines()
            .any(|line| line.contains("DEBUG") && styled(line))
    };
    assert!(!log_styled(&piped), "{piped:?}");
    assert!(log_styled(&terminal), "{terminal:?}");
    assert!(!log_styled(&off), "{off:?}");
}
