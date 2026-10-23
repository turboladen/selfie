# 0010. A package selfie could not answer for makes the run Found, by one rule

Date: 2026-10-21

## Status

Accepted. Refines [0006](0006-command-output-contract.md). It does not change what any exit code
means. It decides, once for the three commands §2 names, which packages count as "part of what it
was asked to cover" that "was not covered", the clause in 0006's definition of Found. It refines
[0009](0009-a-status-selfie-cannot-judge-names-why-in-one-type.md), using the reasons it types.
Refined by
[0011](0011-an-install-run-settles-each-package-once-and-installs-recommends-in-order.md), which
decides how often an install run checks and installs a package.

## Context

0006 defines Found as a run that did what it was asked and either found what it was asked to look
for or told the user that part of what it was asked to cover was not covered. Three commands answer
for packages and need that second clause made exact:

- `package list` prints a row for each spec. A row can say the check timed out or could not start,
  that the spec does not parse, or that selfie refuses it.
- `package status` answers for a package and its dependencies. A dependency's row can be Unknown,
  for any reason 0009 types.
- `package install` runs each package's check again after its install command succeeds. The check
  can say the package is installed, say it is not, or give no answer.

Each of the three needs the same answer to the same question: did selfie establish this package's
state? If each command answered it on its own, one broken spec could score Found in one command and
Clean or Failed in another, and a selfie bug could read as a finding in one and a failure in the
next. An install also needs to know what its own verification's answer means for the install.

## Decision

### 1. One question, asked of each package

For every package a command covers, the library decides one of three things:

- **Answered.** selfie established the package's state. It is installed or it is not, or the spec
  decides there is nothing to check: no check command, or no entry for this environment.
- **Unanswered, with its reason.** selfie could not establish the state, and says why:
  - the spec could not be looked up: no spec has the name, it does not parse, two files claim the
    name, or the package directory could not be read;
  - selfie refuses the spec;
  - the check gave no answer: it timed out, it could not start, its working directory was unusable,
    or its result could not be read.
- **An error.** Something went wrong in selfie itself, such as a check task that panicked, or a
  lookup error no lookup is expected to return.

One function in the library makes this decision, from the reasons 0009 types. Commands only total
the results; none of them matches on a check result or a status to decide it again.

### 2. Which packages a command covers

| command           | covers                                                                           |
| ----------------- | -------------------------------------------------------------------------------- |
| `package list`    | every row it prints, including specs it cannot parse                             |
| `package status`  | the package, once it loads and selfie will read it, and each of its dependencies |
| `package install` | the package and each dependency it installed this run                            |

A recommendation is never covered. A recommended package that fails or gives no answer is warned
about, and the run's verdict does not change, as 0006 already says for a failed recommendation.

### 3. Verdicts

- A covered package that is an **error** makes the run **Failed** (1), in every command.
- A covered package that is **unanswered** makes a listing or a query **Found** (3).
- After an install command succeeds, the package's check decides the install:

  | the check …                                           | the install is |
  | ----------------------------------------------------- | -------------- |
  | exits 0                                               | Clean          |
  | exits non-zero (still not installed)                  | Failed (1)     |
  | could not start, or its working directory is unusable | Failed (1)     |
  | timed out, or its result could not be read            | Found (3)      |

  A check that could not start or ran in an unusable directory after an install fails the install,
  because the install's own verification could not run where the install ran. A dependency that
  fails this way stops the run before the package that needs it installs, as a failed install
  command does.
- Failed outranks Found, as 0006 says.

### 4. A Found run says why

When unanswered packages make a run Found, the answer names each one and its reason: the CLI's
summary line on stdout and the MCP result's fields both name them. A Found summary never says
"successfully". A failure sentence for an install whose check still says not installed names what
happened: the install command succeeded and the check says not installed.

## Consequences

- `package list` exits 3 while any listed spec does not parse or is refused. A repository with a
  broken spec keeps `list` at 3 until the spec is fixed or removed.
- A selfie bug never reads as a finding.
- A package named on the command line that selfie cannot look up or will not read fails the run, in
  every command. It is the request itself that failed, so it is never one of the packages a command
  covers.
- `package check` and `package audit` keep their rows in 0006. Each one's request is the check or
  the audit itself, so a command that gives no answer fails what was asked, as a named package that
  cannot be read does.
- `package audit --all` scores "a spec left out" as Failed, which this record does not change. It is
  not a listing, and its row in 0006 stays as it is.
- An install run reports each package under the first graph that settles it, and a later graph
  neither checks nor installs a package that already settled, as
  [0011](0011-an-install-run-settles-each-package-once-and-installs-recommends-in-order.md) decides.
- 0006's table gains rows for `package list`, `package status` and `package install`, which point
  here.
