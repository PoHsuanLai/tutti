//! `check-pointers`: prose about the code states the code, never a line of it.
//!
//! In the [prose](super::prose) of the repo outside `docs/`, a pointer
//! `path.rs:N` or `path.md:N` fails: a comment states the logic or the
//! invariant itself, and a line number drifts with every edit above it. A
//! file under one of [`FOREIGN`]'s paths cites other projects' sources and is
//! not read. Under `docs/`, a pointer names a repo file whose path ends with
//! `path` and that has at least `N` lines (the last line of a `N-M` range).
//!
//! Limits: under `docs/`, a pointer is checked against a line count, not the
//! line's content. Header and other extensions (`.h:N`, `.cpp:N`) cite SDKs
//! outside the repo and are not read.

use super::lex::line_of;
use super::prose::prose;
use super::{Report, Tree};

/// Directories whose prose cites other projects' files, with the reason.
const FOREIGN: &[(&str, &str)] = &[(
    "crates/tutti-nodes/tests/filters/oracle_biquad.rs",
    "cites the `biquad` crate it is checked against",
)];

const ROOTS: [&str; 5] = ["crates", "testing", "xtask", "docs", "vendor"];

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git")
}

fn path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/')
}

/// Every `path.(rs|md):N[-M]` in `text`: (offset, path, last line).
fn pointers(text: &str) -> Vec<(usize, &str, usize)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    for ext in [".rs:", ".md:"] {
        for (at, _) in text.match_indices(ext) {
            let mut s = at;
            while s > 0 && path_byte(b[s - 1]) {
                s -= 1;
            }
            let path = &text[s..at + ext.len() - 1];
            let stem = &path[..path.len() - 3];
            if stem.is_empty() || stem.ends_with('/') || stem.ends_with('.') {
                continue;
            }
            let digits = |from: usize| {
                let n = text[from..].bytes().take_while(u8::is_ascii_digit).count();
                (n > 0).then(|| (text[from..from + n].parse::<usize>().ok(), from + n))
            };
            let Some((Some(first), e)) = digits(at + ext.len()) else {
                continue;
            };
            let last = match text[e..].strip_prefix('-') {
                Some(_) => digits(e + 1).and_then(|(n, _)| n).unwrap_or(first),
                None => first,
            };
            out.push((s, path, last.max(first)));
        }
    }
    out.sort();
    out
}

pub(super) fn check(tree: &dyn Tree) -> Report {
    let files: Vec<String> = ROOTS.iter().flat_map(|r| tree.files(r, &skipped)).collect();
    let mut bad = Vec::new();
    for p in prose(tree) {
        if FOREIGN.iter().any(|(d, _)| p.path.starts_with(d)) {
            continue;
        }
        let docs = p.path.starts_with("docs/");
        for (at, path, last) in pointers(&p.text) {
            let line = line_of(&p.text, at);
            if !docs {
                bad.push(format!(
                    "{}:{line}: `{path}:{last}` points at a line; state what it holds instead",
                    p.path
                ));
                continue;
            }
            let rel = path.trim_start_matches("./");
            let tail = format!("/{rel}");
            let candidates: Vec<&String> = files
                .iter()
                .filter(|f| *f == rel || f.ends_with(&tail))
                .collect();
            let longest = candidates
                .iter()
                .map(|f| tree.read(f).map_or(0, |t| t.lines().count()))
                .max();
            match longest {
                None => bad.push(format!(
                    "{}:{line}: `{path}:{last}` names a file the repo does not hold",
                    p.path
                )),
                Some(n) if last > n => bad.push(format!(
                    "{}:{line}: `{path}:{last}` is past the end of {} ({n} lines)",
                    p.path, candidates[0]
                )),
                Some(_) => {}
            }
        }
    }
    let mut r = Report::default();
    if bad.is_empty() {
        r.say("check-pointers: OK");
    } else {
        r.fail("check-pointers: FAIL (a file:line pointer in prose):");
        for line in bad {
            r.fail(format!("  {line}"));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const TEN: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";

    fn run(extra: &[(&str, &str)]) -> Vec<String> {
        let mut files = vec![("crates/tutti-a/src/loaded.rs", TEN)];
        files.extend_from_slice(extra);
        check(&Mem::with(&files)).findings
    }

    #[test]
    fn a_pointer_in_a_code_comment_or_a_readme_fails_even_when_it_resolves() {
        assert_eq!(
            run(&[("crates/tutti-a/src/lib.rs", "// see `loaded.rs:3`\n")]),
            [
                "check-pointers: FAIL (a file:line pointer in prose):",
                "  crates/tutti-a/src/lib.rs:1: `loaded.rs:3` points at a line; state what it holds instead",
            ]
        );
        assert_eq!(
            run(&[("crates/tutti-a/README.md", "`src/loaded.rs:2-4`\n")]).len(),
            2
        );
        assert_eq!(run(&[("xtask/src/x.rs", "/// `a/b.md:1`\n")]).len(), 2);
    }

    #[test]
    fn a_pointer_under_docs_names_a_line_that_exists() {
        assert!(
            run(&[(
                "docs/reference/x.md",
                "`tutti-a/src/loaded.rs:3-10`, `loaded.rs:10`\n"
            )])
            .is_empty()
        );
        assert_eq!(
            run(&[("docs/reference/x.md", "`loaded.rs:11`\n`gone.rs:1`\n")])[1..],
            [
                "  docs/reference/x.md:1: `loaded.rs:11` is past the end of crates/tutti-a/src/loaded.rs (10 lines)",
                "  docs/reference/x.md:2: `gone.rs:1` names a file the repo does not hold",
            ]
        );
    }

    #[test]
    fn foreign_sources_headers_and_code_are_not_read() {
        assert!(
            run(&[
                (
                    "crates/tutti-nodes/tests/filters/oracle_biquad.rs",
                    "// `coefficients.rs:302`\n"
                ),
                (
                    "crates/tutti-a/src/x.rs",
                    "// `AUComponent.h:953`\nlet s = \"gone.rs:1\";\n"
                ),
            ])
            .is_empty()
        );
    }
}
