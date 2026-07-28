# 4. Named-value substitution for dotfiles

Date: 2026-07-24

## Status

Accepted

Refines the rejection of templating in [ADR-0001](0001-machine-specific-and-secret-dotfiles.md).

## Context

ADR-0001 rejected a templating language on the grounds that it duplicates what external tools
already provide. That reasoning addressed variation between machines, where per-environment blocks
proved sufficient. It did not weigh the case where a secret is one field inside an otherwise
ordinary configuration file.

Delegating that case to the provider entirely — writing a template in the provider's own reference
syntax and having its inject command render the file — has two defects that only appear once a
second store is involved.

A template written in one vendor's reference syntax works only for users of that vendor. The
structure of a configuration file is not vendor-specific and should not become so merely because one
of its values is secret.

More restrictively, a single file cannot draw values from two different stores. A configuration
needing one credential from a password manager and another from a separate secrets tool cannot be
expressed at all, because the rendering is performed by one vendor's command.

## Decision

A dotfile entry may declare `vars`: a map of names to commands. When present, the entry's repository
file is treated as a template and rendered before deployment; each command is executed and its
output bound to the corresponding name.

Templates are therefore vendor-neutral. They reference names, not stores, and the binding of a name
to a particular tool lives in the package file, where per-environment blocks can already override
it. The same template serves a machine using a password manager and one using a different secrets
tool.

Substitution is by name only. There are no conditionals, loops, includes, or expressions. This is
implemented as direct replacement rather than by embedding a general template engine, so that the
restriction is structural: there is no parser in which control flow could be written. ADR-0001's
objection to a templating language is preserved in substance, since what is added is value
substitution rather than a language.

A placeholder is replaced only when its name is declared in `vars`. Any other placeholder-like text
is left exactly as written, so files that legitimately contain brace syntax pass through unchanged
and no escape mechanism is required. Every declared name must appear in its template; a name that is
never used is an error, which catches misspellings on either side.

Values are substituted verbatim. selfie does not escape them for the target's format, because
inferring that format is unreliable and escaping filters would reintroduce the language this
decision avoids. A value containing characters significant to the target's syntax can therefore
produce a malformed file. This is a documented sharp edge, shared with comparable tools.

It is also more than a correctness concern. Because a value is spliced in without escaping, a value
containing a line break can introduce structure rather than merely corrupt it — in a credentials
file, an additional entry naming a host the user did not configure. Exploiting this requires a
hostile or compromised store, which is a severe compromise in its own right, but the exposure is a
direct consequence of substituting raw.

A value containing a line break therefore produces a warning naming the binding, and is then
substituted as given. Refusing it outright would break legitimate multi-line values such as private
keys and certificates; warning catches both the injection case and the far more common accident of a
store appending a newline, at the cost of one check and without reintroducing escaping.

An entry declaring `vars` is treated as secret-bearing, since selfie cannot determine whether a
bound value is a credential. It receives the handling in
[ADR-0003](0003-no-deploy-state-for-provider-sourced-dotfiles.md): no deploy state, no content in
diffs or events, and owner-only permissions.

## Consequences

- A configuration file's structure stays in the repository, reviewable in a diff, while its secret
  values stay in a store. Only the bindings are vendor-specific.
- A single file may draw values from multiple stores, which delegated rendering cannot express.
- Switching stores on a machine is a change to bindings in the package file, not a rewrite of the
  template.
- Templating cannot be used for non-secret purposes without also losing diffs and deploy state,
  since `vars` implies secret-bearing regardless of what the bound values contain.
- Editing a template changes the rendered output, which differs from the deployed target and is
  reported as a conflict. This follows from holding no deploy state.
- A value whose content collides with the target format's syntax can produce a malformed file, and
  selfie will not detect it.
