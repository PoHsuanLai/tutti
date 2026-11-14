//! `check-buffers`: a public item names audio with a fixed sample format only
//! as a row of [`LIST`].
//!
//! Audio in a public API is generic over `tutti_buffers::Sample` (or `Float`).
//! The gate lists every public site that is not, so the migration works
//! through the list row by row.
//!
//! **Where it reads.** Every `crates/*/src/**/*.rs`, comments and literals
//! blanked, except the `*-ffi` crates, `tests`, `benches` and `examples`
//! directories, `tests.rs` files and `#[cfg(test)]` items.
//!
//! **What a site is.** A public item whose text names, in its signature (up to
//! the `{` or `;`), a field's type, an enum variant's payload or a type
//! alias's right side:
//!
//! - a raw audio container: `[f32]`, `[f64]`, `Vec<f32>` or `Vec<f64>` (this
//!   covers `&mut [f32]`, `Vec<Vec<f32>>`, `Box<[f32]>` and `&[&[f32]]`);
//! - a `tutti_buffers` view (`Interleaved`, `InterleavedMut`, `Planes`,
//!   `PlanesMut`, `Mono`, `MonoMut`, `StereoPlanes`) or `Wave` whose last
//!   generic argument is `f32` or `f64`;
//! - one of those views with no sample argument (`Planes<'_>`, or a bare name
//!   not behind a path other than `tutti_buffers`), which relies on the `f32`
//!   default, inside an item whose own text and enclosing `impl`, `trait`,
//!   `struct` or `enum` header name neither `Sample` nor `Float`.
//!
//! The public items read are `pub fn`, `pub type`, the `pub` fields of a `pub
//! struct`, every variant payload of a `pub enum`, and the methods of a `pub
//! trait`. A `pub(crate)`, `pub(super)` or `pub(in ..)` item is not public.
//! A trait impl's methods are the trait's.
//!
//! **Keys.** `<file>::<item>`: a free item is `crates/a/src/x.rs::name`, a
//! method or trait method is `crates/a/src/x.rs::Owner::name` (`Owner` is the
//! last path segment of the innermost enclosing `impl`'s type, or the trait's
//! name), and a field or variant is `crates/a/src/x.rs::Type.member` (a tuple
//! field's member is its index). No line numbers. Items that share a key (an
//! `impl Foo<f32>` and an `impl Foo<f64>` each with `new`) share one row.
//!
//! **List.** [`LIST`] holds `<key>  <reason>` lines, `#` lines and blank lines
//! aside. The reason is `ffi`, `engine-f32`, `pending` or `measured: <why>`.
//! The gate fails on a site with no row, a row with no site, a row whose
//! reason is unknown or empty, and a key listed twice.
//!
//! **Blind spots.** The scan is lexical: it cannot see a type alias or
//! associated type that hides `f32`, a `pub const`/`static` holding samples, a
//! macro-generated item, a re-export, whether a `pub` item is reachable
//! from the crate root, or a raw slice that is not audio (a coefficient
//! table is a site too and is judged `measured`).

use super::lex::{blank, find_word_from, matching};
use super::value_traits::test_ranges;
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// The list of fixed sites, `<key>  <reason>` per line.
pub(crate) const LIST: &str = "xtask/buffers-fixed.txt";

/// The fewest sites a real run finds; fewer means the walk is broken.
pub(super) const MIN_SITES: usize = 50;

const VIEWS: [&str; 7] = [
    "Interleaved",
    "InterleavedMut",
    "Planes",
    "PlanesMut",
    "Mono",
    "MonoMut",
    "StereoPlanes",
];

/// Owned audio buffers that carry no sample parameter yet; only an explicit
/// `f32`/`f64` argument names them fixed.
const OWNED: [&str; 1] = ["Wave"];

const RAW: [&str; 4] = ["[f32]", "[f64]", "Vec<f32>", "Vec<f64>"];

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git" | "tests" | "benches" | "examples")
}

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// A site: its key and what it names.
#[derive(Debug, PartialEq, Eq)]
struct Site {
    key: String,
    why: String,
}

/// Collapses whitespace: a run between two word characters is one space, any
/// other run vanishes (`& mut [ f32 ]` is `&mut [f32]`).
fn squash(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending = true;
            continue;
        }
        let last_word = out.as_bytes().last().is_some_and(|&b| word(b));
        if pending && last_word && (c.is_ascii_alphanumeric() || c == '_') {
            out.push(' ');
        }
        pending = false;
        out.push(c);
    }
    out
}

/// The top-level (outside `()`, `[]`, `{}` and `<>`) comma-separated parts.
fn split_top(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let (mut depth, mut from) = (0i32, 0);
    let mut parts = Vec::new();
    for i in 0..b.len() {
        match b[i] {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'>' if i == 0 || b[i - 1] != b'-' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(&text[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[from..]);
    parts
}

/// What `text` names that is fixed: a raw container, a view with `f32` or
/// `f64` spelled, or (when `generic` is false) a view that takes the default.
fn fixed(text: &str, generic: bool) -> Option<String> {
    let t = squash(text);
    if let Some(raw) = RAW.iter().find(|r| t.contains(**r)) {
        return Some((*raw).to_owned());
    }
    let b = t.as_bytes();
    for (names, owned) in [(&VIEWS[..], false), (&OWNED[..], true)] {
        for name in names {
            let mut from = 0;
            while let Some(at) = find_word_from(&t, name, from) {
                let end = at + name.len();
                from = end;
                let path = t[..at].strip_suffix("::");
                if path.is_some_and(|p| !p.ends_with("tutti_buffers")) {
                    continue;
                }
                let args = (b.get(end) == Some(&b'<'))
                    .then(|| matching(&t, end, b'<', b'>'))
                    .flatten()
                    .map(|close| &t[end + 1..close]);
                let last = args.and_then(|a| split_top(a).last().copied());
                match last {
                    Some("f32" | "f64") => return Some(format!("{name}<_, {}>", last.unwrap())),
                    Some(a) if !a.starts_with('\'') => {}
                    _ if owned || generic => {}
                    _ => return Some(format!("{name} with the default sample")),
                }
            }
        }
    }
    None
}

/// An `impl` or `trait` body: where it is, what owns its items.
struct Block {
    open: usize,
    close: usize,
    owner: String,
    generic: bool,
    /// A `pub trait`: its methods are public.
    public_trait: bool,
}

fn names_sample(text: &str) -> bool {
    let t = squash(text);
    find_word_from(&t, "Sample", 0).is_some() || find_word_from(&t, "Float", 0).is_some()
}

/// The last path segment of the type an `impl` header is for.
fn impl_owner(header: &str) -> String {
    let mut h = header.trim();
    if h.starts_with('<')
        && let Some(close) = matching(h, 0, b'<', b'>')
    {
        h = h[close + 1..].trim();
    }
    if let Some(w) = find_word_from(h, "where", 0) {
        h = h[..w].trim();
    }
    if let Some(f) = find_word_from(h, "for", 0) {
        h = h[f + 3..].trim();
    }
    let h = h.trim_start_matches(['&', '!']);
    let h = h.strip_prefix("mut ").unwrap_or(h).trim();
    let h = h.strip_prefix("dyn ").unwrap_or(h).trim();
    let path: String = h
        .chars()
        .take_while(|&c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
        .collect();
    match path.rsplit("::").next() {
        Some(last) if !last.is_empty() => last.to_owned(),
        _ => "impl".to_owned(),
    }
}

/// The text before `at`, trimmed.
fn before(code: &str, at: usize) -> &str {
    code[..at].trim_end()
}

/// The `impl` and `trait` bodies of a file.
fn blocks(code: &str) -> Vec<Block> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    for kw in ["impl", "trait"] {
        let mut from = 0;
        while let Some(at) = find_word_from(code, kw, from) {
            from = at + kw.len();
            let head = before(code, at);
            let head_ok = head.is_empty()
                || head.ends_with(['}', ';', '{', ']'])
                || head.ends_with("unsafe")
                || head.ends_with("default")
                || head.ends_with("auto")
                || head.ends_with("pub");
            if !head_ok {
                continue;
            }
            let mut depth = 0i32;
            let mut open = None;
            for (i, &c) in b.iter().enumerate().skip(from) {
                match c {
                    b'(' | b'[' => depth += 1,
                    b')' | b']' => depth -= 1,
                    b';' if depth == 0 => break,
                    b'{' if depth == 0 => {
                        open = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            let Some(open) = open else { continue };
            let Some(close) = matching(code, open, b'{', b'}') else {
                continue;
            };
            let header = &code[from..open];
            let owner = if kw == "impl" {
                impl_owner(header)
            } else {
                header
                    .trim()
                    .chars()
                    .take_while(|&c| c.is_ascii_alphanumeric() || c == '_')
                    .collect()
            };
            let public_trait = kw == "trait"
                && (head.ends_with("pub") || head.ends_with("pub unsafe"))
                && !head.ends_with(')');
            out.push(Block {
                open,
                close,
                owner,
                generic: names_sample(header),
                public_trait,
            });
        }
    }
    out
}

/// The end of a signature or alias: the first `{` or `;` outside `()` and `[]`.
fn item_end(code: &str, from: usize) -> usize {
    let mut depth = 0i32;
    for (i, &c) in code.as_bytes().iter().enumerate().skip(from) {
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'{' | b';' if depth == 0 => return i,
            _ => {}
        }
    }
    code.len()
}

/// The end of a struct or enum header: the first `{`, `(` or `;` outside `<>`.
fn header_end(code: &str, from: usize) -> usize {
    let b = code.as_bytes();
    let mut depth = 0i32;
    for i in from..b.len() {
        match b[i] {
            b'<' => depth += 1,
            b'>' if b[i - 1] != b'-' => depth -= 1,
            b'{' | b'(' | b';' if depth == 0 => return i,
            _ => {}
        }
    }
    b.len()
}

fn ident_at(code: &str, from: usize) -> (&str, usize) {
    let rest = &code[from..];
    let start = from + (rest.len() - rest.trim_start().len());
    let len = code[start..].bytes().take_while(|&c| word(c)).count();
    (&code[start..start + len], start + len)
}

/// `text` without its leading `#[..]` attributes.
fn strip_attrs(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        if !text.starts_with("#[") {
            return text;
        }
        match matching(text, 1, b'[', b']') {
            Some(close) => text = &text[close + 1..],
            None => return text,
        }
    }
}

/// The public sites of one file's text.
fn scan(path: &str, src: &str) -> Vec<Site> {
    let code = blank(src);
    let tests = test_ranges(&code);
    let in_test = |p: usize| tests.iter().any(|&(a, z)| a <= p && p <= z);
    let blocks = blocks(&code);
    let owner_of = |p: usize| {
        blocks
            .iter()
            .filter(|b| b.open < p && p < b.close)
            .max_by_key(|b| b.open)
    };
    let mut sites = Vec::new();
    let mut add = |key: String, why: Option<String>| {
        if let Some(why) = why {
            sites.push(Site {
                key: format!("{path}::{key}"),
                why,
            });
        }
    };
    let fn_site = |p: usize, name_from: usize| {
        let (name, _) = ident_at(&code, name_from);
        let end = item_end(&code, name_from);
        let owner = owner_of(p);
        let generic = names_sample(&code[p..end]) || owner.is_some_and(|b| b.generic);
        let key = match owner {
            Some(b) => format!("{}::{name}", b.owner),
            None => name.to_owned(),
        };
        (key, fixed(&code[p..end], generic), end)
    };

    let mut from = 0;
    while let Some(p) = find_word_from(&code, "pub", from) {
        from = p + 3;
        if in_test(p) || code[from..].trim_start().starts_with('(') {
            continue;
        }
        let mut at = from;
        let kind = loop {
            let (w, end) = ident_at(&code, at);
            match w {
                "const" | "async" | "unsafe" | "default" => at = end,
                "extern" => {
                    // The ABI string is blanked to its quotes.
                    let rest = code[end..].trim_start();
                    at = end;
                    if rest.starts_with('"') {
                        let open = code[end..].find('"').map_or(end, |q| end + q);
                        at = code[open + 1..].find('"').map_or(open, |q| open + 1 + q) + 1;
                    }
                }
                _ => break (w, end),
            }
        };
        let (w, end) = kind;
        match w {
            "fn" => {
                let (key, why, stop) = fn_site(p, end);
                add(key, why);
                from = stop;
            }
            "type" => {
                let (name, _) = ident_at(&code, end);
                let stop = item_end(&code, end);
                let generic = names_sample(&code[p..stop]);
                add(name.to_owned(), fixed(&code[p..stop], generic));
                from = stop;
            }
            "struct" | "enum" => {
                let (name, name_end) = ident_at(&code, end);
                let hend = header_end(&code, name_end);
                let generic = names_sample(&code[end..hend]);
                let is_enum = w == "enum";
                let (o, c) = match code.as_bytes().get(hend) {
                    Some(b'{') => (b'{', b'}'),
                    Some(b'(') => (b'(', b')'),
                    _ => continue,
                };
                let Some(close) = matching(&code, hend, o, c) else {
                    continue;
                };
                for (i, seg) in split_top(&code[hend + 1..close]).iter().enumerate() {
                    let seg = strip_attrs(seg);
                    if is_enum {
                        let (var, vend) = ident_at(seg, 0);
                        if !var.is_empty() {
                            add(format!("{name}.{var}"), fixed(&seg[vend..], generic));
                        }
                        continue;
                    }
                    let Some(rest) = seg.strip_prefix("pub") else {
                        continue;
                    };
                    if rest.trim_start().starts_with('(')
                        || rest.starts_with(|c: char| word(c as u8))
                    {
                        continue;
                    }
                    let (member, ty) = if o == b'{' {
                        match rest.split_once(':') {
                            Some((m, t)) => (m.trim().to_owned(), t),
                            None => continue,
                        }
                    } else {
                        (i.to_string(), rest)
                    };
                    add(format!("{name}.{member}"), fixed(ty, generic));
                }
                from = close;
            }
            _ => {}
        }
    }

    for b in blocks.iter().filter(|b| b.public_trait && !in_test(b.open)) {
        let mut depth = 0i32;
        let mut i = b.open + 1;
        while i < b.close {
            match code.as_bytes()[i] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                b'f' if depth == 0 => {
                    let (w, end) = ident_at(&code, i);
                    let named = !ident_at(&code, end).0.is_empty();
                    let starts = i == 0 || !word(code.as_bytes()[i - 1]);
                    if w == "fn" && named && starts {
                        let (name, _) = ident_at(&code, end);
                        let stop = item_end(&code, end);
                        let generic = names_sample(&code[i..stop]) || b.generic;
                        add(
                            format!("{}::{name}", b.owner),
                            fixed(&code[i..stop], generic),
                        );
                        i = stop;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
    sites
}

/// The files the gate reads.
fn source_files(tree: &dyn Tree) -> Vec<String> {
    tree.files("crates", &skipped)
        .into_iter()
        .filter(|p| {
            let parts: Vec<&str> = p.split('/').collect();
            p.ends_with(".rs")
                && parts.get(2) == Some(&"src")
                && !parts[1].ends_with("-ffi")
                && parts.last() != Some(&"tests.rs")
                && !parts.contains(&"tests")
        })
        .collect()
}

const REASONS: &str = "ffi, engine-f32, pending, or `measured: <why>`";

/// The list's rows, key to reason; the errors name the line.
fn parse(text: &str) -> Result<BTreeMap<String, String>, Vec<String>> {
    let mut rows = BTreeMap::new();
    let mut errors = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = n + 1;
        let (key, reason) = match line.split_once(char::is_whitespace) {
            Some((k, r)) => (k, r.trim()),
            None => (line, ""),
        };
        let known = matches!(reason, "ffi" | "engine-f32" | "pending")
            || reason
                .strip_prefix("measured:")
                .is_some_and(|why| !why.trim().is_empty());
        if !known {
            errors.push(format!(
                "  {LIST}:{at}: `{key}` has reason `{reason}`; a reason is {REASONS}"
            ));
        }
        if rows.insert(key.to_owned(), reason.to_owned()).is_some() {
            errors.push(format!("  {LIST}:{at}: `{key}` is listed twice"));
        }
    }
    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

pub(super) fn check(tree: &dyn Tree, min_sites: usize) -> Report {
    let mut r = Report::default();
    let Some(text) = tree.read(LIST) else {
        r.fail(format!("check-buffers: FAIL: {LIST} is missing"));
        return r;
    };
    let rows = match parse(&text) {
        Ok(rows) => rows,
        Err(errors) => {
            r.fail(format!("check-buffers: FAIL ({LIST} is malformed):"));
            errors.into_iter().for_each(|e| r.fail(e));
            return r;
        }
    };
    let mut sites: BTreeMap<String, String> = BTreeMap::new();
    for path in source_files(tree) {
        if let Some(src) = tree.read(&path) {
            for s in scan(&path, &src) {
                sites.entry(s.key).or_insert(s.why);
            }
        }
    }
    if sites.len() < min_sites {
        r.fail(format!(
            "check-buffers: FAIL: {} sites found, at least {min_sites} expected; the walk is broken",
            sites.len()
        ));
        return r;
    }
    let unlisted: Vec<_> = sites.keys().filter(|k| !rows.contains_key(*k)).collect();
    let stale: BTreeSet<_> = rows.keys().filter(|k| !sites.contains_key(*k)).collect();
    if unlisted.is_empty() && stale.is_empty() {
        r.say("check-buffers: OK");
    } else {
        if !unlisted.is_empty() {
            r.fail(format!(
                "check-buffers: FAIL (a public item names audio with a fixed sample format; \
                 make it generic over Sample or Float, or add `<key>  <reason>` to {LIST}):"
            ));
            for k in unlisted {
                r.fail(format!("  {k}: names {}", sites[k]));
            }
        }
        if !stale.is_empty() {
            r.fail(format!(
                "check-buffers: FAIL (a row of {LIST} names no site; delete it):"
            ));
            for k in stale {
                r.fail(format!("  {k}"));
            }
        }
    }
    let mut by_reason: BTreeMap<&str, usize> = BTreeMap::new();
    for reason in rows.values() {
        let class = reason.split(':').next().unwrap_or(reason);
        *by_reason.entry(class).or_default() += 1;
    }
    let counts: Vec<String> = by_reason.iter().map(|(k, n)| format!("{n} {k}")).collect();
    r.say(format!(
        "check-buffers: {} sites, {} rows in {LIST} ({})",
        sites.len(),
        rows.len(),
        counts.join(", ")
    ));
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const F: &str = "crates/a/src/lib.rs";

    fn run(list: &str, src: &str) -> Report {
        check(&Mem::with(&[(LIST, list), (F, src)]), 0)
    }

    fn keys(src: &str) -> Vec<String> {
        scan(F, src).into_iter().map(|s| s.key).collect()
    }

    #[test]
    fn a_site_with_no_row_fails_and_names_what_it_holds() {
        let r = run("", "pub fn gain(x: &mut [f32]) {}\n");
        assert_eq!(
            r.findings[1..],
            ["  crates/a/src/lib.rs::gain: names [f32]"]
        );
    }

    #[test]
    fn a_listed_site_passes_with_its_count() {
        let r = run(
            "crates/a/src/lib.rs::gain  pending\n",
            "pub fn gain(x: &mut [f32]) {}\n",
        );
        assert!(r.passed());
        assert_eq!(
            r.out[1],
            "check-buffers: 1 sites, 1 rows in xtask/buffers-fixed.txt (1 pending)"
        );
    }

    #[test]
    fn a_row_with_no_site_is_stale_and_fails() {
        let r = run("crates/a/src/lib.rs::gone  pending\n", "pub fn f() {}\n");
        assert!(!r.passed());
        assert_eq!(r.findings[1..], ["  crates/a/src/lib.rs::gone"]);
    }

    #[test]
    fn an_unknown_empty_or_unreasoned_measured_reason_fails() {
        for reason in ["", "later", "measured:", "measured:   "] {
            let list = format!("crates/a/src/lib.rs::gain  {reason}\n");
            let r = run(&list, "pub fn gain(x: &[f32]) {}\n");
            assert!(!r.passed(), "reason {reason:?}");
            assert!(r.findings[0].contains("malformed"));
        }
        let ok = run(
            "crates/a/src/lib.rs::gain  measured: the spec fixes f32\n",
            "pub fn gain(x: &[f32]) {}\n",
        );
        assert!(ok.passed());
    }

    #[test]
    fn a_row_listed_twice_fails() {
        let r = run(
            "crates/a/src/lib.rs::g  ffi\ncrates/a/src/lib.rs::g  pending\n",
            "pub fn g(x: &[f32]) {}\n",
        );
        assert!(r.findings[1].ends_with("is listed twice"));
    }

    #[test]
    fn a_missing_list_fails() {
        assert!(!check(&Mem::with(&[(F, "pub fn f() {}\n")]), 0).passed());
    }

    #[test]
    fn a_walk_that_finds_too_few_sites_fails() {
        let r = check(&Mem::with(&[(LIST, ""), (F, "pub fn f() {}\n")]), 1);
        assert!(r.findings[0].contains("the walk is broken"));
    }

    #[test]
    fn a_generic_item_is_not_a_site() {
        let src = "pub fn f<S: Sample>(x: Interleaved<'_, S>) {}\n\
                   pub fn g<S: Float>(x: PlanesMut<'_>) {}\n\
                   pub fn h<S>(x: Mono<'_>) where S: tutti_buffers::Sample {}\n\
                   impl<S: Sample> Foo<S> { pub fn k(&self, x: Planes<'_>) {} }\n\
                   pub struct W<'a, S: Sample> { pub p: Planes<'a> }\n";
        assert!(keys(src).is_empty());
    }

    #[test]
    fn a_view_with_the_sample_spelled_f32_or_f64_is_a_site_even_when_generic() {
        let src = "pub fn a(x: Interleaved<'_, f32>) {}\n\
                   pub fn b<S: Sample>(x: Mono<'a, f64>) {}\n\
                   pub fn c(x: tutti_buffers::Planes<'_, f32>) {}\n\
                   pub fn d() -> Wave<f32> { todo!() }\n";
        assert_eq!(keys(src), ["a", "b", "c", "d"].map(|n| format!("{F}::{n}")));
    }

    #[test]
    fn a_view_with_no_sample_argument_in_a_plain_item_is_a_site() {
        let src = "pub fn a(x: Interleaved<'_>) {}\n\
                   pub fn b(x: &mut StereoPlanes<'_>) {}\n\
                   pub fn c(x: Mono) {}\n\
                   pub fn d(x: Wave) {}\n\
                   pub fn e(x: Planes<'_, S>) {}\n\
                   pub fn f(x: ChannelLayout::Mono) {}\n";
        assert_eq!(keys(src), ["a", "b", "c"].map(|n| format!("{F}::{n}")));
    }

    #[test]
    fn every_raw_audio_container_is_a_site() {
        let src = "pub fn a(x: &[f32]) {}\npub fn b(x: &mut [f64]) {}\n\
                   pub fn c() -> Vec<f32> {}\npub fn d(x: Vec<Vec<f64>>) {}\n\
                   pub fn e(x: &[&[f32]]) {}\npub fn f(x: &mut [&mut [f32]]) {}\n\
                   pub fn g(x: Box<[f32]>) {}\npub fn h(x: &[ f32 ]) {}\n\
                   pub fn i(x: &[u8], y: [f32; 4]) {}\n";
        assert_eq!(
            keys(src),
            ["a", "b", "c", "d", "e", "f", "g", "h"].map(|n| format!("{F}::{n}"))
        );
    }

    #[test]
    fn a_non_public_item_is_not_a_site() {
        let src = "pub(crate) fn a(x: &[f32]) {}\npub(super) fn b(x: &[f32]) {}\n\
                   fn c(x: &[f32]) {}\npub(in crate::m) fn d(x: &[f32]) {}\n\
                   pub struct S { pub(crate) x: Vec<f32>, y: Vec<f32> }\n\
                   pub(crate) trait T { fn t(&self, x: &[f32]); }\n\
                   impl Tr for S { fn p(&self, x: &[f32]) {} }\n";
        assert!(keys(src).is_empty());
    }

    #[test]
    fn a_test_module_and_a_test_file_are_skipped() {
        let mem = Mem::with(&[
            (LIST, ""),
            (
                F,
                "#[cfg(test)]\nmod tests { pub fn t(x: &[f32]) {} }\n#[cfg(test)]\nuse x::*;\n",
            ),
            ("crates/a/src/tests.rs", "pub fn t(x: &[f32]) {}\n"),
            ("crates/a/src/m/tests/x.rs", "pub fn t(x: &[f32]) {}\n"),
            ("crates/a/tests/x.rs", "pub fn t(x: &[f32]) {}\n"),
            ("crates/a/benches/x.rs", "pub fn t(x: &[f32]) {}\n"),
            ("crates/a-ffi/src/lib.rs", "pub fn t(x: &[f32]) {}\n"),
            ("testing/b/src/lib.rs", "pub fn t(x: &[f32]) {}\n"),
        ]);
        assert!(check(&mem, 0).passed());
    }

    #[test]
    fn a_signature_over_several_lines_is_one_site() {
        let src = "pub fn process(\n    &mut self,\n    // the block\n    input: &[f32],\n    out: &mut [f32],\n) -> usize {\n    0\n}\n";
        assert_eq!(keys(src), [format!("{F}::process")]);
    }

    #[test]
    fn a_word_in_a_comment_or_string_is_not_a_site() {
        let src = "/// Takes `&[f32]`.\npub fn f() { let s = \"pub fn g(x: &[f32]) {}\"; }\n";
        assert!(keys(src).is_empty());
    }

    #[test]
    fn a_method_is_keyed_by_its_impl_type_and_a_trait_method_by_its_trait() {
        let src = "impl<T> Engine<T> { pub fn process(&self, x: &[f32]) {} }\n\
                   impl Source for Engine { fn pull(&mut self, x: &mut [f32]) {} }\n\
                   impl dyn Node { pub fn raw(&self, x: &[f32]) {} }\n\
                   pub trait Sink {\n    fn push(&mut self, x: &[f32]);\n    fn ok(&self);\n    \
                   fn dflt(&self, x: &[f32]) { let f: fn(f32) = g; }\n}\n";
        assert_eq!(
            keys(src),
            ["Engine::process", "Node::raw", "Sink::push", "Sink::dflt"]
                .map(|n| format!("{F}::{n}"))
        );
    }

    #[test]
    fn a_public_field_variant_and_alias_are_sites_and_a_private_one_is_not() {
        let src = "pub struct S { pub a: Vec<f32>, b: Vec<f32>, pub c: u8 }\n\
                   pub struct T(pub Vec<f64>, Vec<f64>);\n\
                   pub enum E { A(Vec<f32>), B { x: &'static [f64] }, C }\n\
                   pub type P = Vec<Vec<f32>>;\n\
                   pub type Q = u8;\n";
        assert_eq!(
            keys(src),
            ["S.a", "T.0", "E.A", "E.B", "P"].map(|n| format!("{F}::{n}"))
        );
    }

    #[test]
    fn items_that_share_a_key_share_one_row() {
        let src = "impl Foo<f32> { pub fn new(x: &[f32]) {} }\nimpl Foo<f64> { pub fn new(x: &[f64]) {} }\n";
        let r = run("crates/a/src/lib.rs::Foo::new  pending\n", src);
        assert!(r.passed());
    }
}
