//! `check-comments`: no comment gains a citation of a design doc, an audit, a
//! pull request, a commit, a mutation run or `CLAUDE.md`.
//!
//! A comment states what the code does and the invariant it holds; a
//! citation points elsewhere for it. The gate reads the comments of every
//! `.rs` file under `crates/`, `testing/` and `xtask/`, and the `#` comments
//! of every `Cargo.toml` there, the root's TOML files and the GitHub
//! workflows. In that text it counts, per file, each:
//!
//! - `doc` or `design` (any case) followed by a three-digit number that starts
//!   with `0`, and every `docs/design` path;
//! - `audit` followed by an id of a capital letter and digits;
//! - the phrase that marks a design's as-built record (capital `A`);
//! - `PR #` followed by digits;
//! - a 40-digit hex commit, or a backticked 7- to 12-digit hex token that
//!   holds a letter and a digit, at least four distinct characters, and no
//!   `0b` or `0x` prefix;
//! - a comment line that opens with `Mutation:`;
//! - `CLAUDE.md`.
//!
//! [`BASELINE`] holds the count each file had when the gate began, settled by
//! the shared [ratchet](super::ratchet).

use super::lex::comments;
use super::ratchet::{self, Found, Wording};
use super::{Report, Tree};

/// `<count> <path>` lines: each file's citations when the gate began.
pub(crate) const BASELINE: &str = "xtask/comment-citations.txt";

const ROOTS: [&str; 3] = ["crates", "testing", "xtask"];

/// The root's TOML files read for `#` comments; an absent one is skipped.
const ROOT_TOML: [&str; 4] = [
    "Cargo.toml",
    "deny.toml",
    "rust-toolchain.toml",
    ".cargo/config.toml",
];

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git" | "node_modules")
}

/// The `#` comments of a TOML or YAML text at its offsets: a `#` at a line's
/// start or after whitespace, outside a quoted string, opens a comment that
/// runs to the line's end.
fn hash_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        let b = line.as_bytes();
        let mut quote: Option<u8> = None;
        let mut start = None;
        for (i, &c) in b.iter().enumerate() {
            match quote {
                Some(q) if c == q && (q == b'\'' || i == 0 || b[i - 1] != b'\\') => quote = None,
                Some(_) => {}
                None if c == b'"' || c == b'\'' => quote = Some(c),
                None if c == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()) => {
                    start = Some(i);
                    break;
                }
                None => {}
            }
        }
        for (i, ch) in line.char_indices() {
            match start {
                Some(s) if i >= s => out.push(ch),
                _ if ch == '\n' => out.push('\n'),
                _ => out.extend(std::iter::repeat_n(' ', ch.len_utf8())),
            }
        }
    }
    out
}

fn word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// The citations in one line of comment text, as short labels.
fn citations(line: &str) -> Vec<&'static str> {
    let b = line.as_bytes();
    let lower = line.to_ascii_lowercase();
    let mut found = Vec::new();
    for word in ["doc", "design"] {
        for (at, _) in lower.match_indices(word) {
            if at > 0 && word_byte(b[at - 1]) {
                continue;
            }
            let mut i = at + word.len();
            if i < b.len() && (b[i] == b' ' || b[i] == b'-') {
                i += 1;
            }
            let digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            let ends = b.get(i + digits).is_none_or(|&c| !word_byte(c));
            if digits == 3 && b[i] == b'0' && ends {
                found.push("a design doc");
            }
        }
    }
    found.extend(line.matches("docs/design").map(|_| "a design doc"));
    for (at, _) in line.match_indices("audit ") {
        let id = &b[at + 6..];
        if id.first().is_some_and(u8::is_ascii_uppercase)
            && id.get(1).is_some_and(u8::is_ascii_digit)
        {
            found.push("an audit");
        }
    }
    found.extend(line.matches("As built").map(|_| "an \"As built\" note"));
    for (at, _) in line.match_indices("PR #") {
        if b.get(at + 4).is_some_and(u8::is_ascii_digit) {
            found.push("a pull request");
        }
    }
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_hexdigit() || (i > 0 && word_byte(b[i - 1])) {
            i += 1;
            continue;
        }
        let n = b[i..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
        let token = &b[i..i + n];
        let bounded = b.get(i + n).is_none_or(|&c| !word_byte(c));
        let ticked = i > 0 && b[i - 1] == b'`' && b.get(i + n) == Some(&b'`');
        let mixed =
            token.iter().any(u8::is_ascii_digit) && token.iter().any(u8::is_ascii_alphabetic);
        let lower_hex = token.iter().all(|c| !c.is_ascii_uppercase());
        let mut distinct = token.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        let short = ticked
            && (7..=12).contains(&n)
            && mixed
            && distinct.len() >= 4
            && !token.starts_with(b"0b")
            && !token.starts_with(b"0x");
        if bounded && lower_hex && (n == 40 || short) {
            found.push("a commit");
        }
        i += n;
    }
    let body = line
        .trim_start()
        .trim_start_matches(['/', '!', '#', '*'])
        .trim_start();
    if body.starts_with("Mutation:") {
        found.push("a mutation run");
    }
    found.extend(line.matches("CLAUDE.md").map(|_| "CLAUDE.md"));
    found
}

/// The comment text of `path`, or `None` when the gate does not read it.
fn comment_text(path: &str, raw: String) -> Option<String> {
    if path.ends_with(".rs") {
        Some(comments(&raw))
    } else if path.ends_with("Cargo.toml")
        || ROOT_TOML.contains(&path)
        || (path.starts_with(".github/") && (path.ends_with(".yml") || path.ends_with(".yaml")))
    {
        Some(hash_comments(&raw))
    } else {
        None
    }
}

const WORDING: Wording = Wording {
    gate: "check-comments",
    baseline: BASELINE,
    noun: "citations",
    headline: "a comment gained a citation; state the fact instead",
};

pub(super) fn check(tree: &dyn Tree) -> Report {
    let baseline = match ratchet::load(tree, &WORDING) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let mut paths: Vec<String> = ROOTS
        .iter()
        .chain([".github"].iter())
        .flat_map(|d| tree.files(d, &skipped))
        .collect();
    paths.extend(ROOT_TOML.iter().map(|p| (*p).to_owned()));
    paths.sort();
    paths.dedup();
    let mut found = Vec::new();
    for path in paths {
        let Some(text) = tree.read(&path).and_then(|raw| comment_text(&path, raw)) else {
            continue;
        };
        let mut sites = Vec::new();
        for (n, line) in text.lines().enumerate() {
            for what in citations(line) {
                sites.push(format!("    {path}:{}: cites {what}", n + 1));
            }
        }
        found.push(Found { path, sites });
    }
    ratchet::settle(&WORDING, &baseline, found)
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    fn run(files: &[(&str, &str)]) -> Report {
        check(&Mem::with(files))
    }

    #[test]
    fn each_citation_kind_is_counted_and_spec_sections_are_not() {
        let line = "// doc 013 §2, Design-021, docs/design/x.md, (audit S11), As built, \
                    PR #93, `4b5bd2fd`, CLAUDE.md, M2-104 §7.4, doc 13, doc 0130, 0x1f2e3d4c";
        assert_eq!(
            citations(line),
            [
                "a design doc",
                "a design doc",
                "a design doc",
                "an audit",
                "an \"As built\" note",
                "a pull request",
                "a commit",
                "CLAUDE.md"
            ]
        );
        assert_eq!(
            citations("    // Mutation: drop the check"),
            ["a mutation run"]
        );
        assert!(
            citations("// `deadbeef` word, `1234567` digits, `0ddddddd`, `0b111111`, decoder 013")
                .is_empty()
        );
    }

    #[test]
    fn a_file_over_its_baseline_fails_with_each_site() {
        let r = run(&[
            (BASELINE, "1 crates/a/src/lib.rs\n"),
            (
                "crates/a/src/lib.rs",
                "// doc 013\nfn f() {}\n/// design 015 §5\nconst S: &str = \"doc 014\";\n",
            ),
        ]);
        assert_eq!(
            r.findings,
            [
                "check-comments: FAIL (a comment gained a citation; state the fact instead):",
                "  crates/a/src/lib.rs: 2 citations, baseline 1",
                "    crates/a/src/lib.rs:1: cites a design doc",
                "    crates/a/src/lib.rs:3: cites a design doc",
            ]
        );
    }

    #[test]
    fn a_file_under_its_baseline_passes_and_reports_the_slack() {
        let r = run(&[
            (BASELINE, "3 crates/a/src/lib.rs\n"),
            ("crates/a/src/lib.rs", "// doc 013\n"),
        ]);
        assert!(r.passed());
        assert_eq!(
            r.out,
            [
                "check-comments: OK",
                "check-comments: 1 citations in 1 baseline files (3 total in xtask/comment-citations.txt)",
                "check-comments: xtask/comment-citations.txt can drop by 2; lower it to the current counts",
            ]
        );
    }

    #[test]
    fn manifest_and_workflow_comments_are_read_and_their_strings_are_not() {
        let r = run(&[
            (BASELINE, ""),
            (
                "crates/a/Cargo.toml",
                "# doc 013\nname = \"doc 014 # design 015\" # PR #12\n",
            ),
            (
                ".github/workflows/ci.yml",
                "run: echo 'doc 016' # design 017\n",
            ),
            ("Cargo.toml", "members = [] # doc 018\n"),
            ("crates/a/README.md", "doc 019\n"),
        ]);
        assert_eq!(
            r.findings[1..],
            [
                "  .github/workflows/ci.yml: 1 citations, baseline 0",
                "    .github/workflows/ci.yml:1: cites a design doc",
                "  Cargo.toml: 1 citations, baseline 0",
                "    Cargo.toml:1: cites a design doc",
                "  crates/a/Cargo.toml: 2 citations, baseline 0",
                "    crates/a/Cargo.toml:1: cites a design doc",
                "    crates/a/Cargo.toml:2: cites a pull request",
            ]
        );
    }

    #[test]
    fn a_missing_or_malformed_baseline_fails() {
        assert!(!run(&[]).passed());
        assert!(!run(&[(BASELINE, "many crates/a/src/lib.rs\n")]).passed());
    }
}
