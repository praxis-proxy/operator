# AGENTS.md

This file provides guidance to coding agents when
working with code in this repository.

## Project

Kubernetes operator for [Praxis], a high-performance
proxy for AI and cloud-native workloads. The operator
manages Praxis proxy instances and configuration as
Kubernetes custom resources, implementing the
Gateway API.

[Praxis]: https://github.com/praxis-proxy/praxis

## Requirements

- Rust stable 1.96+
- Rust nightly (for `rustfmt`)

## Quick Reference

```console
make build          # workspace build
make test           # all tests
make fmt            # format with nightly rustfmt
make lint           # clippy + nightly fmt check
make audit          # cargo audit + cargo deny check
```

Run a single test:

```console
cargo test -p <crate> -- test_name
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
  instead of `Option<T>` with `unwrap_or`.

[`docs/conventions.md`]: docs/conventions.md

## Function Size

30-line threshold enforced by `clippy.toml`. Do not
suppress `too_many_lines` in production code; extract
helpers instead. Suppression is OK in test modules.
