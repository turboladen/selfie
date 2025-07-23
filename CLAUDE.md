## Your Role

You are an expert in Rust software development across multiple operating systems, system
administration, configuration management, and command-line interfaces. Your job is to help implement
a CLI tool with separate backing library, called "selfie-cli" and "selfie", respectively, written in
Rust, that can help me (and other users) manage packages in environments across multiple machines
and operating systems.

## Guidelines

When generating code, use Rust's `stdlib` when possible, `tokio` when async makes sense, and common
third-party libraries. Use the `console` and `dialoguer` crates for working with
stdout/stderr/the console. Use the `tracing` crate for logging. Use `clap` for CLI and argument
parsing. Use `anyhow` and `thiserror` for error handling. Use `assert_cmd` and `mockall` for unit
testing; use `testcontainers` for integration testing. Always use the latest versions of Rust and
libraries.

Don’t implement any backward compatibility when changing existing code. Reuse existing code when
possible. Keep the codebase DRY and lean toward following the KISS principle. Lean towards using
third party libraries for substantial features and functionality, so we can keep the codebase small.

When you write tests for cli commands, lean on the Hexagonal ability to mock out interfaces. We
shouldn't be running commands in tests that alter our development environment.

## Project Organization

There are multiple crates in the repo, all under the `crates/` subdirectory:

1. `selfie-cli` (in `cli/`) which is the main UI (a CLI) for selfie,
2. `selfie` which is the library containing the core logic for selfie,
3. `test-common` which are helper types and functions to use in tests (since setting up for testing
   often requires the same type of set up).

Eventually I may want to create a second UI, so I want to keep logic in the `selfie` library, but
allow consumer crates to be able to handle formatting messages to the user; in general, `selfie`
shouldn't write to stdout/stderr because it doesn't know if it will be called from a GUI, a TUI, a
CLI app or even from some other language.

## Design Patterns

Follow the Hexagonal Architecture design (aka Ports and Adapters), particularly for the core library
(`selfie`); the CLI crate will follow this too, but may also apply other patterns (like Command) as
needed. Hexagonal design usually means using generics and monomorphism in the library (`selfie`),
and dynamic dispatch/trait objects in the calling crates (`selfie-cli`).

Messaging about work that `selfie` does should be communicated via "events" so that the caller can
decide how to display information about that event to the user in the current UI context.

## `selfie` Concepts

This section describes the eventual functionality of the `selfie` library and its primary consumer
for now, the CLI app, `selfie-cli`. We're slowing working toward implementing this, step by step.

At a high level, selfie lets users install a package using the method/package manager of their
choosing. For example, if I know I want to leverage `npm` as much as possible for installing
packages that depend on Node, I'll define my selfie packages to use `npm`. This helps me not have to
remember how I deal with each of these packages; I've encoded it in selfie, and can simply ask
selfie to do what I want.

Since a user may want different behavior for different environments they work in (ex. macOS vs
Debian), selfie has the concept of "environments". The user can specify in their package file how
they want selfie to install their package when working in each environment they've defined. To make
this work, the user should set their current environment selfie setting so that selfie knows which
environment to install for when asked to install a package.

Just to clarify: selfie doesn't actually install any packages: it just runs whatever command you set
up in a given package file; if that command happens to tell another package manager to install a
package by a name that's related to your package file's name, then that will, of course, most likely
result in a package being installed. Obviously, this is the intended purpose for selfie, but it's
really just a glorified command runner.

### Packages

#### Package Definition

`selfie` and all it's UIs are meant to serve as a sort of personal meta-package manager, primarily
for users that use tools and libraries in their environment that are fetched via multiple package
managers. It allows the user to be very controlling over which package manager they use for
installing packages and even groups of similar packages. For example, I can get
`bash-language-server` from Homebrew on my Mac or via `npm`, but maybe I don't want to have to
install `node` and all that just to get the package I want, so I'll want to configure a `selfie`
package for `bash-language-server` to install it via Homebrew. ...but if I'm on Ubuntu, I may want
to use `node` because `apt` only has a really old version of the package, so I'll update my `selfie`
package with that. Then regardless of which environment I'm in, I can
`selfie install bash-language-server` and it'll choose the installation method that I've set up for
that environment.

Package files are defined in YAML and are represented in Rust by the `selfie::package::Package`
type.

#### Package Validation

`selfie` also provides means for validating `selfie` package files, letting users know if the
package file they've created follows the specification.

#### Package Check

`selfie` also provides means for checking if a package is installed per the package definition file.
The package definition file lets users define _how_ they want this command to check if the package
is installed. `selfie` does not need to know how every package manager on the planet operates, as
that's too complicated and too much to maintain.

#### Package Listing

`selfie` provides means for listing all packages that it knows about. Essentially, these are just
YAML files from the configured package directory.

#### Package Create, Edit, Info, Remove

`selfie` provides means for programmatically creating, editing, and removing a package file (in the
configured `package_directory`). This is just a quality-of-life feature that saves the user from
having to remember which directory is their `package_directory`, then creating/editing and opening a
file there; same for removing.

`selfie` provides means for getting information about a package. This info currently only contains
info from the request package's package file, as well as if it's already on the system, but may
eventually report other info.

### Environments

An "environment" in `selfie` can be any string that the user chooses to identify whatever context
they want. Typically, a user would specify an environment per operating system/distribution, but
they can call the environment whatever they want--this is merely a means to tie package
installation/check commands to some label.

Package definition files have `environment` sections in them to let users capture installation and
check methods for those labels.

### Configuration

`selfie` is configurable via command line flags (for `selfie-cli`) or via a config file in
`~/.config/selfie/config.yml`. Main, required config values are:

- `environment`: The value for the current environment selfie should use.
- `package_directory`: The directory in which to expect to find selfie package files.
