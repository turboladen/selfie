//! This module holds the macro that ends a test which cannot discriminate on the
//! machine running it.

/// End the calling test early, because here it cannot tell a pass from a fault.
///
/// On a developer machine this writes `SKIP <test>: <reason>` straight to the
/// process's stderr, past libtest's output capture, and returns from the calling
/// function, so the skip is visible rather than indistinguishable from a pass.
/// With the `CI` environment variable set it panics instead, so a runner that
/// cannot discriminate never reports a green run that asserted nothing.
///
/// # Panics
///
/// When `CI` is set.
#[macro_export]
macro_rules! skip_unless_ci {
    ($test:expr, $reason:expr $(,)?) => {{
        let (test, reason): (&str, &str) = ($test, $reason);
        assert!(
            ::std::env::var_os("CI").is_none(),
            "{test} cannot discriminate on CI: {reason}"
        );
        // `eprintln!` would go to libtest's capture, which a passing test never
        // prints.
        let _ = ::std::io::Write::write_fmt(
            &mut ::std::io::stderr(),
            format_args!("SKIP {test}: {reason}\n"),
        );
        return;
    }};
}
