//! Extended lint: diff-scoped heuristic checks for common low-quality-code
//! patterns that automated compiler lints can't catch structurally.
//!
//! Clippy already denies the machine-checkable half of this class of issue
//! (unwrap/expect, panic, todo!()/unimplemented!(), dead_code, missing_docs,
//! print/dbg macros, and more, depending on the crate's own lint config).
//! What lint tooling structurally cannot check is comment *content* and
//! diff-local *repetition* -- two common low-effort-code tells. This checks
//! only lines added/changed versus the diff base so pre-existing code is
//! never relitigated.
//!
//! Checks (Block = fails; Warn = printed, does not fail):
//!   - Block: leftover TODO/FIXME/XXX/HACK markers in comments
//!   - Block: commented-out code
//!   - Warn: narrating "what the code does" comments
//!   - Warn: the same numeric/string literal repeated 3+ times without a named constant
//!   - Warn: weak/generic identifier names introduced by a new let/fn binding
//!   - Warn: new clippy lint suppressions added
//!
//! Diff base resolution: CLI arg, else `$EXTENDED_LINT_BASE`, else
//! `origin/$GITHUB_BASE_REF` in a GitHub Actions PR, else `origin/main`.
//!
//! Known, deliberate limitation: the `xtask` crate excludes itself from its
//! own scan (see [`run_diff`]), so a genuine leftover TODO added to this
//! crate specifically won't be caught by this tool; everything else in the
//! workspace is scanned normally.

use std::{
    collections::{HashMap, HashSet},
    process::Command,
    sync::LazyLock,
};

use anyhow::{Context, Result};
use regex::Regex;

static TODO_MARKER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)//.*\b(TODO|FIXME|XXX|HACK)\b").unwrap());
static COMMENTED_CODE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^//+\s*(let\s+\w|fn\s+\w|if\s*\(|for\s*\(|match\s+\w|return\b|\w+\s*\([^)]*\)\s*;?\s*$|\w+\.\w+\(.*\)\s*;?\s*$|[\w:<>]+\s*=\s*.+;\s*$)"#,
    )
    .unwrap()
});
static WEAK_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(let(?:\s+mut)?|fn)\s+(temp|tmp|foo|bar|thing|val|obj|stuff)\b").unwrap());
static LIT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?:^|[^\w.])(\d{2,}|"[^"]{4,}")(?:$|[^\w])"#).unwrap());
static CONST_LINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(const|static)\s+\w+").unwrap());
static SUPPRESSION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"#\[(allow|expect)\(clippy::").unwrap());
static TEST_MODULE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(#\[cfg\(test\)\]|mod tests\b)").unwrap());

const NARRATING_OPENERS: &[&str] = &[
    "increment",
    "decrement",
    "loop through",
    "iterate over",
    "iterate through",
    "return the",
    "returns the",
    "create a",
    "creates a",
    "initialize",
    "set the",
    "sets the",
    "get the",
    "gets the",
    "parse the",
    "parses the",
    "convert ",
    "converts ",
    "check if",
    "checks if",
    "validate that",
    "validates that",
    "call ",
    "calls ",
    "define ",
    "defines ",
    "import ",
    "imports ",
    "declare ",
    "declares ",
    "instantiate",
    "loop over",
    "append ",
    "appends ",
    "remove ",
    "removes ",
    "add ",
    "adds ",
];

struct AddedLine {
    file: String,
    lineno: usize,
    content: String,
}

/// Maps a `(file, literal)` pair to the sites (source line text, line number)
/// where that literal was added.
type LiteralSites = HashMap<(String, String), Vec<(String, usize)>>;

fn resolve_diff_base(cli_arg: Option<&str>) -> String {
    if let Some(base) = cli_arg {
        return base.to_string();
    }
    if let Ok(base) = std::env::var("EXTENDED_LINT_BASE") {
        return base;
    }
    if let Ok(base_ref) = std::env::var("GITHUB_BASE_REF") {
        return format!("origin/{base_ref}");
    }
    "origin/main".to_string()
}

fn run_diff(diff_base: &str) -> Result<Vec<AddedLine>> {
    // Exclude this tool's own crate: its doc comments and test fixtures
    // necessarily spell out the literal marker words and comment shapes
    // these checks look for, so scanning them would trip the checks on
    // examples rather than violations. The original `scripts/extended-lint.py`
    // never had this problem since `.py` files never matched the `*.rs` glob.
    let output = Command::new("git")
        .args(["diff", "--unified=0", diff_base, "--", "*.rs", ":(exclude)xtask/**"])
        .output()
        .context("failed to run git diff")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut added = Vec::new();
    let mut current_file = String::new();
    let mut new_lineno: usize = 0;
    let hunk_re = Regex::new(r"^@@ -\d+(?:,\d+)? \+(\d+)").unwrap();

    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = path.to_string();
            continue;
        }
        if let Some(caps) = hunk_re.captures(line) {
            new_lineno = caps[1].parse().unwrap_or(0);
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(content) = line.strip_prefix('+') {
            added.push(AddedLine {
                file: current_file.clone(),
                lineno: new_lineno,
                content: content.to_string(),
            });
            new_lineno += 1;
        } else if !line.starts_with('-') {
            new_lineno += 1;
        }
    }
    Ok(added)
}

fn test_module_start_line(file: &str) -> usize {
    let Ok(text) = std::fs::read_to_string(file) else {
        return usize::MAX;
    };
    for (i, line) in text.lines().enumerate() {
        if TEST_MODULE_RE.is_match(line) {
            return i + 1;
        }
    }
    usize::MAX
}

/// Runs the check; returns `Ok(true)` if clean, `Ok(false)` if blocking
/// findings exist (caller should exit non-zero in that case).
pub(crate) fn run(cli_arg: Option<&str>) -> Result<bool> {
    let diff_base = resolve_diff_base(cli_arg);
    let added = run_diff(&diff_base)?;
    if added.is_empty() {
        println!("[extended-lint] no added Rust lines vs {diff_base}; nothing to check.");
        return Ok(true);
    }

    let mut blocking = Vec::new();
    let mut warnings = Vec::new();
    let mut literal_sites: LiteralSites = HashMap::new();
    let mut const_declared: HashMap<String, HashSet<String>> = HashMap::new();

    for line in &added {
        let stripped = line.content.trim();
        let comment_text = line
            .content
            .find("//")
            .map(|i| line.content[i..].trim().to_string())
            .unwrap_or_default();

        if !comment_text.is_empty() && TODO_MARKER_RE.is_match(&comment_text) {
            blocking.push(format!(
                "{}:{}: leftover TODO/FIXME/XXX/HACK marker: {stripped:?}",
                line.file, line.lineno
            ));
        }

        if !comment_text.is_empty()
            && !comment_text.starts_with("///")
            && !comment_text.starts_with("//!")
            && COMMENTED_CODE_RE.is_match(&comment_text)
        {
            blocking.push(format!(
                "{}:{}: looks like commented-out code: {stripped:?}",
                line.file, line.lineno
            ));
        }

        if comment_text.starts_with("//") && !comment_text.starts_with("///") && !comment_text.starts_with("//!") {
            let body = comment_text.trim_start_matches('/').trim().to_lowercase();
            if NARRATING_OPENERS.iter().any(|opener| body.starts_with(opener)) {
                warnings.push(format!(
                    "{}:{}: narrating 'what' comment, prefer self-explanatory code or a doc comment on why: {stripped:?}",
                    line.file, line.lineno
                ));
            }
        }

        if let Some(caps) = WEAK_NAME_RE.captures(stripped) {
            let weak_name = &caps[2];
            warnings.push(format!(
                "{}:{}: weak/generic identifier name {weak_name:?}: {stripped:?}",
                line.file, line.lineno
            ));
        }

        if SUPPRESSION_RE.is_match(stripped) {
            warnings.push(format!(
                "{}:{}: new clippy suppression added, double-check the reason: {stripped:?}",
                line.file, line.lineno
            ));
        }

        if CONST_LINE_RE.is_match(stripped) {
            for caps in LIT_RE.captures_iter(stripped) {
                const_declared
                    .entry(line.file.clone())
                    .or_default()
                    .insert(caps[1].to_string());
            }
        }

        if line.lineno < test_module_start_line(&line.file) && !stripped.starts_with("#[") {
            for caps in LIT_RE.captures_iter(stripped) {
                literal_sites
                    .entry((line.file.clone(), caps[1].to_string()))
                    .or_default()
                    .push((stripped.to_string(), line.lineno));
            }
        }
    }

    for ((file, literal), sites) in &literal_sites {
        let declared = const_declared.get(file).is_some_and(|s| s.contains(literal));
        if sites.len() >= 3 && !declared {
            let lines: Vec<String> = sites.iter().map(|(_, l)| l.to_string()).collect();
            warnings.push(format!(
                "{file}: literal {literal} repeated {}x at lines {} without a named constant -- consider hoisting it",
                sites.len(),
                lines.join(", ")
            ));
        }
    }

    if !warnings.is_empty() {
        eprintln!("[extended-lint] warnings (review, does not block):");
        for w in &warnings {
            eprintln!("  - {w}");
        }
        eprintln!();
    }

    if !blocking.is_empty() {
        eprintln!("[extended-lint] BLOCKING findings:");
        for b in &blocking {
            eprintln!("  - {b}");
        }
        eprintln!();
        eprintln!("[extended-lint] fix the above, or if a match is a false positive, note why in the PR description.");
        return Ok(false);
    }

    eprintln!("[extended-lint] no blocking findings.");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_todo_marker() {
        assert!(TODO_MARKER_RE.is_match("// TODO: fix this later"));
        assert!(!TODO_MARKER_RE.is_match("// this is fine"));
    }

    #[test]
    fn detects_commented_out_code_but_not_doc_comments() {
        assert!(COMMENTED_CODE_RE.is_match("// let x = compute();"));
        assert!(!COMMENTED_CODE_RE.is_match("/// Returns the computed value."));
    }

    #[test]
    fn detects_weak_names() {
        let caps = WEAK_NAME_RE.captures("let temp = 5;").unwrap();
        assert_eq!(&caps[2], "temp");
        assert!(WEAK_NAME_RE.captures("let value = 5;").is_none());
    }

    #[test]
    fn detects_narrating_comment_openers() {
        assert!(
            NARRATING_OPENERS
                .iter()
                .any(|o| "increment the counter by one".starts_with(o))
        );
        assert!(
            !NARRATING_OPENERS
                .iter()
                .any(|o| "guards against a torn write".starts_with(o))
        );
    }
}
