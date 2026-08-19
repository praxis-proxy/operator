# Agent Guidance

This file provides guidance to coding agents when
working with code in this repository.

AI tools may assist with implementation, but do not
add Claude or another AI tool as a commit
collaborator, co-author, or signatory. Commit
sign-off belongs to the human contributor responsible
for the change.

## Requirements

- Rust stable 1.96+
- Rust nightly (for `rustfmt`)
- Docker 29.3.0+ or Podman (for container builds)

## Quick Reference

```console
make setup-hooks    # install git pre-commit hook
make build          # workspace build
make test           # all tests
make fmt            # format with nightly rustfmt
make lint           # clippy + nightly fmt check
make lint-extra     # typos + taplo + shellcheck
make doc            # rustdoc with -D warnings
make audit          # cargo audit + cargo deny check
make coverage-check # fail if line coverage < 95%
make container      # container image build
```

Run a single test:

```console
cargo test -p praxis-operator -- test_name
```

## Architecture

Three-controller design managing Gateway API
resources:

```text
GatewayClass Controller -> accepts/rejects GatewayClasses
Gateway Controller      -> reconciles Gateways (primary)
HTTPRoute Controller    -> updates route status
```

**Gateway controller reconciliation flow:**

1. Verify GatewayClass ownership
2. Collect attached HTTPRoutes
3. Convert Gateway listeners to Praxis config
4. Convert HTTPRoute rules to Praxis routing config
5. Assemble full Praxis YAML configuration
6. Apply ConfigMap, Deployment, Service via SSA
7. Update Gateway status conditions

**Module structure:**

- `controller/` - reconciliation loops
- `gateway_api/` - attachment, conditions, validation
- `config/` - Praxis YAML generation
- `resources/` - K8s resource builders
- `endpoints.rs` - EndpointSlice resolution
- `stores.rs` - reflector-backed caches
- `leader.rs` - leader election via Lease
- `observability/` - metrics and health endpoints

## Conventions

Full conventions in [`docs/conventions.md`].
Project-specific additions beyond the user-level
Rust Baseline:

- Controller pattern: `reconcile` + `error_policy`,
  finalizers for cleanup, owner references for GC,
  server-side apply for all mutations
- Use enums for fixed value sets in config, not
  strings; `#[serde(deny_unknown_fields)]` on
  config structs; `#[serde(try_from)]` for
  constrained numerics; `#[serde(default)]`
  instead of `Option<T>` with `unwrap_or`
- `#[expect(clippy::..., reason = "...")]` for
  lint suppression (never bare `#[allow]`)
- Descriptive lifetime names (`'route`, `'listen`,
  `'cond`) and closure parameters (no single-char
  identifiers)

[`docs/conventions.md`]: docs/conventions.md

## Test Requirements

New capabilities require:

1. Unit tests covering the logic
2. Integration tests in `tests/integration.rs`
3. Assertion messages on all `assert!` / `assert_eq!`

## Function Size

30-line threshold enforced by `clippy.toml`. Do not
suppress `too_many_lines` in production code; extract
helpers instead. Suppression is OK in test modules.
