# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code)
when working with code in this repository.

## Project

Kubernetes operator for [Praxis], a high-performance proxy
for AI and cloud-native workloads. The operator manages
Praxis proxy instances and configuration as Kubernetes
custom resources, implementing the Gateway API.

[Praxis]: https://github.com/praxis-proxy/praxis

## Requirements

- Rust stable 1.94+
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

Three-controller design managing Gateway API resources:

```console
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

Full conventions in [`docs/conventions.md`]. Key points:

- `#![deny(unsafe_code)]` in all crate roots
- All items (public and private) require `///` doc
  comments; enforced by `missing_docs` and
  `missing_docs_in_private_items` lints
- Clippy with `-D warnings --all-targets`
- Errors via `thiserror`; logging via `tracing`
- Comments answer "why?", never "what?"; use `tracing`
  for runtime narration
- Prefer `to_owned()` over `to_string()` for `&str`
  to `String`; `String::new()` for empty strings
- Use inline format args: `format!("{var}")`
- Use let-chains, `is_some_and()`, `strip_prefix()`
- Reference-style rustdoc links, not inline
- No re-export-only files
- Controller pattern: `reconcile` + `error_policy`,
  finalizers for cleanup, owner references for GC,
  server-side apply for all mutations
- Use enums for fixed value sets in config, not
  strings; `#[serde(deny_unknown_fields)]` on
  config structs; `#[serde(try_from)]` for
  constrained numerics; `#[serde(default)]`
  instead of `Option<T>` with `unwrap_or`.
  See `docs/conventions.md` "Type Design".

[`docs/conventions.md`]: docs/conventions.md

## File Ordering

1. Constants (with separator comment)
2. Public types, impls, functions
3. Private types and impls
4. Private utility functions (with separator)
5. `#[cfg(test)] mod tests` (always last)

Inside `mod tests`: imports, test functions, then test
utilities (with `// Test Utilities` separator).

Struct fields: `name` first (if present), then
alphabetical. Impl blocks: `new()` first, then `name()`,
then alphabetical. Blank line between each documented
field.

## Function Size

30-line threshold enforced by `clippy.toml`. Do not
suppress `too_many_lines` in production code; extract
helpers instead. Suppression is OK in test modules.
Prefer many small files and functions over fewer
large ones.

## Test Conventions

- Tests must verify precise behavior, not directional
  correctness.
- Never use inline comments in test function bodies.
  Explanatory text goes in assertion messages or
  `tracing::info!`/`debug!`/`trace!` calls.
- Do not add doc comments or regular comments on test
  functions. The function name is the documentation.
- Do not add per-test separator comments. Use one
  full-width separator to mark where tests begin.
- Use "Test Utilities" in separator comments, not
  "Helpers".
- All separator comments must be full-width (77 dashes):

  ```rust
  // -------------------------------------------------------------------------
  // Test Utilities
  // -------------------------------------------------------------------------
  ```

- Test utilities must stay inside `#[cfg(test)]` blocks.

## Separator Comments

Full-width separators (77 dashes) delineate logical
sections:

```rust
// -----------------------------------------------------------------------------
// Section Name
// -----------------------------------------------------------------------------
```

Never use short-form separators like `// --- Section ---`.
