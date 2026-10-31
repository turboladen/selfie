//! The refused rows a dotfile listing shows, kept with the gap each one leaves.
//!
//! The fields are private to this module, so the only way to add a refused row
//! is [`Unread::refuse`], which adds its gap too. A listing therefore cannot
//! show a refused spec and still exit clean.

use crate::package::{
    event::{Gap, RefusedSpec},
    port::PackageParseError,
};

/// What a listing could not read: the refused rows it shows and the gaps they
/// leave.
#[derive(Default)]
pub(super) struct Unread {
    rows: Vec<RefusedSpec>,
    gaps: Vec<Gap>,
}

impl Unread {
    /// Adds a refused row and the gap it leaves.
    pub(super) fn refuse(&mut self, spec: RefusedSpec) {
        self.gaps.extend(spec.gap());
        self.rows.push(spec);
    }

    /// Adds the gap a spec file that does not parse leaves. The file has its own
    /// event, so no row is added. It is named by the name the file claims, as a
    /// refused row is, so the files of one name leave one gap.
    pub(super) fn unparsable(&mut self, error: &PackageParseError, directory: &std::path::Path) {
        let name = crate::package::spec_name_of(error.package_path())
            .unwrap_or_else(|| error.display_name());
        self.gaps.extend(Gap::unparsable(&name, error, directory));
    }

    /// Returns the rows and one gap per package.
    pub(super) fn finish(self) -> (Vec<RefusedSpec>, Vec<Gap>) {
        (self.rows, Gap::one_per_package(self.gaps))
    }
}
