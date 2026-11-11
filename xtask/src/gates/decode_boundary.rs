//! The decoder is swapped by editing one module: `symphonia` is named in
//! `tutti-codec/src/decode.rs` and `src/decode/` alone, and no `pub` item there
//! exposes it (part of `check-layout`).
//!
//! A test oracle may read with symphonia (`tests/`, `benches/`, `examples/`:
//! it is a dev-dependency). The scan is per file: it collects the names the
//! file imports from symphonia (an `as` rename counts as the new name, a glob
//! import is refused) and every `type` alias whose right side uses one, then
//! fails when a `pub` item's header, a `pub` field, or the whole body of a
//! `pub enum` or `pub trait` uses `symphonia`, one of those names, or an
//! `impl` header does. A `pub(super)` or private item may use them.
//!
//! What the scan cannot see (a textual scan, not the compiler): an item a
//! macro writes; a public type reached through a type of ours in another file
//! that wraps a symphonia type privately yet exposes it through a trait it
//! implements or an associated type, since each file is scanned alone; an
//! `impl Trait` return or an inferred type that is symphonia's; a trait-impl
//! method whose signature the trait fixes; a rename through `use super::..` of
//! a name another file imported from symphonia; and `cfg`-ed text (it reads
//! every cfg arm). The complete check is the crate's rustdoc JSON or `cargo
//! public-api`, which this repo does not run.

use super::Tree;
use super::lex::{blank, find_word_from, line_of};
use std::collections::BTreeSet;

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git")
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The start offsets of every line.
fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    v.retain(|&i| i <= text.len());
    v
}

/// Past spaces and tabs.
fn indent(text: &str, at: usize) -> usize {
    at + text[at..].len() - text[at..].trim_start_matches([' ', '\t']).len()
}

/// Whether `s` starts with the keyword `kw` at a word boundary.
fn keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(kw)?;
    (!rest.starts_with(is_word)).then_some(rest)
}

/// To just before the first `{` or `;`, or the end.
fn header_end(text: &str, from: usize) -> usize {
    text[from..]
        .find(['{', ';'])
        .map_or(text.len(), |p| from + p)
}

/// `[^{;]*` after `pub (const |unsafe |async )*KW\b`, the regex's backtracking
/// over the modifiers kept.
fn pub_item_end(text: &str, at: usize) -> Option<usize> {
    const KEYWORDS: [&str; 9] = [
        "fn", "struct", "enum", "type", "trait", "use", "static", "const", "mod",
    ];
    text[at..].strip_prefix("pub ")?;
    let mut pos = at + 4;
    let mut offsets = vec![pos];
    loop {
        let rest = &text[pos..];
        let Some(m) = ["const ", "unsafe ", "async "]
            .iter()
            .find(|m| rest.starts_with(**m))
        else {
            break;
        };
        pos += m.len();
        offsets.push(pos);
    }
    // Greedy first, then fewer modifiers.
    for &p in offsets.iter().rev() {
        if KEYWORDS.iter().any(|k| keyword(&text[p..], k).is_some()) {
            return Some(header_end(text, p));
        }
    }
    None
}

/// `pub \w+:[^,\n]*`.
fn pub_field_end(text: &str, at: usize) -> Option<usize> {
    let rest = text[at..].strip_prefix("pub ")?;
    let n = rest.find(|c: char| !is_word(c)).unwrap_or(rest.len());
    if n == 0 || !rest[n..].starts_with(':') {
        return None;
    }
    let from = at + 4 + n + 1;
    Some(
        text[from..]
            .find([',', '\n'])
            .map_or(text.len(), |p| from + p),
    )
}

/// The spans of `public_item`'s three alternatives, non-overlapping, leftmost.
fn public_items(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut next = 0;
    for ls in line_starts(text) {
        if ls < next {
            continue;
        }
        let at = indent(text, ls);
        let end = pub_item_end(text, at)
            .or_else(|| pub_field_end(text, at))
            .or_else(|| keyword(&text[at..], "impl").map(|_| header_end(text, at)));
        if let Some(end) = end {
            out.push((ls, end));
            next = end;
        }
    }
    out
}

/// `pub enum|trait .. {` through its closing brace.
fn pub_bodies(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for ls in line_starts(text) {
        let at = indent(text, ls);
        let Some(rest) = text[at..].strip_prefix("pub ") else {
            continue;
        };
        let Some(after) = keyword(rest, "enum").or_else(|| keyword(rest, "trait")) else {
            continue;
        };
        let from = text.len() - after.len();
        let end = header_end(text, from);
        if !text[end..].starts_with('{') {
            continue;
        }
        let mut depth = 1;
        let mut i = end + 1;
        for c in text[i..].chars() {
            if depth == 0 {
                break;
            }
            depth += i32::from(c == '{') - i32::from(c == '}');
            i += c.len_utf8();
        }
        out.push((ls, i));
    }
    out
}

/// `(?<!::)\bn\b` in `hay`.
fn uses_name(hay: &str, n: &str) -> bool {
    let mut from = 0;
    while let Some(at) = find_word_from(hay, n, from) {
        if !hay[..at].ends_with("::") {
            return true;
        }
        from = at + 1;
    }
    false
}

/// `use symphonia ..;` statements' text after `symphonia`, one per statement.
fn symphonia_uses(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for ls in line_starts(text) {
        let mut at = indent(text, ls);
        if let Some(r) = text[at..].strip_prefix("pub") {
            let r2 = r
                .strip_prefix(|c| c == '(')
                .map_or(r, |p| p.find(')').map_or(r, |c| &p[c + 1..]));
            let r3 = r2.trim_start_matches([' ', '\t']);
            if r3.len() == r2.len() {
                continue;
            }
            at = text.len() - r3.len();
        }
        let Some(r) = text[at..].strip_prefix("use") else {
            continue;
        };
        let r2 = r.trim_start_matches([' ', '\t']);
        if r2.len() == r.len() {
            continue;
        }
        let Some(after) = keyword(r2, "symphonia") else {
            continue;
        };
        if let Some(semi) = after.find(';') {
            out.push(&after[..semi]);
        }
    }
    out
}

/// `(alias, right-hand side)` of each `type` alias.
fn type_aliases(text: &str) -> Vec<(String, &str)> {
    let mut out = Vec::new();
    for ls in line_starts(text) {
        let mut at = indent(text, ls);
        if let Some(r) = text[at..].strip_prefix("pub") {
            let r2 = r
                .strip_prefix('(')
                .map_or(r, |p| p.find(')').map_or(r, |c| &p[c + 1..]));
            let r3 = r2.trim_start_matches([' ', '\t']);
            if r3.len() != r2.len() {
                at = text.len() - r3.len();
            }
        }
        let Some(r) = text[at..].strip_prefix("type") else {
            continue;
        };
        let r2 = r.trim_start_matches([' ', '\t']);
        if r2.len() == r.len() {
            continue;
        }
        let name: String = r2.chars().take_while(|&c| is_word(c)).collect();
        if name.is_empty() {
            continue;
        }
        let mut rest = &r2[name.len()..];
        if rest.starts_with('<') {
            let Some(gt) = rest.find('>') else { continue };
            rest = &rest[gt + 1..];
        }
        let Some(rest) = rest.trim_start_matches([' ', '\t']).strip_prefix('=') else {
            continue;
        };
        if let Some(semi) = rest.find(';') {
            out.push((name, &rest[..semi]));
        }
    }
    out
}

/// Words of `s` not followed by `\s*::`, with the word after `as`.
fn imported_names(stmt: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let b = stmt.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !(b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
        }
        let word = &stmt[start..i];
        let rest = &stmt[i..];
        let after = rest.trim_start();
        if after.starts_with("::") {
            continue;
        }
        let renamed = rest.len() != after.len() && keyword(after, "as").is_some();
        if renamed || word == "as" || word == "self" {
            continue;
        }
        names.insert(word.to_owned());
    }
    // `as NAME` adds the new name.
    let mut from = 0;
    while let Some(at) = find_word_from(stmt, "as", from) {
        let rest = stmt[at + 2..].trim_start();
        let name: String = rest.chars().take_while(|&c| is_word(c)).collect();
        if !name.is_empty() && rest.len() != stmt[at + 2..].len() {
            names.insert(name);
        }
        from = at + 2;
    }
    names
}

fn names_in_file(code: &str, rel: &str, bad: &mut Vec<String>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for stmt in symphonia_uses(code) {
        if stmt.contains('*') {
            bad.push(format!(
                "{rel}: a glob import from symphonia hides the names it brings in"
            ));
        }
        names.extend(imported_names(stmt));
        if find_word_from(stmt, "self", 0).is_some() {
            // The last `\w+::` segment.
            let mut last = None;
            let mut from = 0;
            while let Some(p) = stmt[from..].find("::") {
                let end = from + p;
                let seg: String = stmt[..end]
                    .chars()
                    .rev()
                    .take_while(|&c| is_word(c))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                if !seg.is_empty() {
                    last = Some(seg);
                }
                from = end + 2;
            }
            names.extend(last);
        }
    }
    let aliases = type_aliases(code);
    loop {
        let mut grew = false;
        for (alias, rhs) in &aliases {
            if !names.contains(alias)
                && (rhs.contains("symphonia") || names.iter().any(|n| uses_name(rhs, n)))
            {
                names.insert(alias.clone());
                grew = true;
            }
        }
        if !grew {
            return names;
        }
    }
}

/// Where the decoder lives.
const DECODE_DIR: &str = "crates/tutti-codec/src/decode";
const DECODE_FILE: &str = "crates/tutti-codec/src/decode.rs";

pub(super) fn check(tree: &dyn Tree, bad: &mut Vec<String>) {
    let mut decode: Vec<String> = vec![DECODE_FILE.to_owned()];
    decode.extend(
        tree.files(DECODE_DIR, &skipped)
            .into_iter()
            .filter(|p| p.ends_with(".rs")),
    );
    for top in ["crates", "testing"] {
        for path in tree.files(top, &skipped) {
            if !path.ends_with(".rs") || decode.contains(&path) {
                continue;
            }
            if ["/tests/", "/benches/", "/examples/"]
                .iter()
                .any(|d| path.contains(d))
            {
                continue;
            }
            let text = tree.read(&path).unwrap_or_default();
            if blank(&text).contains("symphonia") {
                bad.push(format!(
                    "{path} names symphonia outside tutti-codec's decode module"
                ));
            }
        }
    }
    decode.sort();
    for path in decode {
        if path.rsplit('/').next() == Some("tests.rs") {
            continue;
        }
        let Some(text) = tree.read(&path) else {
            continue;
        };
        let code = blank(&text);
        let names = names_in_file(&code, &path, bad);
        let mut spans = public_items(&code);
        spans.extend(pub_bodies(&code));
        for (start, end) in spans {
            let item = &code[start..end];
            if item.contains("symphonia") || names.iter().any(|n| uses_name(item, n)) {
                let shown: String = item.trim().chars().take(80).collect();
                bad.push(format!(
                    "{path}:{}: a pub item names symphonia (or a name imported from it): {shown}",
                    line_of(&code, start)
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const READ: &str = "crates/tutti-codec/src/decode/read.rs";

    fn run(files: &[(&str, &str)]) -> Vec<String> {
        let mut bad = Vec::new();
        check(&Mem::with(files), &mut bad);
        bad
    }

    fn one(src: &str) -> Vec<String> {
        run(&[(READ, src)])
    }

    #[test]
    fn private_use_of_symphonia_inside_the_decode_module_passes() {
        let src = "use symphonia::core::errors::Error;\nfn parse() -> Result<(), Error> { Ok(()) }\npub(super) fn f() -> Error { todo!() }\npub fn clean() -> u8 { 0 }\n// pub fn g() -> symphonia::X\n";
        assert!(one(src).is_empty(), "{:?}", one(src));
    }

    #[test]
    fn a_pub_item_naming_symphonia_directly_or_through_an_import_fails() {
        let direct = one("pub fn probe() -> symphonia::core::errors::Error { todo!() }\n");
        assert_eq!(direct.len(), 1);
        assert!(direct[0].starts_with(&format!("{READ}:1: a pub item names symphonia")));
        assert_eq!(
            one("use symphonia::core::errors::Error;\npub fn leak() -> Option<Error> { None }\n")
                .len(),
            1
        );
        // A rename counts as the new name only.
        let renamed = "use symphonia::core::errors::Error as Raw;\npub enum DecodeError {\n    Wrapped(Raw),\n}\n";
        assert_eq!(one(renamed).len(), 1);
        assert!(
            one("use symphonia::core::errors::Error as Raw;\npub fn f() -> Error { todo!() }\n")
                .is_empty()
        );
    }

    #[test]
    fn aliases_fields_impl_headers_and_globs_are_read() {
        assert_eq!(one("type Hidden = symphonia::core::errors::Error;\npub fn f() -> Option<Hidden> { None }\n").len(), 1);
        let chain = "use symphonia::core::errors::Error;\ntype Again = Error;\npub fn g() -> Option<Again> { None }\n";
        assert_eq!(one(chain).len(), 1);
        assert_eq!(one("use symphonia::core::formats::FormatReader;\npub struct S {\n    pub inner: Box<dyn FormatReader>,\n}\n").len(), 1);
        assert_eq!(one("use symphonia::core::errors::Error;\nimpl From<Error> for X { fn from(e: Error) -> X { X } }\n").len(), 1);
        assert_eq!(
            one("use symphonia::core::errors::*;\n")[0],
            format!("{READ}: a glob import from symphonia hides the names it brings in")
        );
        // A name behind `::` is another module's.
        assert!(
            one("use symphonia::core::errors::Error;\npub fn f() -> io::Error { todo!() }\n")
                .is_empty()
        );
    }

    #[test]
    fn symphonia_outside_the_decode_module_fails_except_in_tests_and_comments() {
        let other = "crates/tutti-engine/src/lib.rs";
        assert_eq!(
            run(&[(other, "use symphonia::core::errors;\n")]),
            [format!(
                "{other} names symphonia outside tutti-codec's decode module"
            )]
        );
        assert!(run(&[(other, "// symphonia\n")]).is_empty());
        assert!(run(&[("crates/tutti-engine/tests/oracle.rs", "use symphonia::x;\n")]).is_empty());
        assert!(run(&[("crates/tutti-codec/src/decode.rs", "use symphonia::x;\n")]).is_empty());
    }
}
