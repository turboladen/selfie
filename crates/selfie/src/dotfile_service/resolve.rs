//! Resolving a dotfile's content at apply time.
//!
//! Secret-bearing content is produced here and held only in memory: it is
//! compared against the target directly and never recorded. See ADR-0003.
//!
//! Nothing in this module puts resolved content into an error, a warning, or any
//! other value that reaches the event stream. The bytes leave only as
//! [`ResolvedContent::bytes`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::deploy::resolve_source_path;
use super::template;
use crate::commands::{BoundedText, CommandRunner};
use crate::fs::filesystem::FileSystem;
use crate::package::{ContentSource, DotfileEntry, InvalidEntry};
use crate::paths::is_within;

/// Upper bound on resolved content.
///
/// This bounds what selfie compares and writes, not what the command produces:
/// the command runner buffers a command's whole output before this check can run,
/// as it already does for every install and check command. A genuinely unbounded
/// provider is therefore still bounded only by the runner's own behavior.
const MAX_CONTENT_BYTES: usize = 8 * 1024 * 1024;

/// Content resolved for one dotfile entry.
pub(crate) struct ResolvedContent {
    /// The file's content, exactly as produced.
    pub bytes: Vec<u8>,
    /// Non-fatal advisories. Never contains a resolved value — only var names.
    pub warnings: Vec<String>,
}

/// Prints the content's length, never the content.
///
/// Hand-written rather than derived because a derived `Debug` is a second exit
/// for the bytes, opened silently by any future `{:?}`, `unwrap_err()`, or
/// `expect()` on a `Result<ResolvedContent, _>`. The module header claims the
/// bytes leave only as `ResolvedContent::bytes`; this is what makes that true
/// rather than merely observed.
impl std::fmt::Debug for ResolvedContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedContent")
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .field("warnings", &self.warnings)
            .finish()
    }
}

/// Why an entry's content could not be resolved.
///
/// Every variant names commands and var names, which are references drawn from
/// the package file, and stderr, which is forwarded on failure only. None
/// includes resolved content.
#[derive(Debug, Error)]
pub(crate) enum ResolveError {
    #[error("dotfile command '{command}' failed: {stderr}")]
    CommandFailed { command: String, stderr: String },
    #[error("dotfile var '{name}' command failed: {stderr}")]
    BindingFailed { name: String, stderr: String },
    #[error("dotfile command '{command}' produced no output")]
    EmptyOutput { command: String },
    #[error("dotfile var '{name}' produced no output")]
    EmptyBinding { name: String },
    #[error("dotfile content exceeds {MAX_CONTENT_BYTES} bytes")]
    TooLarge,
    // The field is `template` rather than `source` because thiserror treats a
    // field of that name as the error's `source()`.
    #[error("dotfile template '{template}' cannot be read: {message}")]
    TemplateUnreadable { template: String, message: String },
    #[error("dotfile for '{target}' has no content to resolve")]
    NotSecretBearing { target: String },
    #[error("dotfile template '{template}' escapes the package directory")]
    TemplateEscapesPackage { template: String },
}

/// Resolve a template entry's path, refusing one that escapes `base_dir`.
///
/// The same runtime containment check the repository-file path applies, and it
/// resolves against the same base via [`resolve_source_path`]. Apply never runs
/// validation, so the static `..` check on `source` is not a gate — a package
/// that was never validated reaches here intact, and without this a crafted
/// `source` would splice the contents of a file outside the package directory
/// into a deployed dotfile.
///
/// The check is **lexical**: it resolves `.` and `..` textually and does not
/// follow symlinks, so a symlink planted inside the package directory still
/// reaches its target. That is not the security boundary here — a package that
/// can plant a symlink can also just run an arbitrary `command:` — but the limit
/// is worth stating rather than implying the check is stronger than it is.
fn template_path(source: &str, base_dir: &Path) -> Result<PathBuf, ResolveError> {
    let path = resolve_source_path(base_dir, source);

    if is_within(&path, base_dir) {
        Ok(path)
    } else {
        Err(ResolveError::TemplateEscapesPackage {
            template: source.to_string(),
        })
    }
}

/// Everything that can be refused without running a command or reading a file.
///
/// Applied before a dry run reports what it would do, so a preview declines
/// exactly what a real apply declines instead of promising to run commands for an
/// entry that can never deploy.
pub(crate) fn check_resolvable(entry: &DotfileEntry, base_dir: &Path) -> Result<(), ResolveError> {
    match entry.content_source() {
        Ok(ContentSource::Template { source, .. }) => template_path(source, base_dir).map(|_| ()),
        // A provider has no path to contain, and a repository file is not
        // resolved here at all.
        Ok(ContentSource::Provider(_) | ContentSource::RepoFile(_)) => Ok(()),
        // An entry that cannot deploy is refused by `handle_apply` before this
        // module is reached, so there is nothing left to decide here.
        //
        // The variants are named rather than matched with `Err(_)` so that claim
        // has to be re-checked when a new one appears: a reason that is *not*
        // refused upstream would need a decision here, and `_` would silently
        // make it "resolvable". Naming them costs a line and turns that into a
        // build failure.
        Err(InvalidEntry::Shape | InvalidEntry::UnknownKeys(_) | InvalidEntry::VarName(_)) => {
            Ok(())
        }
    }
}

/// Resolve an entry's content, running any commands it declares.
///
/// Commands run with their working directory set to `base_dir`, the same base
/// against which repository sources resolve.
pub(crate) async fn resolve_content<F, CR>(
    entry: &DotfileEntry,
    base_dir: &Path,
    filesystem: &F,
    runner: &CR,
    timeout: Duration,
    token: &CancellationToken,
) -> Result<ResolvedContent, ResolveError>
where
    F: FileSystem,
    CR: CommandRunner,
{
    match entry.content_source() {
        // Repo-file entries take the checksum-and-deploy-state path, and an
        // invalid entry is not deployable at all. Returning an error rather than
        // panicking: the invariant that keeps these out is enforced by a caller in
        // another module, which is too far away to be worth a panic.
        Ok(ContentSource::RepoFile(_))
        | Err(InvalidEntry::Shape | InvalidEntry::UnknownKeys(_) | InvalidEntry::VarName(_)) => {
            Err(ResolveError::NotSecretBearing {
                target: entry.target().to_string(),
            })
        }

        Ok(ContentSource::Provider(command)) => {
            let bytes = run_capture(command, base_dir, runner, timeout, token)
                .await
                .map_err(|stderr| ResolveError::CommandFailed {
                    command: command.to_string(),
                    stderr,
                })?;

            // Zero length is an error regardless of exit code: writing an empty
            // file over a credentials target is destructive, and empty output
            // almost always means a failure that did not set a non-zero status.
            // The check is on length, not on trimmed length, so no rule about
            // whitespace leaks into a path that must stay byte-exact.
            if bytes.is_empty() {
                return Err(ResolveError::EmptyOutput {
                    command: command.to_string(),
                });
            }
            if bytes.len() > MAX_CONTENT_BYTES {
                return Err(ResolveError::TooLarge);
            }

            Ok(ResolvedContent {
                bytes,
                warnings: Vec::new(),
            })
        }

        Ok(ContentSource::Template { source, vars }) => {
            let template_path = template_path(source, base_dir)?;

            let text = filesystem.read_file(&template_path).map_err(|e| {
                ResolveError::TemplateUnreadable {
                    template: source.to_string(),
                    message: e.to_string(),
                }
            })?;

            // Built fresh for this entry. Sharing or reusing a binding map across
            // entries would splice one entry's secret into another entry's file.
            let mut bindings: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            let mut warnings = Vec::new();

            // BTreeMap iterates in key order, so bindings resolve — and failures
            // are reported — deterministically.
            for (name, command) in vars {
                let value = run_capture(command, base_dir, runner, timeout, token)
                    .await
                    .map_err(|stderr| ResolveError::BindingFailed {
                        name: name.clone(),
                        stderr,
                    })?;

                // A structurally valid file holding an empty credential is worse
                // than a loud failure.
                if value.is_empty() {
                    return Err(ResolveError::EmptyBinding { name: name.clone() });
                }

                // Values are spliced verbatim, so a line break can add structure
                // rather than merely corrupt it. Refusing outright would break
                // legitimate multi-line values such as private keys, so this warns
                // and substitutes. The warning names the binding, never the value.
                if value.contains(&b'\n') {
                    warnings.push(format!(
                        "dotfile var '{name}' contains a line break; it will be substituted \
                         as-is and may add structure to the rendered file"
                    ));
                }

                bindings.insert(name.clone(), value);
            }

            let bytes = template::render(&text, &bindings);
            if bytes.len() > MAX_CONTENT_BYTES {
                return Err(ResolveError::TooLarge);
            }

            Ok(ResolvedContent { bytes, warnings })
        }
    }
}

/// Run a command in `base_dir`, returning stdout, or stderr on failure.
///
/// stderr is returned only on failure: a command invoked with a verbose or debug
/// flag can echo secret material there, and on the success path it has no
/// purpose.
///
/// The error is rendered with `Display`, never `Debug`. No `CommandError`
/// variant carries stdout today, so `Debug` would leak nothing — but `Display`
/// is what each variant's `#[error(...)]` curates, and `Debug` prints whatever
/// fields a future variant happens to add.
///
/// Both exits go through [`BoundedText`] and then unwrap to a `String`, because
/// [`ResolveError`]'s fields are `String`. The bound is therefore applied here
/// and not enforced by the error type — unlike `CommandFailure::ExecutionFailed`,
/// whose field *is* a `BoundedText`. A new failure exit added to this function
/// has to call `bound` itself; nothing will stop it if it does not.
///
/// A command that ran fine but whose output could not be read to the end fails
/// here too, as [`CommandError::OutputReadFailed`](crate::commands::CommandError).
/// That matters more on this path than anywhere else in selfie, and it matters at
/// **both** of this function's call sites:
///
/// - For a provider, a truncated read is a truncated credential, and the
///   `MAX_CONTENT_BYTES` cap is a *maximum* — a short buffer sails under it.
/// - For a var binding it is worse: a prefix of a credential is still non-empty,
///   so the `is_empty` check passes, and truncation only makes the rendered file
///   *smaller*, so the cap on the render passes too. Nothing downstream of here
///   can tell a truncated binding from a short one.
async fn run_capture<CR: CommandRunner>(
    command: &str,
    base_dir: &Path,
    runner: &CR,
    timeout: Duration,
    token: &CancellationToken,
) -> Result<Vec<u8>, String> {
    let output = runner
        .execute_in_dir(command, base_dir, timeout, token)
        .await
        .map_err(|e| BoundedText::bound(e.to_string().as_bytes()).into_string())?;

    if output.is_success() {
        // Consumed rather than borrowed-and-copied: `stdout().to_vec()` left the
        // provider's whole output alive twice at peak, and on this path that
        // output is a credential. One buffer is also one thing to zeroize later
        // rather than two.
        Ok(output.into_stdout())
    } else {
        Err(BoundedText::bound(output.stderr()).into_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::runner::{CommandError, CommandOutput};
    use crate::fs::MockFileSystem;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;

    const TIMEOUT: Duration = Duration::from_secs(5);
    const BASE: &str = "/pkg";

    /// A runner with a fixed answer per command, recording what it was asked.
    ///
    /// Deliberately not `test_common::FakeCommandRunner`, which is the same thing
    /// and is used by this crate's integration tests. `test-common` depends on
    /// `selfie`, so inside a unit-test build of `selfie` it links a *second* copy
    /// of the lib; its `FakeCommandRunner` then implements a different
    /// `CommandRunner` trait than the one in scope here and will not satisfy the
    /// bound ("multiple different versions of crate `selfie` in the dependency
    /// graph"). Integration tests under `tests/` link the real lib and do share it.
    #[derive(Default, Clone)]
    struct FakeRunner {
        /// command -> (exit code, stdout, stderr)
        responses: std::collections::HashMap<String, (i32, Vec<u8>, Vec<u8>)>,
        /// Commands whose output cannot be read to the end.
        read_failing: std::collections::HashSet<String>,
        calls: std::sync::Arc<std::sync::Mutex<Vec<(String, PathBuf)>>>,
    }

    impl FakeRunner {
        fn succeeding(command: &str, stdout: &[u8]) -> Self {
            let mut this = Self::default();
            this.responses
                .insert(command.to_string(), (0, stdout.to_vec(), Vec::new()));
            this
        }

        fn failing(command: &str, stderr: &[u8]) -> Self {
            let mut this = Self::default();
            this.responses
                .insert(command.to_string(), (1, Vec::new(), stderr.to_vec()));
            this
        }

        fn with(mut self, command: &str, stdout: &[u8]) -> Self {
            self.responses
                .insert(command.to_string(), (0, stdout.to_vec(), Vec::new()));
            self
        }

        /// The command runs, but its stdout pipe dies part-way through.
        fn read_failing(command: &str) -> Self {
            let mut this = Self::default();
            this.read_failing.insert(command.to_string());
            this
        }

        fn also_read_failing(mut self, command: &str) -> Self {
            self.read_failing.insert(command.to_string());
            self
        }

        fn calls(&self) -> Vec<(String, PathBuf)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        async fn is_command_available(&self, _command: &str) -> bool {
            true
        }

        async fn execute(
            &self,
            command: &str,
            token: &CancellationToken,
        ) -> Result<CommandOutput, CommandError> {
            self.execute_in_dir(command, Path::new("."), TIMEOUT, token)
                .await
        }

        async fn execute_with_timeout(
            &self,
            command: &str,
            timeout: Duration,
            token: &CancellationToken,
        ) -> Result<CommandOutput, CommandError> {
            self.execute_in_dir(command, Path::new("."), timeout, token)
                .await
        }

        async fn execute_in_dir(
            &self,
            command: &str,
            working_dir: &Path,
            _timeout: Duration,
            _token: &CancellationToken,
        ) -> Result<CommandOutput, CommandError> {
            self.calls
                .lock()
                .unwrap()
                .push((command.to_string(), working_dir.to_path_buf()));

            if self.read_failing.contains(command) {
                return Err(CommandError::OutputReadFailed {
                    command: command.to_string(),
                    working_directory: working_dir.to_path_buf(),
                    stream: crate::commands::OutputStream::Stdout,
                    source: std::sync::Arc::new(std::io::Error::other("pipe died mid-read")),
                });
            }

            let (code, stdout, stderr) = self
                .responses
                .get(command)
                .cloned()
                .unwrap_or_else(|| panic!("unexpected command: {command}"));

            Ok(CommandOutput {
                output: std::process::Output {
                    status: ExitStatusExt::from_raw(code << 8),
                    stdout,
                    stderr,
                },
                duration: Duration::ZERO,
            })
        }

        async fn execute_streaming(
            &self,
            _command: &str,
            _timeout: Duration,
            _output_sender: tokio::sync::mpsc::Sender<crate::commands::OutputChunk>,
            _token: &CancellationToken,
        ) -> Result<CommandOutput, CommandError> {
            unimplemented!("not used by resolve")
        }
    }

    fn no_fs() -> MockFileSystem {
        MockFileSystem::default()
    }

    fn fs_with_template(body: &'static str) -> MockFileSystem {
        let mut fs = MockFileSystem::default();
        fs.expect_read_file()
            .returning(move |_| Ok(body.to_string()));
        fs
    }

    fn provider_entry(command: &str, target: &str) -> DotfileEntry {
        serde_saphyr::from_str(&format!("command: {command}\ntarget: {target}")).unwrap()
    }

    fn template_entry(source: &str, vars: &[(&str, &str)], target: &str) -> DotfileEntry {
        let mut yaml = format!("source: {source}\ntarget: {target}\nvars:\n");
        for (name, command) in vars {
            yaml.push_str(&format!("  {name}: {command}\n"));
        }
        serde_saphyr::from_str(&yaml).unwrap()
    }

    async fn resolve(
        entry: &DotfileEntry,
        fs: &MockFileSystem,
        runner: &FakeRunner,
    ) -> Result<ResolvedContent, ResolveError> {
        resolve_content(
            entry,
            Path::new(BASE),
            fs,
            runner,
            TIMEOUT,
            &CancellationToken::new(),
        )
        .await
    }

    #[tokio::test]
    async fn provider_output_becomes_the_content() {
        let runner = FakeRunner::succeeding("op read x", b"secret-token");
        let entry = provider_entry("op read x", "~/.gem/credentials");

        let resolved = resolve(&entry, &no_fs(), &runner).await.unwrap();

        assert_eq!(resolved.bytes, b"secret-token");
        assert!(resolved.warnings.is_empty());
    }

    #[tokio::test]
    async fn commands_run_in_the_package_directory() {
        let runner = FakeRunner::succeeding("op read x", b"t");
        let entry = provider_entry("op read x", "~/.x");

        resolve(&entry, &no_fs(), &runner).await.unwrap();

        assert_eq!(runner.calls(), vec![("op read x".into(), BASE.into())]);
    }

    #[tokio::test]
    async fn zero_length_output_is_an_error() {
        let runner = FakeRunner::succeeding("op read x", b"");
        let entry = provider_entry("op read x", "~/.gem/credentials");

        let err = resolve(&entry, &no_fs(), &runner).await.unwrap_err();

        assert!(err.to_string().contains("produced no output"), "{err}");
    }

    #[tokio::test]
    async fn whitespace_only_output_is_content_not_empty() {
        let runner = FakeRunner::succeeding("op read x", b"\n");
        let entry = provider_entry("op read x", "~/.x");

        let resolved = resolve(&entry, &no_fs(), &runner).await.unwrap();

        assert_eq!(resolved.bytes, b"\n");
    }

    #[tokio::test]
    async fn provider_output_is_byte_exact_including_non_utf8() {
        let runner = FakeRunner::succeeding("op read key", &[0x00, 0xff, 0xfe, 0x0a]);
        let entry = provider_entry("op read key", "~/.ssh/id_ed25519");

        let resolved = resolve(&entry, &no_fs(), &runner).await.unwrap();

        assert_eq!(resolved.bytes, vec![0x00, 0xff, 0xfe, 0x0a]);
    }

    #[tokio::test]
    async fn a_failing_provider_forwards_stderr() {
        let runner = FakeRunner::failing("op read x", b"not logged in");
        let entry = provider_entry("op read x", "~/.x");

        let err = resolve(&entry, &no_fs(), &runner).await.unwrap_err();

        assert!(err.to_string().contains("not logged in"), "{err}");
        assert!(err.to_string().contains("op read x"), "{err}");
    }

    #[tokio::test]
    async fn a_failing_providers_stderr_is_bounded() {
        // Separate from the forwarding test above, which was named
        // `..._forwards_truncated_stderr` while feeding 13 bytes and asserting
        // only `contains`: it named a property none of its assertions tested.
        // Removing the bound from `run_capture` entirely killed no test in the
        // workspace, on the credential path of all places.
        //
        // Counts surviving bytes rather than checking for the marker, since an
        // implementation that appends the marker without cutting passes a suffix
        // check. 'Z' is absent from the rest of the rendered error, so the count
        // measures the forwarded stderr and nothing else.
        use crate::commands::runner::MAX_BOUNDED_BYTES;

        let runner = FakeRunner::failing("op read x", &vec![b'Z'; MAX_BOUNDED_BYTES * 3]);
        let entry = provider_entry("op read x", "~/.x");

        let err = resolve(&entry, &no_fs(), &runner).await.unwrap_err();
        let rendered = err.to_string();

        assert!(
            rendered.contains("bytes elided"),
            "not marked as elided: {rendered}"
        );
        assert_eq!(
            rendered.chars().filter(|c| *c == 'Z').count(),
            MAX_BOUNDED_BYTES,
            "exactly the bound should survive"
        );
    }

    #[tokio::test]
    async fn stderr_is_not_forwarded_when_the_command_succeeds() {
        let mut runner = FakeRunner::default();
        runner.responses.insert(
            "op read x".to_string(),
            (0, b"content".to_vec(), b"verbose: token=SECRET".to_vec()),
        );
        let entry = provider_entry("op read x", "~/.x");

        let resolved = resolve(&entry, &no_fs(), &runner).await.unwrap();

        assert_eq!(resolved.bytes, b"content");
        assert!(
            resolved.warnings.is_empty(),
            "stderr must not surface on the success path: {:?}",
            resolved.warnings
        );
    }

    #[tokio::test]
    async fn template_substitutes_each_binding() {
        let runner = FakeRunner::succeeding("op read a", b"AAA").with("teller get B", b"BBB");
        let entry = template_entry(
            "creds.tpl",
            &[("api_key", "op read a"), ("corp", "teller get B")],
            "~/.gem/credentials",
        );

        let resolved = resolve(
            &entry,
            &fs_with_template("key: {{ api_key }}\ncorp: {{ corp }}\n"),
            &runner,
        )
        .await
        .unwrap();

        assert_eq!(resolved.bytes, b"key: AAA\ncorp: BBB\n");
    }

    #[tokio::test]
    async fn bindings_resolve_in_a_deterministic_order() {
        let runner = FakeRunner::succeeding("cmd-z", b"Z").with("cmd-a", b"A");
        let entry = template_entry("t.tpl", &[("zeta", "cmd-z"), ("alpha", "cmd-a")], "~/.x");

        resolve(&entry, &fs_with_template("{{ alpha }}{{ zeta }}"), &runner)
            .await
            .unwrap();

        let commands: Vec<_> = runner.calls().into_iter().map(|(c, _)| c).collect();
        assert_eq!(
            commands,
            vec!["cmd-a", "cmd-z"],
            "bindings run in key order, so failures report deterministically"
        );
    }

    #[tokio::test]
    async fn value_containing_newline_warns_but_still_substitutes() {
        let runner = FakeRunner::succeeding("op read x", b"tok\nevil: injected");
        let entry = template_entry("t.tpl", &[("api_key", "op read x")], "~/.x");

        let resolved = resolve(&entry, &fs_with_template("key: {{ api_key }}"), &runner)
            .await
            .unwrap();

        assert!(resolved.warnings.iter().any(|w| w.contains("api_key")));
        assert!(resolved.warnings.iter().any(|w| w.contains("line break")));
        assert!(String::from_utf8_lossy(&resolved.bytes).contains("evil: injected"));
    }

    #[tokio::test]
    async fn a_line_break_warning_does_not_contain_the_value() {
        let runner = FakeRunner::succeeding("op read x", b"s3cr3t\nmore");
        let entry = template_entry("t.tpl", &[("api_key", "op read x")], "~/.x");

        let resolved = resolve(&entry, &fs_with_template("key: {{ api_key }}"), &runner)
            .await
            .unwrap();

        for warning in &resolved.warnings {
            assert!(!warning.contains("s3cr3t"), "leaked in: {warning}");
        }
    }

    #[tokio::test]
    async fn failing_binding_names_the_binding_and_stops() {
        let mut runner = FakeRunner::failing("teller get Y", b"not logged in");
        runner
            .responses
            .insert("op read later".to_string(), (0, b"X".to_vec(), Vec::new()));
        let entry = template_entry(
            "t.tpl",
            &[("a", "teller get Y"), ("z_later", "op read later")],
            "~/.x",
        );

        let err = resolve(&entry, &fs_with_template("{{ a }}{{ z_later }}"), &runner)
            .await
            .unwrap_err();

        assert!(err.to_string().contains('a'), "{err}");
        assert!(err.to_string().contains("not logged in"), "{err}");
        assert_eq!(
            runner.calls().len(),
            1,
            "remaining bindings must not run once the render cannot succeed"
        );
    }

    #[tokio::test]
    async fn an_empty_binding_is_an_error() {
        let runner = FakeRunner::succeeding("op read x", b"");
        let entry = template_entry("t.tpl", &[("api_key", "op read x")], "~/.x");

        let err = resolve(&entry, &fs_with_template("key: {{ api_key }}"), &runner)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("api_key"), "{err}");
        assert!(err.to_string().contains("no output"), "{err}");
    }

    #[tokio::test]
    async fn a_non_utf8_binding_survives_into_the_rendered_output() {
        let runner = FakeRunner::succeeding("op read x", &[0xff, 0xfe]);
        let entry = template_entry("t.tpl", &[("v", "op read x")], "~/.x");

        let resolved = resolve(&entry, &fs_with_template("k: {{ v }}"), &runner)
            .await
            .unwrap();

        assert_eq!(resolved.bytes, b"k: \xff\xfe");
    }

    #[tokio::test]
    async fn an_unreadable_template_names_the_template() {
        let runner = FakeRunner::default();
        let mut fs = MockFileSystem::default();
        fs.expect_read_file().returning(|_| {
            Err(crate::fs::filesystem::FileSystemError::IoError(
                std::sync::Arc::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such file",
                )),
            ))
        });
        let entry = template_entry("missing.tpl", &[("a", "op read a")], "~/.x");

        let err = resolve(&entry, &fs, &runner).await.unwrap_err();

        assert!(err.to_string().contains("missing.tpl"), "{err}");
        assert!(
            runner.calls().is_empty(),
            "an unreadable template must not run any binding"
        );
    }

    #[tokio::test]
    async fn oversized_provider_output_is_rejected() {
        let big = vec![b'x'; MAX_CONTENT_BYTES + 1];
        let runner = FakeRunner::succeeding("op read big", &big);
        let entry = provider_entry("op read big", "~/.x");

        let err = resolve(&entry, &no_fs(), &runner).await.unwrap_err();

        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn a_provider_whose_output_could_not_be_read_is_an_error() {
        // Mechanism only. The harm this prevents — a truncated credential
        // reaching a file — is asserted at the apply level, in
        // `tests/dotfile_service_tests.rs`, because that is where a write
        // happens and where this test would not notice one.
        let runner = FakeRunner::read_failing("op read x");
        let entry = provider_entry("op read x", "~/.gem/credentials");

        let err = resolve(&entry, &no_fs(), &runner).await.unwrap_err();

        assert!(matches!(err, ResolveError::CommandFailed { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_binding_whose_output_could_not_be_read_is_an_error() {
        // The second call site of `run_capture`, and the less guarded one: a
        // truncated binding is non-empty, so `EmptyBinding` does not catch it,
        // and truncation shrinks the render, so the size cap does not either.
        let runner = FakeRunner::succeeding("op read a", b"AAA").also_read_failing("teller get B");
        let entry = template_entry(
            "creds.tpl",
            &[("api_key", "op read a"), ("corp", "teller get B")],
            "~/.gem/credentials",
        );

        let err = resolve(
            &entry,
            &fs_with_template("key: {{ api_key }}\ncorp: {{ corp }}\n"),
            &runner,
        )
        .await
        .unwrap_err();

        match err {
            ResolveError::BindingFailed { ref name, .. } => assert_eq!(name, "corp"),
            other => panic!("expected BindingFailed, got: {other}"),
        }
    }

    #[tokio::test]
    async fn a_repo_file_entry_is_refused_rather_than_panicking() {
        let runner = FakeRunner::default();
        let entry = DotfileEntry::new("bat/config", "~/.config/bat/config");

        let err = resolve(&entry, &no_fs(), &runner).await.unwrap_err();

        assert!(err.to_string().contains("no content to resolve"), "{err}");
    }
}
