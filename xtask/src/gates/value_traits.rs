//! `check-value-traits`: a fieldless public enum that is `Eq` is also `Hash`.
//!
//! A `pub` enum whose variants are all unit variants and that derives both
//! `PartialEq` and `Eq` must derive `Hash` too, so it can key a `HashMap` or a
//! `HashSet`. An enum that cannot is a row in [`ALLOW`]: `(file, type, reason)`,
//! the reason starting `permanent:` (the type must not be `Hash`) or `temporary:`
//! (a lane owes the derive). A row that no longer matches a violation is stale
//! and fails.
//!
//! Every `crates/*/src/**/*.rs` is read with comments and literals blanked, a
//! `#[cfg(test)]` item is skipped, and an attribute list is read across lines.
//! An enum behind `pub(crate)` or no `pub` is not public. A derive written as a
//! path (`std::hash::Hash`) counts by its last segment.

use super::lex::{blank, line_of, matching};
use super::{Report, Tree};
use std::collections::BTreeSet;

/// The fewest source files a real run reads; fewer means the walk is broken.
pub(super) const MIN_FILES: usize = 100;

/// `(file, type, reason)`.
pub(super) const ALLOW: &[(&str, &str, &str)] = &[];

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git")
}

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The byte ranges of items behind `#[cfg(test)]`, in blanked text.
fn test_ranges(code: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(p) = code[from..].find("#[cfg(test)]") {
        let at = from + p;
        let after = at + "#[cfg(test)]".len();
        from = after;
        let Some(stop) = code[after..].find(['{', ';']).map(|i| after + i) else {
            continue;
        };
        if code.as_bytes()[stop] == b'{' {
            if let Some(end) = matching(code, stop, b'{', b'}') {
                out.push((at, end));
                from = end;
            }
        } else {
            out.push((at, stop));
        }
    }
    out
}

/// The attribute bodies (`derive(Debug)`) written before `at`, and whether the
/// item is introduced by a bare `pub`.
fn attrs_before(code: &str, at: usize) -> Option<Vec<String>> {
    let head = code[..at].trim_end();
    let head = head.strip_suffix("pub")?;
    if head.as_bytes().last().is_some_and(|&b| word(b)) {
        return None;
    }
    let mut head = head.trim_end();
    let mut attrs = Vec::new();
    while head.ends_with(']') {
        let b = head.as_bytes();
        let mut depth = 0;
        let mut open = None;
        for i in (0..b.len()).rev() {
            match b[i] {
                b']' => depth += 1,
                b'[' => {
                    depth -= 1;
                    if depth == 0 {
                        open = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let open = open?;
        let before = head[..open].trim_end();
        let before = before.strip_suffix('#')?;
        attrs.push(head[open + 1..head.len() - 1].trim().to_owned());
        head = before.trim_end();
    }
    Some(attrs)
}

/// The trait names a list of attribute bodies derives.
fn derived(attrs: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for a in attrs {
        let Some(rest) = a.strip_prefix("derive") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(inner) = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')) else {
            continue;
        };
        for t in inner.split(',') {
            let t = t.trim();
            if let Some(last) = t.rsplit("::").next().filter(|s| !s.is_empty()) {
                out.insert(last.to_owned());
            }
        }
    }
    out
}

/// Whether every variant of the enum body is a unit variant (and there is one).
fn only_unit_variants(body: &str) -> bool {
    let mut variants: Vec<String> = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    for c in body.chars() {
        match c {
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth -= 1,
            _ => {}
        }
        if c == ',' && depth == 0 {
            variants.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    variants.push(cur);
    let mut any = false;
    for v in variants {
        let v = strip_attrs(&v);
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        any = true;
        let eq = v.find('=');
        if let Some(p) = v.find(['(', '{'])
            && eq.is_none_or(|e| p < e)
        {
            return false;
        }
    }
    any
}

fn strip_attrs(v: &str) -> String {
    let mut out = String::new();
    let mut rest = v;
    while let Some(p) = rest.find("#[") {
        out.push_str(&rest[..p]);
        match matching(rest, p + 1, b'[', b']') {
            Some(end) => rest = &rest[end + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// One enum the gate has an opinion on: public, fieldless, `PartialEq + Eq`.
struct Found {
    file: String,
    name: String,
    line: usize,
    hash: bool,
}

fn scan(file: &str, text: &str, out: &mut Vec<Found>) {
    let code = blank(text);
    let tests = test_ranges(&code);
    let mut from = 0;
    while let Some(p) = code[from..].find("enum") {
        let at = from + p;
        from = at + 4;
        let b = code.as_bytes();
        if (at > 0 && word(b[at - 1])) || b.get(at + 4).is_none_or(|&c| word(c)) {
            continue;
        }
        if tests.iter().any(|&(s, e)| s <= at && at <= e) {
            continue;
        }
        let Some(attrs) = attrs_before(&code, at) else {
            continue;
        };
        let name: String = code[at + 4..]
            .trim_start()
            .chars()
            .take_while(|&c| word(c as u8))
            .collect();
        let Some(open) = code[at..].find('{').map(|i| at + i) else {
            continue;
        };
        let Some(close) = matching(&code, open, b'{', b'}') else {
            continue;
        };
        let traits = derived(&attrs);
        if !(traits.contains("PartialEq") && traits.contains("Eq")) {
            continue;
        }
        if !only_unit_variants(&code[open + 1..close]) {
            continue;
        }
        out.push(Found {
            file: file.to_owned(),
            name,
            line: line_of(&code, at),
            hash: traits.contains("Hash"),
        });
    }
}

pub(super) fn check(tree: &dyn Tree, allow: &[(&str, &str, &str)], min_files: usize) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let mut found = Vec::new();
    let mut files = 0;
    for path in tree.files("crates", &skipped) {
        let in_src = path.ends_with(".rs")
            && path
                .strip_prefix("crates/")
                .and_then(|r| r.split_once('/'))
                .is_some_and(|(_, rest)| rest.starts_with("src/"));
        if !in_src {
            continue;
        }
        if let Some(text) = tree.read(&path) {
            files += 1;
            scan(&path, &text, &mut found);
        }
    }
    if files < min_files {
        bad.push(format!(
            "only {files} source files read; the scan is broken"
        ));
    }

    for (file, name, reason) in allow {
        if !(reason.starts_with("permanent:") || reason.starts_with("temporary:")) {
            bad.push(format!(
                "ALLOW {file} {name}: the reason must start `permanent:` or `temporary:`"
            ));
        }
        let hit = found
            .iter()
            .any(|f| f.file == *file && f.name == *name && !f.hash);
        if !hit {
            bad.push(format!(
                "ALLOW {file} {name}: stale (the enum is gone, derives Hash, or is no longer a fieldless Eq enum); remove the row"
            ));
        }
    }
    for f in found.iter().filter(|f| !f.hash) {
        if allow
            .iter()
            .any(|(file, n, _)| *file == f.file && *n == f.name)
        {
            continue;
        }
        bad.push(format!(
            "{}:{}: `pub enum {}` derives PartialEq and Eq but not Hash (derive it, or add a row to ALLOW in xtask/src/gates/value_traits.rs)",
            f.file, f.line, f.name
        ));
    }

    let mut r = Report::default();
    if bad.is_empty() {
        r.say(format!(
            "check-value-traits: ok ({} fieldless Eq enums, {} allowed)",
            found.len(),
            allow.len()
        ));
    } else {
        r.fail("check-value-traits: FAIL");
        for b in bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    fn run(src: &str, allow: &[(&str, &str, &str)]) -> Report {
        let m = Mem::with(&[("crates/c/src/lib.rs", src)]);
        check(&m, allow, 1)
    }

    const F: &str = "crates/c/src/lib.rs";

    #[test]
    fn a_walk_that_reads_too_few_files_fails() {
        let r = check(&Mem::default(), &[], 5);
        assert!(r.findings[1].contains("the scan is broken"));
    }

    #[test]
    fn a_fieldless_eq_enum_without_hash_is_flagged() {
        let r = run(
            "#[derive(Debug, PartialEq, Eq)]\npub enum Mode { A, B }\n",
            &[],
        );
        assert_eq!(r.findings[0], "check-value-traits: FAIL");
        assert!(r.findings[1].contains("crates/c/src/lib.rs:2: `pub enum Mode`"));
    }

    #[test]
    fn a_derive_with_hash_passes() {
        let r = run(
            "#[derive(Debug, PartialEq, Eq, Hash)]\npub enum Mode { A, B }\n",
            &[],
        );
        assert!(r.passed(), "{:?}", r.findings);
        assert!(r.out[0].contains("1 fieldless Eq enums"));
    }

    #[test]
    fn a_hash_written_as_a_path_counts() {
        let r = run(
            "#[derive(PartialEq, Eq, std::hash::Hash)]\npub enum Mode { A }\n",
            &[],
        );
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn an_enum_that_is_not_eq_is_skipped() {
        let r = run("#[derive(PartialEq)]\npub enum E { A }\n", &[]);
        assert!(r.passed(), "{:?}", r.findings);
        let r = run("#[derive(Debug, Clone)]\npub enum E { A }\n", &[]);
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn an_enum_with_a_data_variant_is_skipped() {
        let r = run(
            "#[derive(PartialEq, Eq)]\npub enum E { A, B(u8), C { x: u8 } }\n",
            &[],
        );
        assert!(r.passed(), "{:?}", r.findings);
        assert!(r.out[0].contains("0 fieldless"));
    }

    #[test]
    fn discriminants_and_variant_attributes_are_still_unit_variants() {
        let r = run(
            "#[derive(PartialEq, Eq)]\n#[repr(u8)]\npub enum E {\n    #[doc = \"x\"]\n    A = 1,\n    B = 2,\n}\n",
            &[],
        );
        assert!(r.findings[1].contains("`pub enum E`"));
    }

    #[test]
    fn a_non_pub_enum_is_skipped() {
        for vis in ["", "pub(crate) "] {
            let r = run(
                &format!("#[derive(PartialEq, Eq)]\n{vis}enum E {{ A }}\n"),
                &[],
            );
            assert!(r.passed(), "{vis}: {:?}", r.findings);
        }
    }

    #[test]
    fn a_multi_line_derive_and_stacked_attributes_are_read() {
        let flagged = "#[derive(\n    Debug,\n    PartialEq,\n    Eq,\n)]\n#[non_exhaustive]\npub enum E { A }\n";
        assert!(!run(flagged, &[]).passed());
        let hashed = "#[derive(\n    Debug,\n    PartialEq,\n    Eq,\n    Hash,\n)]\n#[non_exhaustive]\npub enum E { A }\n";
        assert!(run(hashed, &[]).passed());
    }

    #[test]
    fn a_cfg_test_module_is_skipped() {
        let src =
            "#[cfg(test)]\nmod tests {\n    #[derive(PartialEq, Eq)]\n    pub enum E { A }\n}\n";
        assert!(run(src, &[]).passed());
        let after = "#[cfg(test)]\nmod tests {}\n#[derive(PartialEq, Eq)]\npub enum E { A }\n";
        assert!(!run(after, &[]).passed());
    }

    #[test]
    fn comments_and_strings_are_not_code() {
        let src = "// #[derive(PartialEq, Eq)]\n// pub enum E { A }\nconst S: &str = \"pub enum E { A }\";\n";
        let r = run(src, &[]);
        assert!(r.passed(), "{:?}", r.findings);
        assert!(r.out[0].contains("0 fieldless"));
    }

    #[test]
    fn an_allowed_enum_passes() {
        let r = run(
            "#[derive(PartialEq, Eq)]\npub enum E { A }\n",
            &[(F, "E", "permanent: a key would hide a bug")],
        );
        assert!(r.passed(), "{:?}", r.findings);
        assert!(r.out[0].contains("1 allowed"));
    }

    #[test]
    fn a_stale_allow_row_fails() {
        let allow = [(F, "E", "temporary: a lane adds Hash")];
        let gone = run("pub fn f() {}\n", &allow);
        assert!(gone.findings[1].contains("stale"));
        let hashed = run("#[derive(PartialEq, Eq, Hash)]\npub enum E { A }\n", &allow);
        assert!(hashed.findings[1].contains("stale"));
    }

    #[test]
    fn an_allow_reason_must_say_permanent_or_temporary() {
        let r = run(
            "#[derive(PartialEq, Eq)]\npub enum E { A }\n",
            &[(F, "E", "because")],
        );
        assert!(r.findings[1].contains("must start `permanent:` or `temporary:`"));
    }
}
