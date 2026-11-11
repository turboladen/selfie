//! The name a dotfile's copy takes in the repository that holds it.
//!
//! A copy sits in a directory git and other tools walk. A few file names are
//! instructions to those tools about the files beside them, so a copy under one
//! of those names stops being data: a `.gitignore` copy makes `git add` skip the
//! very sources it names, with no message.

/// File names that git or a search tool reads, in the directory holding them,
/// as instructions about the files around them.
// `.gitmodules` is not listed: git reads it only at the repository's root,
// where no copy is placed.
const READ_AS_INSTRUCTIONS: [&str; 6] = [
    ".git",
    ".gitignore",
    ".gitattributes",
    ".ignore",
    ".rgignore",
    ".fdignore",
];

/// Whether a file named `name` would be read as instructions to git or a search
/// tool, rather than kept as data, if a repository held it.
///
/// The comparison ignores ASCII case, because on a case-insensitive file system
/// git opens `.GitIgnore` as `.gitignore`.
#[must_use]
pub(crate) fn is_read_as_instructions(name: &str) -> bool {
    READ_AS_INSTRUCTIONS
        .iter()
        .any(|listed| listed.eq_ignore_ascii_case(name))
}

/// The name to store a copy of the file `name` under.
///
/// The name itself, unless a tool would read it as instructions; then the name
/// without its leading dot, so `.gitignore` is stored as `gitignore`. The
/// deployed target keeps its own name either way.
#[must_use]
pub(crate) fn copy_name(name: &str) -> &str {
    if is_read_as_instructions(name) {
        name.strip_prefix('.').unwrap_or(name)
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each listed name loses its dot, in any case, and only those: a name that
    // merely begins with a dot, or contains a listed name, keeps its own.
    #[test]
    fn only_the_listed_names_lose_their_dot() {
        assert_eq!(copy_name(".gitignore"), "gitignore");
        assert_eq!(copy_name(".GitIgnore"), "GitIgnore");
        assert_eq!(copy_name(".gitattributes"), "gitattributes");
        assert_eq!(copy_name(".git"), "git");
        assert_eq!(copy_name(".ignore"), "ignore");
        assert_eq!(copy_name(".rgignore"), "rgignore");
        assert_eq!(copy_name(".fdignore"), "fdignore");
        assert_eq!(copy_name(".gitmodules"), ".gitmodules");
        assert_eq!(copy_name(".bashrc"), ".bashrc");
        assert_eq!(copy_name(".gitignore_global"), ".gitignore_global");
        assert_eq!(copy_name("gitignore"), "gitignore");
    }
}
