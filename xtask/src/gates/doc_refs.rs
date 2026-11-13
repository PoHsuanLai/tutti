//! `check-doc-refs`: prose names what exists.
//!
//! In the [prose](super::prose) of the repo:
//!
//! - a path rooted at a workspace crate (`tutti_graph::Node::process`,
//!   `arc_lend::Cell`) names, segment by segment, identifiers that occur in
//!   that crate's code (`src/`, comments and literals blanked), or past the
//!   first segment in the code of a crate it names (a re-export). A crate's
//!   `pub use other as alias;` continues a path through `alias` in `other`,
//!   and `PREFIX_*` names any identifier starting with `PREFIX_`;
//! - a design-doc citation (`doc NNN`, `design NNN`, `docs/design/NNN-`) names
//!   a file in `docs/design/` or `docs/archive/`, and a `§N.N` after it names
//!   a numbered heading of that file.
//!
//! Rustdoc refuses a broken intra-doc link; this gate covers what rustdoc
//! does not read: a path in backticks, in a `//` comment or in a `.md` file.
//!
//! Limits: a segment resolves when its identifier occurs anywhere in the
//! crate's code (an item, a variant, a method, a local), so a real identifier
//! in the wrong place passes. Only the first number of a list (`docs 013,
//! 014`) is read.

use super::lex::{blank, line_of};
use super::prose::{prose, word};
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// A workspace crate, as its code names it.
struct Krate {
    /// Every identifier in the crate's `src/` code.
    idents: BTreeSet<String>,
    /// `pub use <crate> as <alias>;`: alias to crate name.
    aliases: BTreeMap<String, String>,
}

fn skipped(dir: &str) -> bool {
    dir == "target"
}

/// The identifier starting at `at`, or `""`.
fn ident_at(s: &str, at: usize) -> &str {
    let b = s.as_bytes();
    let mut e = at;
    while e < b.len() && word(b[e]) {
        e += 1;
    }
    &s[at..e]
}

/// Every workspace crate under `crates/` and `testing/`, by its code name.
fn krates(tree: &dyn Tree) -> BTreeMap<String, Krate> {
    let mut dirs = BTreeSet::new();
    for top in ["crates", "testing"] {
        for path in tree.files(top, &skipped) {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() == 3 && parts[2] == "Cargo.toml" {
                dirs.insert(format!("{top}/{}", parts[1]));
            }
        }
    }
    let mut out = BTreeMap::new();
    for dir in dirs {
        let name = dir.rsplit('/').next().unwrap_or(&dir).replace('-', "_");
        let mut k = Krate {
            idents: BTreeSet::new(),
            aliases: BTreeMap::new(),
        };
        for path in tree.files(&format!("{dir}/src"), &skipped) {
            if !path.ends_with(".rs") {
                continue;
            }
            let code = blank(&tree.read(&path).unwrap_or_default());
            let b = code.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if word(b[i]) {
                    let id = ident_at(&code, i);
                    i += id.len();
                    if !id.as_bytes()[0].is_ascii_digit() {
                        k.idents.insert(id.to_owned());
                    }
                } else {
                    i += 1;
                }
            }
            for line in code.lines() {
                let t = line.trim();
                let Some(rest) = t.strip_prefix("pub use ").and_then(|r| r.strip_suffix(';'))
                else {
                    continue;
                };
                if let Some((from, alias)) = rest.split_once(" as ")
                    && !from.contains("::")
                {
                    k.aliases
                        .insert(alias.trim().to_owned(), from.trim().to_owned());
                }
            }
        }
        out.insert(name, k);
    }
    out
}

/// The `::`-separated identifiers after `at` (which is just past the crate
/// name), stopping at anything that is not `::ident`.
fn segments(text: &str, mut at: usize) -> Vec<&str> {
    let mut out = Vec::new();
    while text[at..].starts_with("::") {
        let id = ident_at(text, at + 2);
        if id.is_empty() {
            break;
        }
        at += 2 + id.len();
        if text[at..].starts_with('*') {
            // `CMD_*`: a prefix, kept with its star.
            out.push(&text[at - id.len()..=at]);
            break;
        }
        out.push(id);
    }
    out
}

/// Whether `seg` (an identifier, or a prefix ending in `*`) is one of `idents`.
fn holds(idents: &BTreeSet<String>, seg: &str) -> bool {
    match seg.strip_suffix('*') {
        Some(prefix) => idents
            .range(prefix.to_owned()..)
            .next()
            .is_some_and(|i| i.starts_with(prefix)),
        None => idents.contains(seg),
    }
}

/// The first segment of `segs` that `krate` does not define, walking aliases.
fn unresolved<'a>(
    krates: &BTreeMap<String, Krate>,
    krate: &str,
    segs: &[&'a str],
) -> Option<(String, &'a str)> {
    let mut cur = krate.to_owned();
    let mut aliasing = true;
    for (n, &seg) in segs.iter().enumerate() {
        let k = &krates[&cur];
        if aliasing
            && let Some(target) = k.aliases.get(seg)
            && krates.contains_key(target)
        {
            cur = target.clone();
            continue;
        }
        aliasing = false;
        if holds(&k.idents, seg) {
            continue;
        }
        // A re-export: past the first segment, an identifier of a crate this
        // one names resolves too.
        let reexported = n > 0
            && krates
                .iter()
                .any(|(name, other)| k.idents.contains(name) && holds(&other.idents, seg));
        if !reexported {
            return Some((cur, seg));
        }
    }
    None
}

/// The design and archive docs by number: each one's text.
fn docs(tree: &dyn Tree) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for dir in ["docs/design", "docs/archive"] {
        for path in tree.files(dir, &skipped) {
            let name = path.rsplit('/').next().unwrap_or(&path);
            let num = &name[..name.len().min(3)];
            if num.len() == 3
                && num.bytes().all(|b| b.is_ascii_digit())
                && name[3..].starts_with('-')
            {
                out.insert(num.to_owned(), tree.read(&path).unwrap_or_default());
            }
        }
    }
    out
}

/// Whether `doc` has a heading numbered `sec` (`## 2.`, `### 2.7.6 Title`,
/// `## §3`).
fn has_section(doc: &str, sec: &str) -> bool {
    doc.lines().any(|l| {
        let Some(h) = l.strip_prefix('#') else {
            return false;
        };
        let h = h.trim_start_matches('#').trim_start();
        let h = h.strip_prefix('§').unwrap_or(h);
        let Some(rest) = h.strip_prefix(sec) else {
            return false;
        };
        let rb = rest.as_bytes();
        match rb.first() {
            None => true,
            Some(b'.') => !rb.get(1).is_some_and(|c| c.is_ascii_alphanumeric()),
            Some(c) => !c.is_ascii_alphanumeric(),
        }
    })
}

/// Every design-doc citation in `text`: (offset, number, section).
fn citations(text: &str) -> Vec<(usize, &str, Option<&str>)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !word(b[i]) || (i > 0 && word(b[i - 1])) {
            i += 1;
            continue;
        }
        let id = ident_at(text, i);
        let mut after = i + id.len();
        let lead = if text[i..].starts_with("docs/design/") {
            after = i + "docs/design/".len();
            Some(after)
        } else if text[i..].starts_with("docs/archive/") {
            after = i + "docs/archive/".len();
            Some(after)
        } else if matches!(id, "doc" | "docs" | "Doc" | "Docs" | "design" | "Design") {
            let ws = text[after..].len() - text[after..].trim_start_matches(' ').len();
            (ws > 0).then(|| after + ws)
        } else {
            None
        };
        let Some(n) = lead else {
            i = after.max(i + 1);
            continue;
        };
        let num = &text[n..(n + 3).min(text.len())];
        let tail = b.get(n + 3).copied();
        if num.len() != 3
            || !num.bytes().all(|c| c.is_ascii_digit())
            || tail.is_some_and(|c| c.is_ascii_digit())
        {
            i = after.max(i + 1);
            continue;
        }
        let mut s = n + 3;
        if text[s..].starts_with(',') {
            s += 1;
        }
        s += text[s..].len() - text[s..].trim_start_matches(' ').len();
        let sec = text[s..].strip_prefix('§').and_then(|r| {
            let end = r
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
                .unwrap_or(r.len());
            let sec = r[..end].trim_end_matches('.');
            (!sec.is_empty()).then_some(sec)
        });
        out.push((i, num, sec));
        i = n + 3;
    }
    out
}

pub(super) fn check(tree: &dyn Tree) -> Report {
    let krates = krates(tree);
    let docs = docs(tree);
    let mut bad = Vec::new();
    for p in prose(tree) {
        let text = &p.text;
        let b = text.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if !word(b[i]) || (i > 0 && (word(b[i - 1]) || b[i - 1] == b':')) {
                i += 1;
                continue;
            }
            let id = ident_at(text, i);
            let end = i + id.len();
            if krates.contains_key(id) && text[end..].starts_with("::") {
                let segs = segments(text, end);
                if let Some((krate, seg)) = unresolved(&krates, id, &segs) {
                    bad.push(format!(
                        "{}:{}: `{id}::{}` names `{seg}`, which {krate}'s code does not hold",
                        p.path,
                        line_of(text, i),
                        segs.join("::"),
                    ));
                }
            }
            i = end;
        }
        for (at, num, sec) in citations(text) {
            let line = line_of(text, at);
            match (docs.get(num), sec) {
                (None, _) => bad.push(format!(
                    "{}:{line}: cites design doc {num}, which is not in docs/design or docs/archive",
                    p.path
                )),
                (Some(doc), Some(sec)) if !has_section(doc, sec) => bad.push(format!(
                    "{}:{line}: cites doc {num} §{sec}, which has no such heading",
                    p.path
                )),
                _ => {}
            }
        }
    }
    let mut r = Report::default();
    if bad.is_empty() {
        r.say("check-doc-refs: OK");
    } else {
        r.fail("check-doc-refs: FAIL (prose names what does not exist):");
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

    const LIB: &str =
        "pub struct Node;\nimpl Node { pub fn process(&self) {} }\npub use tutti_b as b;\n";

    fn run(prose: &str) -> Vec<String> {
        let tree = Mem::with(&[
            ("crates/tutti-a/Cargo.toml", ""),
            ("crates/tutti-a/src/lib.rs", LIB),
            ("crates/tutti-b/Cargo.toml", ""),
            ("crates/tutti-b/src/lib.rs", "pub enum Mode { Fast }\n"),
            ("crates/tutti-c/Cargo.toml", ""),
            ("crates/tutti-c/src/lib.rs", prose),
            (
                "docs/design/013-graph.md",
                "# 013\n## 2. Design\n### 2.7.6 Sums\n## §3 Rest\n",
            ),
            ("docs/archive/007-old.md", "# 007\n"),
        ]);
        check(&tree).findings
    }

    #[test]
    fn a_path_whose_segments_the_crate_holds_resolves() {
        assert!(run("// `tutti_a::Node::process` and tutti_a::Node.\n").is_empty());
        assert!(run("/// [`Mode`](tutti_a::b::Mode::Fast), `tutti_b::Mode`\n").is_empty());
        assert!(run("// `tutti_a::*` and `tutti_a::{Node}` stop at the glob\n").is_empty());
        assert!(run("// `tutti_a::No*` is a prefix of `Node`\n").is_empty());
        assert!(
            run("// `tutti_a::Node::Fast`: tutti_a names tutti_b, which holds it\n").is_empty()
        );
    }

    #[test]
    fn a_segment_the_crate_does_not_hold_fails_naming_it() {
        let bad = run("// x\n// `tutti_a::Node::render`\n");
        assert_eq!(
            bad[1],
            "  crates/tutti-c/src/lib.rs:2: `tutti_a::Node::render` names `render`, which tutti_a's code does not hold"
        );
        assert_eq!(run("// `tutti_a::downmix`\n").len(), 2);
        assert_eq!(
            run("// `tutti_a::Fast`: a re-export is not a root item\n").len(),
            2
        );
        assert_eq!(run("// `tutti_a::Zz*`\n").len(), 2);
        assert_eq!(
            run("// `tutti_a::b::Slow`\n").len(),
            2,
            "{:?}",
            run("// `tutti_a::b::Slow`\n")
        );
    }

    #[test]
    fn code_and_strings_are_not_prose() {
        assert!(run("fn f() { let s = \"tutti_a::Gone\"; }\n").is_empty());
    }

    #[test]
    fn a_citation_names_a_doc_and_a_section_it_has() {
        assert!(run("// doc 013, design 013 §2.7.6, doc 013 §2, doc 013 §3, docs/design/013-graph.md, doc 007\n").is_empty());
        let bad = run("// design 022 §1\n// doc 013 §2.8\n// doc 013 §2.7\n");
        assert_eq!(
            bad[1..],
            [
                "  crates/tutti-c/src/lib.rs:1: cites design doc 022, which is not in docs/design or docs/archive",
                "  crates/tutti-c/src/lib.rs:2: cites doc 013 §2.8, which has no such heading",
                "  crates/tutti-c/src/lib.rs:3: cites doc 013 §2.7, which has no such heading",
            ]
        );
        assert_eq!(run("// docs/design/026-stretch.md\n").len(), 2);
    }

    #[test]
    fn numbers_that_are_not_citations_pass() {
        assert!(run("// a doc 1234 long, doc 12, the design 2026 roadmap, mydoc 999\n").is_empty());
    }
}
