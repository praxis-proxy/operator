# Contributing to Praxis Operator

Thank you for your interest in contributing to
Praxis Operator! We welcome contributions of all
kinds: code, documentation, bug reports, and feature
proposals.

## Prerequisites

- Rust stable 1.96+
- Rust nightly (for `rustfmt`)

## Getting Started

1. Fork the repository and clone your fork
2. Build the project: `make build`
3. Run the tests: `make test`

## Quick Reference

```console
make build          # workspace build
make test           # all tests
make fmt            # format with nightly rustfmt
make lint           # clippy + nightly fmt check
make extended-lint  # diff-scoped heuristic checks (TODOs, comment slop, repetition), via xtask
make audit          # cargo audit + cargo deny check
```

## Developer Certificate of Origin

> **WARNING**: TBD - not currently in effect, we're
> waiting on CNCF sandbox submission.

All commits must be signed off per the
[Developer Certificate of Origin][dco] (DCO). This
certifies that you have the right to submit the
contribution under the project's license.

Sign off by adding `-s` to your commit command:

```console
git commit -s -m "your commit message"
```

This adds a `Signed-off-by` trailer with your name
and email. Commits without sign-off will be rejected
by CI.

## Pull Request Process

1. **Open an issue first** for non-trivial changes.
2. **Create a feature branch** from `main`.
3. **Keep commits focused.** Each commit should be a
   single logical change.
4. **Run lint and tests locally** before submitting:
   `make lint && make test`.
5. **Submit a pull request** with a clear description
   of the change and its motivation.

## Commit Messages

- Subject line: imperative mood, under 50 characters
- Body: wrap at 72 characters, explain _why_ not
  _what_
- Reference issues: `Fixes #123` or `Relates to #456`

## Code Style

Praxis Operator enforces a strict coding style. Read
the full [conventions guide][conventions] before
submitting code. Key points:

- `#![deny(unsafe_code)]` in all crate roots
- Clippy with `-D warnings` (zero tolerance)
- Format with `cargo +nightly fmt`
- Errors via `thiserror`, logging via `tracing`
- Comments answer "why?", never "what?"

## Testing Requirements

New capabilities require:

1. Unit tests covering the implementation
2. Integration tests proving end-to-end behavior

A feature without tests is not complete. See the
[conventions guide][conventions] for details on test
organization and style.

## Code Responsibility

Every contributor is responsible for the code they
submit, regardless of how it was produced. All code
must be human-reviewed before submission or merging.

Pull requests from bots (other than `dependabot`)
will not be accepted. If AI tools assist with
implementation, the submitter must review every line
of the diff and be able to explain every change.

Signed-off commits represent your assertion that you
have reviewed and fully understand the changes you
are submitting.

## Communication

- [GitHub Issues][issues] for bugs and feature
  requests
- [GitHub Discussions][disc] for questions and design

## Code of Conduct

All participants must follow the
[CNCF Code of Conduct][coc].

[conventions]: docs/conventions.md
[dco]: https://developercertificate.org/
[issues]: https://github.com/praxis-proxy/operator/issues
[disc]: https://github.com/orgs/praxis-proxy/discussions
[coc]: CODE_OF_CONDUCT.md
