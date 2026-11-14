//! `check-range-types`: a struct whose fields are all private must not derive
//! `Deserialize` plainly.
//!
//! The derive builds the value without `new`, so a document can carry a raw
//! the type refuses (CLAUDE.md, "Range-checked values"). The one accepted
//! spelling is `serde(try_from = ..)`. `transparent` and `from = ..` build the
//! value without a check too, so they are flagged like a plain derive. A type
//! whose every combination of field values is valid goes on [`EXCEPTIONS`], by
//! file and name, with that reason.
//!
//! `EXCEPTIONS` is the debt, one entry per type with what removes it. An entry
//! whose type no longer matches (the file or struct is gone, or it no longer
//! derives `Deserialize` plainly) is an error, so the list cannot rot.
//!
//! Two passes read the same attributes: tuple structs (`struct Name(..)`, whose
//! first field is not a bare `pub`) and named-field structs (`struct Name {
//! .. }` with at least one field and none a bare `pub`). A struct with a bare
//! `pub` field is skipped: its fields are public, so the derive admits nothing
//! a caller could not build. `pub(crate)` and `pub(super)` count as private.
//!
//! Limits: a textual scan of `struct Name` preceded by its attributes, over a
//! derive spelled in `#[derive]` or `#[cfg_attr(.., derive(..))]`; a
//! hand-written `impl Deserialize` is not checked. Directories named `tests`,
//! `target` and `benches` are skipped.
//!
//! The tuple pass was a Python regex; [`struct_head`] is that regex
//! (`re.M`) written as code, with its backtracking order:
//!
//! ```text
//! ^[ \t]*(?:#\[.*?\][ \t]*)*(?:pub(?:\([^)]*\))?[ \t]+)?struct[ \t]+(\w+)
//!   (?:<[^>]*>)?[ \t]*\([ \t\n]*(pub(?![\w(])|\))?
//! ```

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
    (
        "crates/tutti-ump/src/note_velocity.rs",
        "NoteVelocity",
        "permanent: every u16 is a valid velocity field, so the derive cannot admit a bad raw",
    ),
    (
        "crates/tutti-midi-device/src/id.rs",
        "DeviceId",
        "permanent: the OS mints the identity and ordinal, and any pair names a device; new has no check",
    ),
    (
        "crates/tutti-midi-device/src/learned.rs",
        "Learned",
        "permanent: each field is its own checked type or absent, so every combination is a valid report",
    ),
    (
        "crates/tutti-midi-device/src/learned.rs",
        "SavedDevice",
        "permanent: any listing row with any report is a saved device",
    ),
    (
        "crates/tutti-midi-protocol/src/mpe/zone.rs",
        "MpeZoneConfigRaw",
        "permanent: the persisted shape `TryFrom` reads and checks",
    ),
    (
        "crates/tutti-plugin-db/src/wire.rs",
        "Raw",
        "permanent: the persisted shape `TryFrom` reads and checks",
    ),
    (
        "crates/tutti-plugin-ipc/src/protocol/wire.rs",
        "Holder",
        "permanent: a test-local codec fixture, never read from a document",
    ),
    (
        "crates/tutti-plugin-ipc/src/protocol/wire.rs",
        "Plain",
        "permanent: a test-local codec fixture, never read from a document",
    ),
    (
        "crates/tutti-plugin-ipc/src/protocol/wire.rs",
        "Coded",
        "permanent: a test-local codec fixture, never read from a document",
    ),
    (
        "crates/tutti-plugin-types/src/automation.rs",
        "Raw",
        "permanent: the local shape `ParameterQueue`'s `Deserialize` reads, then checks",
    ),
    (
        "crates/tutti-plugin-types/src/classification.rs",
        "Vst3SubCategories",
        "permanent: `serde(from = String)`, and every string parses",
    ),
    (
        "crates/tutti-plugin-types/src/harmony.rs",
        "Got",
        "permanent: a test-local codec fixture, never read from a document",
    ),
    (
        "crates/tutti-plugin-types/src/hosting.rs",
        "Load",
        "permanent: every optional id is a choice of plugin; a missing one is the load's error",
    ),
    (
        "crates/tutti-plugin-types/src/parameters.rs",
        "ParameterInfo",
        "temporary: a document can set `flags` outside `known` and a `measures` its range does not fit; `serde(try_from)` removes it",
    ),
    (
        "crates/tutti-plugin/src/host/plugin_ref.rs",
        "PluginRef",
        "permanent: any path, format and id names a plugin; a missing one is the load's error",
    ),
    (
        "crates/tutti-plugin/src/host/plugin_ref.rs",
        "PluginState",
        "permanent: any plugin with any saved bytes is a state; the plugin refuses what it cannot read",
    ),
    (
        "crates/tutti-types/src/meter.rs",
        "TimeSignature",
        "permanent: each half is its own checked type, so every pair is a signature",
    ),
    (
        "crates/tutti-types/src/topology.rs",
        "ChannelTopology",
        "permanent: any ordered list of speakers is a topology; duplicates are not refused",
    ),
    (
        "crates/tutti-types/src/value/note.rs",
        "Note",
        "permanent: every class and octave is a note; only the MIDI conversion refuses",
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
            for head in [struct_head as Head, named_head as Head] {
                let mut at = 0;
                while let Some(m) = next_struct(&text, at, head) {
                    at = m.end;
                    if !m.private {
                        // a `pub` field, unit-like `()`, or no fields
                        continue;
                    }
                    let a = format!(
                        "{}{}",
                        attrs_before(&text, m.start),
                        &text[m.start..m.head_end]
                    );
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

/// How one pass reads a struct item at `at` (past its attributes): the end of
/// the item, its name, and whether every field is private.
type Head = fn(&str, usize) -> Option<Item>;

struct Item {
    /// Past the whole match: the rest of the scan resumes here.
    end: usize,
    /// Past the part the derive attributes can sit in (the header, not the
    /// body), so a field's own attribute is not read as the struct's.
    head_end: usize,
    name: String,
    private: bool,
}

/// One match of a struct pass.
struct Match {
    /// The start of the line the match begins on (`^`).
    start: usize,
    end: usize,
    head_end: usize,
    name: String,
    /// The first field is not a `pub` (and the struct is not unit-like): the
    /// regex's group 2 did not match.
    private: bool,
}

/// `re.finditer` from `from`: the next match, which can begin only at a line
/// start. Matches never overlap.
fn next_struct(text: &str, from: usize, head: Head) -> Option<Match> {
    let mut start = from;
    loop {
        // `^` under `re.M`: the start of the text or just after a `\n`.
        if (start == 0 || text.as_bytes()[start - 1] == b'\n')
            && let Some(m) = struct_at(text, start, head)
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

/// The pass `head` at the line start `start`.
fn struct_at(text: &str, start: usize, head: Head) -> Option<Match> {
    let at = skip_blanks(text, start);
    let it = after_attributes(text, at, head)?;
    Some(Match {
        start,
        end: it.end,
        head_end: it.head_end,
        name: it.name,
        private: it.private,
    })
}

/// `(?:#\[.*?\][ \t]*)*` then the rest: greedy in the number of attributes,
/// lazy in where each one's `]` is (`.` does not cross a line).
fn after_attributes(text: &str, at: usize, head: Head) -> Option<Item> {
    if text[at..].starts_with("#[") {
        let line_end = text[at..].find('\n').map_or(text.len(), |n| at + n);
        for (k, _) in text[at + 2..line_end].match_indices(']') {
            let next = skip_blanks(text, at + 2 + k + 1);
            if let Some(found) = after_attributes(text, next, head) {
                return Some(found);
            }
        }
    }
    head(text, at)
}

/// `(?:pub(?:\([^)]*\))?[ \t]+)?struct[ \t]+(\w+)(?:<[^>]*>)?[ \t]*\(
/// [ \t\n]*(pub(?![\w(])|\))?`
fn struct_head(text: &str, at: usize) -> Option<Item> {
    let (mut p, name) = struct_name(text, at)?;
    p = skip_blanks(text, p);
    if !text[p..].starts_with('(') {
        return None;
    }
    p += 1;
    while matches!(text.as_bytes().get(p), Some(b' ' | b'\t' | b'\n')) {
        p += 1;
    }
    let rest = &text[p..];
    let item = |end, private| Item {
        end,
        head_end: end,
        name: name.clone(),
        private,
    };
    if let Some(r) = rest.strip_prefix("pub")
        && !r.chars().next().is_some_and(|c| word(c) || c == '(')
    {
        return Some(item(p + 3, false));
    }
    if rest.starts_with(')') {
        return Some(item(p + 1, false));
    }
    Some(item(p, true))
}

/// `(?:pub(?:\([^)]*\))?[ \t]+)?struct[ \t]+(\w+)(?:<[^>]*>)?`: the position
/// after the generics, and the name.
fn struct_name(text: &str, at: usize) -> Option<(usize, String)> {
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
    Some((p, name))
}

/// `struct Name<..> [where ..] { fields }`: private when there is at least one
/// field and none is a bare `pub`. A field's attributes and comments are not
/// part of its visibility.
fn named_head(text: &str, at: usize) -> Option<Item> {
    let (p, name) = struct_name(text, at)?;
    let open = p + text[p..].find(['{', ';', '('])?;
    if text.as_bytes()[open] != b'{'
        || !text[p..open].trim().is_empty() && !is_where(&text[p..open])
    {
        return None;
    }
    let close = matching_brace(text, open)?;
    let fields = fields_of(&text[open + 1..close]);
    let private = !fields.is_empty() && fields.iter().all(|f| !is_bare_pub(f));
    Some(Item {
        end: close + 1,
        head_end: open,
        name,
        private,
    })
}

/// A `where` clause between the name and the `{`.
fn is_where(between: &str) -> bool {
    between.trim_start().starts_with("where") && !between.contains([';', '(', '{'])
}

/// The `}` closing the `{` at `open`; `None` when it never closes.
fn matching_brace(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The fields of a struct body: split at commas outside brackets, with `//`
/// comments and `#[..]` attributes removed. An empty piece is not a field.
fn fields_of(body: &str) -> Vec<String> {
    let mut clean = String::new();
    for line in body.lines() {
        clean.push_str(line.find("//").map_or(line, |i| &line[..i]));
        clean.push('\n');
    }
    let (mut out, mut cur, mut depth) = (Vec::new(), String::new(), 0i32);
    let mut prev = ' ';
    for c in clean.chars() {
        match c {
            '(' | '[' | '<' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '>' if prev != '-' => depth -= 1,
            _ => {}
        }
        if c == ',' && depth == 0 {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
        prev = c;
    }
    out.push(cur);
    out.into_iter()
        .map(|f| strip_attributes(f.trim()))
        .filter(|f| !f.is_empty())
        .collect()
}

/// `field` without its leading `#[..]` attributes.
fn strip_attributes(field: &str) -> String {
    let mut rest = field.trim_start();
    while rest.starts_with("#[") {
        let mut depth = 0i32;
        let mut end = rest.len();
        for (i, c) in rest.char_indices() {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = rest[end..].trim_start();
    }
    rest.to_owned()
}

/// The field starts with `pub` alone, not `pub(..)` or a name like `public`.
fn is_bare_pub(field: &str) -> bool {
    field
        .strip_prefix("pub")
        .is_some_and(|r| !r.chars().next().is_some_and(|c| word(c) || c == '('))
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

    #[test]
    fn a_pub_field_and_a_unit_struct_are_not_checked() {
        assert!(one("#[derive(serde::Deserialize)] struct P(pub u8);\n").passed());
        assert!(one("#[derive(serde::Deserialize)] struct P();\n").passed());
        assert!(one("#[derive(serde::Deserialize)] struct P(\n    pub u8);\n").passed());
        assert!(!one("#[derive(serde::Deserialize)] pub struct P(pub(crate) u8);\n").passed());
        assert!(!one("#[derive(serde::Deserialize)] struct P(pub_name: u8);\n").passed());
        assert!(!one("#[derive(serde::Deserialize)] struct P(u8, pub u8);\n").passed());
    }

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
        assert!(
            one("#[derive(serde::Deserialize)]\nstruct A { pub x: u8 }\nstruct P(u8);\n").passed()
        );
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
    fn every_exception_states_why_and_none_is_listed_twice() {
        assert!(
            EXCEPTIONS
                .iter()
                .all(|e| e.2.starts_with("permanent: ") || e.2.starts_with("temporary: "))
        );
        for (i, e) in EXCEPTIONS.iter().enumerate() {
            assert!(
                EXCEPTIONS[i + 1..].iter().all(|o| (o.0, o.1) != (e.0, e.1)),
                "{} in {} is listed twice",
                e.1,
                e.0
            );
        }
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

    const NAMED: &str = "#[derive(serde::Deserialize)]\nstruct P {\n    a: u8,\n    b: u8,\n}\n";

    #[test]
    fn a_named_struct_with_only_private_fields_is_flagged() {
        let r = one(NAMED);
        assert_eq!(r.findings, ["check-range-types: FAIL", &flagged("P", 2)]);
        let r = one(
            "#[cfg_attr(feature = \"serde\", derive(serde::Deserialize))]\npub struct P<T> where T: A {\n    /// docs\n    #[serde(default)]\n    a: Vec<(u8, u8)>,\n    pub(crate) b: T,\n}\n",
        );
        assert_eq!(r.findings[1], flagged("P", 2));
        let try_from = "#[derive(serde::Deserialize)]\n#[serde(try_from = \"Raw\")]\nstruct P {\n    a: u8,\n}\n";
        assert!(one(try_from).passed());
        // A field's own `try_from` is not the struct's.
        let field = "#[derive(serde::Deserialize)]\nstruct P {\n    #[serde(try_from = \"u8\")]\n    a: u8,\n}\n";
        assert_eq!(one(field).findings[1], flagged("P", 2));
        // A struct without the derive is not checked.
        assert!(one("#[derive(Debug)]\nstruct P {\n    a: u8,\n}\n").passed());
    }

    #[test]
    fn a_named_struct_with_a_pub_field_is_skipped() {
        let pub_last = NAMED.replace("    b: u8", "    pub b: u8");
        assert!(one(&pub_last).passed());
        let pub_first = NAMED.replace("    a: u8", "    pub a: u8");
        assert!(one(&pub_first).passed());
        // A field named `public` is private, and an empty struct has no field.
        assert!(!one(&NAMED.replace("    a: u8", "    public: u8")).passed());
        assert!(one("#[derive(serde::Deserialize)]\nstruct P {}\n").passed());
        assert!(one("#[derive(serde::Deserialize)]\nstruct P;\n").passed());
    }

    #[test]
    fn a_named_exception_passes_and_a_stale_one_fails() {
        let ex = [(FILE, "P", "permanent: every combination is valid")];
        let r = run(&[(FILE, NAMED)], &ex);
        assert_eq!(r.out, ["check-range-types: ok (1 excepted)"]);
        // The struct gained `try_from`: the exception no longer matches.
        let fixed = NAMED.replace("struct P", "#[serde(try_from = \"R\")]\nstruct P");
        let r = run(&[(FILE, &fixed)], &ex);
        assert_eq!(
            r.findings,
            [
                "check-range-types: FAIL".to_owned(),
                format!("  stale exception P in {FILE} (permanent: every combination is valid)")
            ]
        );
        // The struct no longer derives `Deserialize`.
        let r = run(&[(FILE, "struct P {\n    a: u8,\n}\n")], &ex);
        assert!(!r.passed());
        // The struct is renamed.
        let r = run(&[(FILE, &NAMED.replace("struct P", "struct Q"))], &ex);
        assert_eq!(r.findings.len(), 3, "{:?}", r.findings);
    }
}
