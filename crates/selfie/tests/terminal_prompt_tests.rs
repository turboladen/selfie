//! A command that asks a question on `/dev/tty` reaches the runner's handler,
//! and one nothing can answer fails instead of waiting behind the caller's
//! spinner.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use selfie::commands::{
    CommandError, CommandRunner, PromptHandler, ShellCommandRunner, TerminalAnswer, TerminalReply,
};
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(30);

// A command asks the way `sudo` does: the question goes to the terminal and the
// answer is read from it, never from stdin.
const ASKS: &str =
    r#"printf 'Password: ' > /dev/tty; IFS= read -r answer < /dev/tty; printf 'got:%s' "$answer""#;

fn runner() -> ShellCommandRunner {
    ShellCommandRunner::new(ShellCommandRunner::default_shell(), TIMEOUT)
}

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

// A handler that records each question it is asked and answers `answer`.
fn answering(answer: &'static str) -> (PromptHandler, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    let handler = PromptHandler::new(move |prompt| {
        seen.lock().unwrap().push(prompt.question().to_string());
        async move { TerminalReply::Answer(TerminalAnswer::new(answer.to_string())) }
    });
    (handler, asked)
}

#[tokio::test]
async fn a_question_on_the_terminal_is_answered_through_the_handler() {
    let (handler, asked) = answering("hunter2-correct");
    let dir = dir();

    let output = runner()
        .with_prompt_handler(handler)
        .execute(ASKS, dir.path(), &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(output.stdout_str(), "got:hunter2-correct");
    assert_eq!(*asked.lock().unwrap(), vec!["Password:".to_string()]);
}

#[tokio::test]
async fn a_streaming_command_is_answered_the_same_way() {
    let (handler, _) = answering("hunter2-correct");
    let dir = dir();
    let (sender, _receiver) = tokio::sync::mpsc::channel(16);

    let output = runner()
        .with_prompt_handler(handler)
        .execute_streaming(ASKS, dir.path(), TIMEOUT, sender, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(output.stdout_str(), "got:hunter2-correct");
}

#[tokio::test]
async fn two_questions_in_one_command_are_both_answered() {
    let (handler, asked) = answering("same");
    let dir = dir();
    let command = format!("{ASKS}; printf ' '; {ASKS}");

    let output = runner()
        .with_prompt_handler(handler)
        .execute(&command, dir.path(), &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(output.stdout_str(), "got:same got:same");
    assert_eq!(asked.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_question_nothing_can_answer_fails_the_command_at_once() {
    let dir = dir();
    let started = Instant::now();

    let error = runner()
        .execute(ASKS, dir.path(), &CancellationToken::new())
        .await
        .unwrap_err();

    // The timeout is 30 seconds, so a command left waiting would fail on that
    // instead, with a different error.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "waited {:?}",
        started.elapsed()
    );
    let CommandError::InputRequired { prompt, .. } = &error else {
        panic!("expected InputRequired, got {error:?}");
    };
    assert_eq!(prompt.as_str().trim(), "Password:");
}

#[tokio::test]
async fn a_declined_question_fails_the_command_too() {
    let handler = PromptHandler::new(|_| async { TerminalReply::Decline });
    let dir = dir();

    let error = runner()
        .with_prompt_handler(handler)
        .execute(ASKS, dir.path(), &CancellationToken::new())
        .await
        .unwrap_err();

    assert!(
        matches!(error, CommandError::InputRequired { .. }),
        "got {error:?}"
    );
}

#[tokio::test]
async fn a_failure_names_the_question_and_never_the_answer() {
    let dir = dir();

    let error = runner()
        .execute(ASKS, dir.path(), &CancellationToken::new())
        .await
        .unwrap_err();

    let sentence = error.to_string();
    assert!(sentence.contains("Password:"), "{sentence}");
    assert!(sentence.contains("no terminal to ask on"), "{sentence}");
}

#[tokio::test]
async fn stdout_and_stderr_are_still_pipes_and_stdin_is_still_closed() {
    let dir = dir();

    let output = runner()
        .execute(
            "[ -t 0 ] && printf 'stdin-tty ' ; [ -t 1 ] && printf 'stdout-tty ' ; [ -t 2 ] && printf 'stderr-tty ' ; printf done",
            dir.path(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(output.stdout_str(), "done");
}

#[tokio::test]
async fn a_command_that_asks_nothing_is_not_held_up() {
    let dir = dir();
    let started = Instant::now();

    let output = runner()
        .execute("printf plain", dir.path(), &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(output.stdout_str(), "plain");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_question_ended_by_a_newline_is_not_taken_for_a_prompt() {
    let dir = dir();

    let output = runner()
        .execute(
            "printf 'a note for the user\\n' > /dev/tty; sleep 1; printf finished",
            dir.path(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(output.stdout_str(), "finished");
}

#[tokio::test]
async fn ending_a_command_ends_what_it_started() {
    // The sleep ignores SIGHUP, which is what closing the terminal sends. Without
    // that it would end on the hangup alone and the kill would go untested.
    let dir = dir();
    let pid_file = dir.path().join("pid");
    let command = format!(
        "trap '' HUP; sleep 60 & echo $! > '{}'; printf 'Password: ' > /dev/tty; wait",
        pid_file.display()
    );

    let _ = runner()
        .execute(&command, dir.path(), &CancellationToken::new())
        .await
        .unwrap_err();

    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let pid = nix::unistd::Pid::from_raw(pid);
    // The kill is a signal, so the process is gone a moment later.
    let mut alive = true;
    for _ in 0..50 {
        alive = nix::sys::signal::kill(pid, None).is_ok();
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive, "the background sleep outlived its command");
}

#[test]
fn an_answer_does_not_print_itself() {
    let answer = TerminalAnswer::new("correct-horse-battery".to_string());

    let shown = format!("{answer:?}");

    assert!(!shown.contains("correct"), "{shown}");
    assert!(shown.contains("21 bytes"), "{shown}");
}

#[tokio::test]
async fn an_answer_is_not_echoed_into_the_next_question() {
    let texts = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&texts);
    let handler = PromptHandler::new(move |prompt| {
        seen.lock().unwrap().push(prompt.text().to_string());
        async { TerminalReply::Answer(TerminalAnswer::new("first-secret-value".to_string())) }
    });
    let dir = dir();
    // Neither question turns echo off, as `sudo` does for itself.
    let command = format!("{ASKS}; printf ' '; {ASKS}");

    runner()
        .with_prompt_handler(handler)
        .execute(&command, dir.path(), &CancellationToken::new())
        .await
        .unwrap();

    let texts = texts.lock().unwrap();
    assert_eq!(texts.len(), 2);
    assert!(
        !texts[1].contains("first-secret-value"),
        "the first answer came back as {:?}",
        texts[1]
    );
}
