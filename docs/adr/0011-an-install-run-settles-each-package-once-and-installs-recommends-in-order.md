# 0011. An install run settles each package once and installs recommends in order

Date: 2026-10-22

## Status

Accepted. Refines [0006](0006-command-output-contract.md) §5, adding one line that prints at default
verbosity, and [0010](0010-a-package-selfie-could-not-answer-for-makes-the-run-found.md), deciding
how often an install run checks and installs a package and under which graph it is reported.

## Context

`selfie package install R` resolves R's dependency graph and installs it, then resolves and installs
each of R's recommends through a graph of its own. The graphs overlap: R and recommend A can both
depend on D, and recommends A and B can both depend on E. Unless the graphs share what they settle,
each one that reaches a shared package checks it, installs it if the check says it is missing, and
reports it, so one package is checked and reported several times in one run. Run concurrently, A's
and B's graphs can start E's install at the same moment. Package managers such as apt and Homebrew
take a lock, so one of the two installs waits or fails, and B can be reported failed for a lock it
lost.

The rules any answer has to keep:

- Each package's outcome is reported once per run, and a check never predates an install earlier in
  the run that could change its answer.
- A recommend is reported failed only for a command it ran itself. One a cancel stopped before it
  ran anything is named untried.
- Which graph a shared package is reported under, and the warnings and their order, follow from the
  spec and the command line, never from scheduling.
- Each graph's progress ends at its total, and the total counts only the steps that graph takes.
- A recommend's failure, or a gap in its graph, is a warning and never changes the run's verdict.

Options considered and rejected:

- **One plan for the whole run.** Resolve R's graph and every recommend's graph first, merge them,
  and install the merged graph through a scheduler that starts a package once its dependencies
  settle. It meets the rules, but needs the scheduler, a merged graph that keeps R's packages fatal
  and the recommends' packages non-fatal, and cancellation rules for dependents still waiting. A
  missing spec in a recommend would also surface before R installs. It is about three times the size
  of the decision below, and most of that size is concurrency.
- **A run-wide ledger shared by concurrent graphs.** One cell per package, filled by whichever graph
  reaches it first and awaited by the rest. A cached "not installed" goes stale once a later install
  provides what that check looks for, a graph waiting on a canceled cell is reported failed for a
  command it never ran, and which recommend a shared gap is reported under depends on which graph
  won the race. Each repair adds a special case.

Concurrency across recommends buys little. A root has a handful of recommends, and most install time
is spent inside a package manager that serializes on its own lock.

## Decision

1. **Recommends install one at a time, in the order the package lists them.** `max_concurrency` does
   not apply to them.
2. **One record per install run holds every package that settled**: installed, found already
   installed, or installed with no answer from its check (a gap, ADR 0010 §3). Before a later graph
   refuses anything, runs any check or sets its progress total, it drops the packages in the record.
   It neither checks nor installs them again.
3. **A failure is never recorded.** The next graph that needs a package that failed runs its
   commands again itself and reports its own failure, so every failure message is about a command
   that graph ran.
4. **A shared package is reported under the first graph that settles it, in run order**: R's graph,
   then each recommend in R's declared order. A gap is reported once, under that graph: in R's
   result if R's graph found it, otherwise as that recommend's warning.
5. **A later graph that reuses a settled package prints one line naming it** and saying how it
   settled, such as "dep: installed earlier in this run". The line is not a warning, and it changes
   no verdict, gap or warning. It is the one line about a run, besides a step that waits and the
   result, that prints at default verbosity, so a run that skipped work says why.
6. **Exit codes do not change.** ADR 0010 covers R and the dependencies R's graph installs, which
   all settle before any recommend runs, so the record never moves a package into or out of the
   covered set.

## Consequences

- A run with several slow recommends takes longer, since they no longer overlap. If a real
  configuration makes that slow, the remedy is a scheduler over one merged plan, the first option
  above.
- An install that fails can run twice in one run, once for each graph that needs it.
- Two installs' streamed output never interleave, and output follows the declared order.
- Reusing a package never gives a stale answer: only a success is reused, and an install does not
  uninstall anything.
- A run that reuses a package prints one more line at default verbosity than 0006 §5 otherwise
  allows.
