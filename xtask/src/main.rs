//! Development-time task runner for the `praxis-operator` workspace.
//!
//! Run `cargo run -p xtask -- <SUBCOMMAND> [ARGS]`. See [`lint_extended`]
//! for the `lint-extended` subcommand.

mod lint_extended;

use std::process::ExitCode;

const USAGE: &str = "Usage: cargo run -p xtask -- <SUBCOMMAND> [ARGS]\n\n\
Subcommands:\n  \
lint-extended [DIFF_BASE]  diff-scoped heuristic checks for comment/repetition smells\n";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("lint-extended") => run_lint_extended(args.next().as_deref()),
        Some(other) => {
            eprintln!("xtask: unknown subcommand '{other}'\n\n{USAGE}");
            ExitCode::FAILURE
        },
        None => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        },
    }
}

fn run_lint_extended(diff_base: Option<&str>) -> ExitCode {
    match lint_extended::run(diff_base) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("xtask: lint-extended failed: {err:#}");
            ExitCode::FAILURE
        },
    }
}
