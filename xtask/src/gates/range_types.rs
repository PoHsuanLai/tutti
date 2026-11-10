//! `check-range-types`: a tuple struct with a private field must not derive
//! `Deserialize` plainly.
//!
//! The derive builds the value without `new`, so a document can carry a raw
//! the type refuses (CLAUDE.md, "Range-checked values"). The one accepted
//! spelling is `serde(try_from = ..)`. `transparent` and `from = ..` build the
//! value without a check too, so they are flagged like a plain derive. A type
//! whose every raw is valid goes on [`EXCEPTIONS`], by name, with that reason.
//!
//! `EXCEPTIONS` is the debt, one entry per type with what removes it. An entry
//! whose type no longer matches is an error, so the list cannot rot.
//!
//! Limits: a textual scan of `struct Name(..)` preceded by its attributes. It
//! sees tuple structs only (not named fields) and a derive spelled in
//! `#[derive]` or `#[cfg_attr(.., derive(..))]`; a hand-written `impl
//! Deserialize` is not checked. Directories named `tests`, `target` and
//! `benches` are skipped.
//!
//! The script was Python over a regex; [`tuple_struct`] is that regex
//! (`re.M`) written as code, with its backtracking order:
//!
//! ```text
//! ^[ \t]*(?:#\[.*?\][ \t]*)*(?:pub(?:\([^)]*\))?[ \t]+)?struct[ \t]+(\w+)
//!   (?:<[^>]*>)?[ \t]*\([ \t\n]*(pub(?![\w(])|\))?
//! ```
//!
//! Mutation record (each planted in this file, seen to fail the named test,
//! removed):
//! - `private` always true: `a_pub_field_and_a_unit_struct_are_not_checked`;
//! - drop `try_from` from the accepted spellings:
//!   `try_from_is_the_one_accepted_spelling`;
//! - `attrs_before` never walks past a `//` line:
//!   `attributes_are_read_above_comments_and_across_lines`;
//! - drop the stale-exception loop: `a_stale_exception_is_an_error`;
//! - skip nothing in `SKIPPED_DIRS`: `tests_target_and_benches_are_skipped`.

use super::{Report, Tree, under_skipped_dir};

/// `(file, type, what removes the entry)`.
pub(super) const EXCEPTIONS: &[(&str, &str, &str)] = &[
    (
        "crates/tutti-midi-protocol/src/sensitivity.rs",
        "PitchBendSensitivity",
        "permanent: every u32 is a valid RPN word",
    ),
    (
        "crates/tutti-plugin-types/src/parameters.rs",
        "ParamId",
        "permanent: every u32 is a valid id",
    ),
    (
        "crates/tutti-types/src/value/latency.rs",
        "Latency",
        "permanent: every Samples is a valid latency, and new is infallible",
    ),
    (
        "crates/tutti-types/src/channels.rs",
        "ChannelLayout",
        "permanent: every u16 is a valid layout, so the derive cannot admit a bad raw",
    ),
];

const SKIPPED_DIRS: &[&str] = &["tests", "target", "benches"];

fn skipped(dir: &str) -> bool {
    SKIPPED_DIRS.contains(&dir)
}

pub(super) fn check(tree: &dyn Tree, exceptions: &[(&str, &str, &str)]) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for top in ["crates", "testing"] {
        for path in tree.files(top, &skipped) {
            if !path.ends_with(".rs") || under_skipped_dir(&path, &skipped) {
                continue;
            }
            let Some(text) = tree.read(&path) else {
                continue;
            };
            let mut at = 0;
            while let Some(m) = next_tuple_struct(&text, at) {
                at = m.end;
                if !m.private {
                    // a `pub` field or unit-like `()`
                    continue;
                }
                let a = format!("{}{}", attrs_before(&text, m.start), &text[m.start..m.end]);
                if !a.contains("Deserialize") || has_word(&a, "try_from") {
                    continue;
                }
                let line = text[..m.start].matches('\n').count() + 1;
                match exceptions
                    .iter()
                    .find(|(p, n, _)| *p == path && *n == m.name)
                {
                    Some((p, n, _)) => seen.push((*p, *n)),
                    None => bad.push(format!(
                        "{path}:{line}: {} derives Deserialize on a private field; use serde(try_from = ..)",
                        m.name
                    )),
                }
            }
        }
    }
    for (p, n, why) in exceptions {
        if !seen.contains(&(*p, *n)) {
            bad.push(format!("stale exception {n} in {p} ({why})"));
        }
    }

    let mut r = Report::default();
    if bad.is_empty() {
        let mut excepted = seen;
        excepted.sort_unstable();
        excepted.dedup();
        r.say(format!(
            "check-range-types: ok ({} excepted)",
            excepted.len()
        ));
    } else {
        r.fail("check-range-types: FAIL");
        for b in bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

/// Python's `\w`.
fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `\bneedle\b`.
fn has_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        !hay[..i].chars().next_back().is_some_and(word)
            && !hay[i + needle.len()..].chars().next().is_some_and(word)
    })
}

/// One match of the tuple-struct regex.
struct Match {
    /// The start of the line the match begins on (`^`).
    start: usize,
    end: usize,
    name: String,
    /// The first field is not a `pub` (and the struct is not unit-like): the
    /// regex's group 2 did not match.
    private: bool,
}

/// `re.finditer` from `from`: the next match, which can begin only at a line
/// start. Matches never overlap.
fn next_tuple_struct(text: &str, from: usize) -> Option<Match> {
    let mut start = from;
    loop {
        // `^` under `re.M`: the start of the text or just after a `\n`.
        if (start == 0 || text.as_bytes()[start - 1] == b'\n')
            && let Some(m) = tuple_struct(text, start)
        {
            return Some(m);
        }
        start += text[start..].find('\n')? + 1;
    }
}

fn skip_blanks(text: &str, mut at: usize) -> usize {
    while matches!(text.as_bytes().get(at), Some(b' ' | b'\t')) {
        at += 1;
    }
    at
}

/// The regex at the line start `start`.
fn tuple_struct(text: &str, start: usize) -> Option<Match> {
    let at = skip_blanks(text, start);
    let (end, name, private) = after_attributes(text, at)?;
    Some(Match {
        start,
        end,
        name,
        private,
    })
}

/// `(?:#\[.*?\][ \t]*)*` then the rest: greedy in the number of attributes,
/// lazy in where each one's `]` is (`.` does not cross a line).
fn after_attributes(text: &str, at: usize) -> Option<(usize, String, bool)> {
    if text[at..].starts_with("#[") {
        let line_end = text[at..].find('\n').map_or(text.len(), |n| at + n);
        for (k, _) in text[at + 2..line_end].match_indices(']') {
            let next = skip_blanks(text, at + 2 + k + 1);
            if let Some(found) = after_attributes(text, next) {
                return Some(found);
            }
        }
    }
    struct_head(text, at)
}

/// `(?:pub(?:\([^)]*\))?[ \t]+)?struct[ \t]+(\w+)(?:<[^>]*>)?[ \t]*\(
/// [ \t\n]*(pub(?![\w(])|\))?`
fn struct_head(text: &str, at: usize) -> Option<(usize, String, bool)> {
    let mut p = at;
    if let Some(rest) = text[p..].strip_prefix("pub") {
        p += 3;
        if let Some(inner) = rest.strip_prefix('(') {
            p += 1 + inner.find(')')? + 1;
        }
        let after = skip_blanks(text, p);
        if after == p {
            return None;
        }
        p = after;
    }
    if !text[p..].starts_with("struct") {
        return None;
    }
    p += "struct".len();
    let after = skip_blanks(text, p);
    if after == p {
        return None;
    }
    p = after;
    let name_len: usize = text[p..]
        .chars()
        .take_while(|&c| word(c))
        .map(char::len_utf8)
        .sum();
    if name_len == 0 {
        return None;
    }
    let name = text[p..p + name_len].to_owned();
    p += name_len;
    if text[p..].starts_with('<') {
        p += text[p..].find('>')? + 1;
    }
    p = skip_blanks(text, p);
    if !text[p..].starts_with('(') {
        return None;
    }
    p += 1;
    while matches!(text.as_bytes().get(p), Some(b' ' | b'\t' | b'\n')) {
        p += 1;
    }
    let rest = &text[p..];
    if let Some(r) = rest.strip_prefix("pub")
        && !r.chars().next().is_some_and(|c| word(c) || c == '(')
    {
        return Some((p + 3, name, false));
    }
    if rest.starts_with(')') {
        return Some((p + 1, name, false));
    }
    Some((p, name, true))
}

/// The attributes directly above `pos`, walking back over `#[..]` (matched
/// brackets, any number of lines) and `//` lines, nearest first.
fn attrs_before(text: &str, pos: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut end = pos;
    loop {
        let head = text[..end].trim_end();
        let last = head.rsplit('\n').next().unwrap_or("").trim();
        if last.starts_with("//") {
            end = head.len() - last.len();
            continue;
        }
        if !head.ends_with(']') {
            return out.join("\n");
        }
        let b = head.as_bytes();
        let mut depth = 0i32;
        let mut i = b.len() as isize - 1;
        while i >= 0 {
            match b[i as usize] {
                b']' => depth += 1,
                b'[' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                break;
            }
            i -= 1;
        }
        if i < 1 || b[i as usize - 1] != b'#' {
            return out.join("\n");
        }
        out.push(&head[i as usize - 1..]);
        end = i as usize - 1;
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const FILE: &str = "crates/tutti-types/src/lib.rs";

    fn run(files: &[(&str, &str)], ex: &[(&str, &str, &str)]) -> Report {
        check(&Mem::with(files), ex)
    }

    fn one(src: &str) -> Report {
        run(&[(FILE, src)], &[])
    }

    fn flagged(name: &str, line: usize) -> String {
        format!(
            "  {FILE}:{line}: {name} derives Deserialize on a private field; use serde(try_from = ..)"
        )
    }

    #[test]
    fn a_tree_without_the_pattern_passes() {
        let r = one("#[derive(Debug)] struct P(u8);\nstruct Q { a: u8 }\n");
        assert_eq!(r.out, ["check-range-types: ok (0 excepted)"]);
        assert!(r.passed());
    }

    #[test]
    fn a_plain_derive_is_flagged_with_its_line() {
        let r = one("\n\n#[derive(serde::Deserialize)] struct P(u8);\n");
        assert_eq!(r.findings, ["check-range-types: FAIL", &flagged("P", 3)]);
        assert_eq!(r.out, r.findings);
    }

    // Mutation: drop `try_from` from the accepted spellings (`has_word`
    // test): the `try_from` lines below are flagged. Accept it as a substring
    // (`contains`): the `x_try_from_x` case is no longer flagged.
    #[test]
    fn try_from_is_the_one_accepted_spelling() {
        assert!(
            one("#[derive(serde::Deserialize)]\n#[serde(try_from = \"u8\")]\nstruct P(u8);\n")
                .passed()
        );
        assert!(one("#[cfg_attr(feature = \"serde\", derive(serde::Deserialize))]\n#[cfg_attr(feature = \"serde\", serde(try_from = \"u8\"))]\npub struct P(pub(crate) u8);\n").passed());
        assert!(
            !one("#[derive(serde::Deserialize)]\n#[serde(x_try_from_x = \"u8\")]\nstruct P(u8);\n")
                .passed()
        );
        for flag in ["transparent", "from = \"u8\""] {
            let r = one(&format!(
                "#[derive(serde::Deserialize)] #[serde({flag})] struct P(u8);\n"
            ));
            assert_eq!(r.findings[1], flagged("P", 1), "{flag}");
        }
    }

    // Mutation: make `private` always true: the `pub` and `()` cases are
    // flagged. Treat `pub(crate)` as public: the second case is not flagged.
    #[test]
    fn a_pub_field_and_a_unit_struct_are_not_checked() {
        assert!(one("#[derive(serde::Deserialize)] struct P(pub u8);\n").passed());
        assert!(one("#[derive(serde::Deserialize)] struct P();\n").passed());
        assert!(one("#[derive(serde::Deserialize)] struct P(\n    pub u8);\n").passed());
        assert!(!one("#[derive(serde::Deserialize)] pub struct P(pub(crate) u8);\n").passed());
        assert!(!one("#[derive(serde::Deserialize)] struct P(pub_name: u8);\n").passed());
        assert!(!one("#[derive(serde::Deserialize)] struct P(u8, pub u8);\n").passed());
    }

    // Mutation: `attrs_before` returns at the first `//` line instead of
    // walking past it: the first case is no longer flagged; stop at a
    // single-line attribute: the multi-line case passes.
    #[test]
    fn attributes_are_read_above_comments_and_across_lines() {
        let r = one(
            "#[derive(serde::Deserialize)]\n// why\n/// docs\n#[allow(dead_code)]\nstruct P(u8);\n",
        );
        assert_eq!(r.findings[1], flagged("P", 5));
        let r = one(
            "#[cfg_attr(\n    feature = \"serde\",\n    derive(serde::Deserialize)\n)]\npub struct P(u8);\n",
        );
        assert_eq!(r.findings[1], flagged("P", 5));
        // A derive above an unrelated item is not this struct's.
        assert!(one("#[derive(serde::Deserialize)]\nstruct A { x: u8 }\nstruct P(u8);\n").passed());
        // Code above stops the walk.
        assert!(one("#[derive(serde::Deserialize)]\nfn f() {}\nstruct P(u8);\n").passed());
    }

    #[test]
    fn visibility_generics_and_blank_fields_are_read() {
        for src in [
            "#[derive(serde::Deserialize)] pub struct P<T>(T);\n",
            "#[derive(serde::Deserialize)] pub(crate) struct P<T: A>(T);\n",
            "#[derive(serde::Deserialize)] pub(in crate::x) struct P (\n\t u8);\n",
            "    #[derive(serde::Deserialize)]\n    struct P(u8);\n",
        ] {
            assert_eq!(one(src).findings.len(), 2, "{src}");
        }
        for src in [
            "#[derive(serde::Deserialize)] structP(u8);\n",
            "#[derive(serde::Deserialize)] struct P\n(u8);\n",
            "#[derive(serde::Deserialize)] pubstruct P(u8);\n",
        ] {
            assert!(one(src).passed(), "{src}");
        }
    }

    // Mutation: scan only `crates`: the second case fails.
    #[test]
    fn crates_and_testing_are_scanned_and_only_rs_files() {
        let bad = "#[derive(serde::Deserialize)] struct P(u8);\n";
        let r = run(
            &[("testing/t/src/lib.rs", bad), ("crates/c/README.md", bad)],
            &[],
        );
        assert_eq!(
            r.findings,
            [
                "check-range-types: FAIL".to_owned(),
                "  testing/t/src/lib.rs:1: P derives Deserialize on a private field; use serde(try_from = ..)".to_owned()
            ]
        );
    }

    // Mutation: empty `SKIPPED_DIRS`: this fails.
    #[test]
    fn tests_target_and_benches_are_skipped() {
        let bad = "#[derive(serde::Deserialize)] struct P(u8);\n";
        let r = run(
            &[
                ("crates/c/tests/t.rs", bad),
                ("crates/c/src/tests/t.rs", bad),
                ("crates/c/target/t.rs", bad),
                ("crates/c/benches/b.rs", bad),
            ],
            &[],
        );
        assert!(r.passed(), "{:?}", r.findings);
    }

    // Mutation: drop the `seen.push`: every entry below is reported stale.
    // Compare the path as well as the name: the other-file case passes
    // wrongly.
    #[test]
    fn every_exception_is_honoured_by_path_and_name() {
        for (path, name, _) in EXCEPTIONS {
            let src = format!("#[derive(serde::Deserialize)]\npub struct {name}(u32);\n");
            let only = [(*path, *name, "why")];
            let r = run(&[(*path, &src)], &only);
            assert_eq!(r.out, ["check-range-types: ok (1 excepted)"], "{name}");
            let r = run(&[("crates/other/src/lib.rs", &src)], &only);
            assert_eq!(
                r.findings,
                [
                    "check-range-types: FAIL".to_owned(),
                    format!(
                        "  crates/other/src/lib.rs:2: {name} derives Deserialize on a private field; use serde(try_from = ..)"
                    ),
                    format!("  stale exception {name} in {path} (why)"),
                ]
            );
        }
    }

    #[test]
    fn the_exception_list_is_the_four_it_was() {
        let names: Vec<_> = EXCEPTIONS.iter().map(|e| e.1).collect();
        assert_eq!(
            names,
            [
                "PitchBendSensitivity",
                "ParamId",
                "Latency",
                "ChannelLayout"
            ]
        );
        assert!(EXCEPTIONS.iter().all(|e| e.2.starts_with("permanent: ")));
    }

    #[test]
    fn a_stale_exception_is_an_error() {
        let r = run(
            &[(FILE, "struct A;\n")],
            &[("crates/x.rs", "Gone", "temporary: lane 7")],
        );
        assert_eq!(
            r.findings,
            [
                "check-range-types: FAIL",
                "  stale exception Gone in crates/x.rs (temporary: lane 7)"
            ]
        );
    }

    #[test]
    fn the_same_type_excepted_twice_counts_once() {
        let src = "#[derive(serde::Deserialize)] struct P(u8);\n#[derive(serde::Deserialize)] struct P(u8);\n";
        let r = run(&[(FILE, src)], &[(FILE, "P", "why")]);
        assert_eq!(r.out, ["check-range-types: ok (1 excepted)"]);
    }
}
