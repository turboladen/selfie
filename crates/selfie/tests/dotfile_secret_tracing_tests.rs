//! Leak regression for `tracing`, the egress the event stream does not cover.
//!
//! The event-payload leak tests scan `PackageEvent`s. That catches the library's
//! own log calls only because `EventSender` mirrors each one into an event — it
//! does **not** catch a bare `tracing::debug!` added anywhere in the resolve or
//! deploy path, which would write a secret straight to the subscriber and from
//! there to the CLI's log file.
//!
//! This lives in its own test binary because it installs a process-global
//! subscriber. Keeping it alone in the file means the captured output belongs to
//! this test and nothing else, so a failure is unambiguous.

use selfie::package::SpecOrigin;
use std::io;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use tempfile::TempDir;
use test_common::FakeCommandRunner;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::fmt::MakeWriter;

use selfie::{
    config::SelfieConfigBuilder,
    dotfile_service::{
        port::{ApplyOptions, DotfileService},
        service::DotfileServiceImpl,
    },
    fs::RealFileSystem,
    package::{event::PackageEvent, repository::YamlPackageRepository},
    privilege::{Elevation, Privilege, SudoPolicy},
};

// A privilege port that always reports an ordinary, unelevated process.
struct Unprivileged;

impl Privilege for Unprivileged {
    fn elevation(&self) -> Elevation {
        Elevation::Unprivileged
    }
}

const SECRET: &str = "s3cr3t-v4lue-DO-NOT-LEAK";

// A `MakeWriter` that appends every emitted record to a shared buffer.
#[derive(Clone, Default)]
struct CaptureWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn no_tracing_record_contains_a_resolved_secret() {
    let captured: Arc<Mutex<Vec<u8>>> = Arc::default();
    let writer = CaptureWriter {
        buffer: Arc::clone(&captured),
    };

    // TRACE so that even the most verbose call site is captured. Installed
    // globally because the apply runs on a spawned task, which a thread-local
    // subscriber would not cover.
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .init();

    let temp = TempDir::new().unwrap();
    let package_dir = temp.path().join("packages");
    let target_dir = temp.path().join("target");
    let state_dir = temp.path().join("state");
    for dir in [&package_dir, &target_dir, &state_dir] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let provider_target = target_dir.join("provider.conf");
    let template_target = target_dir.join("template.conf");

    std::fs::create_dir_all(package_dir.join("creds")).unwrap();
    std::fs::write(package_dir.join("creds/t.tpl"), "key: {{ v }}\n").unwrap();
    std::fs::write(
        package_dir.join("creds.yml"),
        format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - command: \"op read x\"\n    target: \"{}\"\n  \
             - source: \"creds/t.tpl\"\n    target: \"{}\"\n    vars:\n      v: \"op read y\"\n",
            provider_target.display(),
            template_target.display()
        ),
    )
    .unwrap();

    // A pre-existing, differing target on one entry so the conflict path runs too.
    std::fs::write(&provider_target, "hand-edited").unwrap();

    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(&package_dir)
        .dotfiles_directory(temp.path().join("dotfiles"))
        .state_directory(state_dir)
        .build();
    let repo = YamlPackageRepository::new(
        RealFileSystem,
        package_dir.clone(),
        SpecOrigin::PackageDirectory,
    );
    let runner = FakeCommandRunner::new()
        .succeeding("op read x", SECRET.as_bytes())
        .succeeding("op read y", SECRET.as_bytes());
    let service = DotfileServiceImpl::new(
        repo,
        RealFileSystem,
        runner,
        config,
        CancellationToken::new(),
        // Not `RealPrivilege`: the answer would depend on how the suite was
        // invoked, and `sudo cargo test` would refuse the apply this test needs.
        SudoPolicy::new(Unprivileged),
    );

    let events: Vec<PackageEvent> = service
        .apply_all(ApplyOptions::default())
        .await
        .collect()
        .await;

    // Positive control: the run genuinely produced and handled the secret. Without
    // this the test would pass if the apply had quietly done nothing.
    assert_eq!(
        std::fs::read_to_string(&template_target).unwrap(),
        format!("key: {SECRET}\n"),
        "the template entry should have been deployed"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            PackageEvent::DotfileDeployed { .. } | PackageEvent::DotfileConflict { .. }
        )),
        "expected the apply to produce a dotfile outcome"
    );

    let log = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
    assert!(
        !log.is_empty(),
        "the subscriber captured nothing, so this test proves nothing"
    );
    test_common::assert_secret_free(&log, SECRET, "a tracing record");
}
