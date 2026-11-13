//! `check-stale-pointers`: a `file.rs:N` pointer in prose names a line that
//! exists.
//!
//! In the [prose](super::prose) of the repo, a pointer `path.rs:N` or
//! `path.md:N-M` names a file of the repo whose path ends with `path`
//! (preferring the citing file's own crate when both have one), with at least
//! `M` (or `N`) lines, or the pointer fails. A file under one of [`FOREIGN`]'s
//! paths cites other projects' sources and is not read.
//!
//! Limits: a pointer is checked against a line count, not the line's content,
//! so a pointer that drifts within its file passes. Header and other
//! extensions (`.h:N`, `.cpp:N`) cite SDKs outside the repo and are not read.

use super::lex::line_of;
use super::prose::prose;
use super::{Report, Tree};

/// Directories whose prose cites other projects' files, with the reason.
const FOREIGN: &[(&str, &str)] = &[
    (
        "crates/arc-lend/",
        "cites the sources of the cells it is measured against",
    ),
    (
        "testing/arc-lend-compare/",
        "cites the sources of the cells it compares",
    ),
    (
        "crates/tutti-nodes/tests/filters/oracle_biquad.rs",
        "cites the `biquad` crate it is checked against",
    ),
];

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

/// The crate directory (`crates/x/`) a path is under, or `""`.
fn crate_of(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > 2 && (parts[0] == "crates" || parts[0] == "testing") {
        format!("{}/{}/", parts[0], parts[1])
    } else {
        String::new()
    }
}

pub(super) fn check(tree: &dyn Tree) -> Report {
    let files: Vec<String> = ROOTS.iter().flat_map(|r| tree.files(r, &skipped)).collect();
    let mut bad = Vec::new();
    for p in prose(tree) {
        if FOREIGN.iter().any(|(d, _)| p.path.starts_with(d)) {
            continue;
        }
        let home = crate_of(&p.path);
        for (at, path, last) in pointers(&p.text) {
            let rel = path.trim_start_matches("./");
            let tail = format!("/{rel}");
            let all: Vec<&String> = files
                .iter()
                .filter(|f| *f == rel || f.ends_with(&tail))
                .collect();
            let own: Vec<&String> = all
                .iter()
                .copied()
                .filter(|f| !home.is_empty() && f.starts_with(&home))
                .collect();
            let candidates = if own.is_empty() { all } else { own };
            let line = line_of(&p.text, at);
            if candidates.is_empty() {
                bad.push(format!(
                    "{}:{line}: `{path}:{last}` names a file the repo does not hold",
                    p.path
                ));
                continue;
            }
            let longest = candidates
                .iter()
                .map(|f| tree.read(f).map_or(0, |t| t.lines().count()))
                .max()
                .unwrap_or(0);
            if last > longest {
                bad.push(format!(
                    "{}:{line}: `{path}:{last}` is past the end of {} ({longest} lines)",
                    p.path, candidates[0]
                ));
            }
        }
    }
    let mut r = Report::default();
    if bad.is_empty() {
        r.say("check-stale-pointers: OK");
    } else {
        r.fail("check-stale-pointers: FAIL (a file:line pointer names no such line):");
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
        let mut files = vec![
            ("crates/tutti-a/src/loaded.rs", TEN),
            ("crates/tutti-b/src/loaded.rs", "1\n2\n"),
            ("docs/reference/x.md", "1\n2\n3\n"),
        ];
        files.extend_from_slice(extra);
        check(&Mem::with(&files)).findings
    }

    #[test]
    fn a_pointer_within_its_file_passes() {
        assert!(
            run(&[(
                "crates/tutti-a/src/lib.rs",
                "// `loaded.rs:10`, `src/loaded.rs:3-9`\n"
            )])
            .is_empty()
        );
        assert!(
            run(&[(
                "crates/tutti-c/src/lib.rs",
                "// `tutti-a/src/loaded.rs:8`\n"
            )])
            .is_empty()
        );
        assert!(run(&[("crates/tutti-c/README.md", "see `docs/reference/x.md:3`\n")]).is_empty());
    }

    #[test]
    fn a_pointer_past_the_end_or_to_no_file_fails() {
        let bad = run(&[(
            "crates/tutti-b/src/lib.rs",
            "// `loaded.rs:9`\n// `gone.rs:1`\n// `src/loaded.rs:1-11`\n",
        )]);
        assert_eq!(
            bad[1..],
            [
                "  crates/tutti-b/src/lib.rs:1: `loaded.rs:9` is past the end of crates/tutti-b/src/loaded.rs (2 lines)",
                "  crates/tutti-b/src/lib.rs:2: `gone.rs:1` names a file the repo does not hold",
                "  crates/tutti-b/src/lib.rs:3: `src/loaded.rs:11` is past the end of crates/tutti-b/src/loaded.rs (2 lines)",
            ]
        );
        assert_eq!(
            run(&[("crates/tutti-c/src/lib.rs", "// `src/loaded.rs:11`\n")]).len(),
            2
        );
    }

    #[test]
    fn foreign_sources_headers_and_code_are_not_read() {
        assert!(
            run(&[
                (
                    "crates/arc-lend/src/lib.rs",
                    "// `src/cache.rs:159`, `loaded.rs:99`\n"
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
