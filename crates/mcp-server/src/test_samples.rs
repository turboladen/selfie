// Sample values shared by the tests of more than one module.

use selfie::package::event::{CheckFailure, RefusalKind, UnknownStatus};

// These build each lookup failure the way production does, by classifying
// the error a lookup returned.
fn lookup(error: selfie::package::port::PackageError) -> UnknownStatus {
    UnknownStatus::LookupFailed(
        selfie::package::port::PackageRepoError::from(error)
            .into_lookup_failure()
            .expect("a lookup returns this error"),
    )
}

pub(crate) fn not_found() -> UnknownStatus {
    lookup(selfie::package::port::PackageError::PackageNotFound {
        name: "d".to_string(),
        packages_path: "/packages".into(),
        files_examined: 0,
        search_patterns: Vec::new(),
    })
}

pub(crate) fn ambiguous() -> UnknownStatus {
    lookup(selfie::package::port::PackageError::MultiplePackagesFound {
        name: "twice".to_string(),
        packages_path: "/packages".into(),
        conflicting_paths: vec!["/packages/twice.yaml".into(), "/packages/twice.yml".into()],
        files_examined: 2,
        search_patterns: Vec::new(),
    })
}

pub(crate) fn unloadable() -> UnknownStatus {
    use selfie::package::port::{PackageParseError, PackageParseKind};

    let source =
        selfie::yaml::parse::<selfie::package::Package>("name: dep\nenvironments: {oops\n")
            .expect_err("the fixture must not parse");
    lookup(selfie::package::port::PackageError::ParseError {
        name: "dep".to_string(),
        packages_path: "/packages".into(),
        failed_file: "/packages/dep.yml".into(),
        cause: PackageParseError::new(
            "/packages/dep.yml",
            PackageParseKind::Yaml { cause: source },
        ),
    })
}

pub(crate) fn directory_unreadable() -> UnknownStatus {
    UnknownStatus::LookupFailed(
        selfie::package::port::PackageRepoError::from(
            selfie::package::port::PackageListError::new(
                "/packages".into(),
                selfie::fs::DirectoryState::Unlistable(std::sync::Arc::new(std::io::Error::from(
                    std::io::ErrorKind::PermissionDenied,
                ))),
            ),
        )
        .into_lookup_failure()
        .expect("a lookup returns this error"),
    )
}

// One of each reason that carries a label: every lookup failure, every way a
// check can give no answer, and each reason labeled by its own name.
pub(crate) fn every_labeled_reason() -> Vec<UnknownStatus> {
    let message = || selfie::commands::BoundedText::bound(b"m");
    let checks = [
        CheckFailure::TimedOut(selfie::commands::runner::TimedOut::new(
            "c",
            std::time::Duration::from_secs(1),
        )),
        CheckFailure::CouldNotStart { message: message() },
        CheckFailure::WorkingDirectoryUnusable(selfie::commands::runner::UnusableDirectory::new(
            "c".to_string(),
            "/nowhere",
            std::sync::Arc::new(std::io::Error::other("gone")),
        )),
        CheckFailure::ResultUnreadable { message: message() },
        CheckFailure::Canceled { message: message() },
        CheckFailure::Internal { message: message() },
    ];
    [
        not_found(),
        ambiguous(),
        unloadable(),
        directory_unreadable(),
        UnknownStatus::Refused {
            kind: RefusalKind::NoEnvironments,
            reason: "r".to_string(),
        },
        UnknownStatus::NotInEnvironment,
        UnknownStatus::NoCheckCommand,
    ]
    .into_iter()
    .chain(checks.into_iter().map(UnknownStatus::CheckFailed))
    .collect()
}
