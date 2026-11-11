//! `check-session`: `tutti-session` is the Bevy-free API layer both umbrellas
//! use, and not an umbrella itself (design 015 section 8).
//!
//! Three assertions no compile can make:
//!
//! 1. **No Bevy crate in its normal dependency closure, with every feature on.**
//!    A `cargo tree`, not the workspace's metadata, which resolves features once
//!    for the whole workspace; `--all-features` because a Bevy crate behind an
//!    optional feature is absent from the default tree; `-e normal` keeps a
//!    dev-dependency walk out. (`bevy-tutti` is parked, so no Bevy crate is in the
//!    lock file at all; the guard still holds for an optional Bevy edge of an
//!    engine crate, and for the day the adapter comes back.)
//! 2. **No `bevy` feature** in the manifest of `tutti-session`, `tutti-graph`,
//!    `tutti-ump` or `tutti-codec` (CLAUDE.md, "Setup": they must never grow one).
//! 3. **No whole-crate re-export in `tutti-session`**: its source re-exports no
//!    crate or module whole (`pub use tutti_x as ..;`, `pub use tutti_x;`,
//!    `pub use tutti_x::{self ..}`, or a glob anywhere under a `tutti_` path),
//!    with or without attributes in front, across lines, in groups nested to any
//!    depth and in a root group. Single items are allowed: the types its own
//!    signatures take. `pub(crate) use` is not a re-export. Each file is
//!    lexed (comments and literals blanked) and each `pub use .. ;` read as a use
//!    tree.
//!
//! The adapter's own rules (design 015 section 8, checks 3 and 5: it names no
//! engine composition and holds no policy) are `scripts/parked/check-bevy-tutti.sh`,
//! which nothing runs while `bevy-tutti` is parked.

use super::lex::{blank, line_of};
use super::toml::Toml;
use super::{Cargo, Report, Tree};

const SESSION: &str = "tutti-session";
const SESSION_SRC: &str = "crates/tutti-session/src";
/// The crates that never grow a `bevy` feature.
const NO_BEVY_FEATURE: &[&str] = &["tutti-session", "tutti-graph", "tutti-ump", "tutti-codec"];

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git")
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `(::|\w+|[{},*])` tokens of a `use` body.
fn tokens(body: &str) -> Vec<&str> {
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b':' && b.get(i + 1) == Some(&b':') {
            out.push(&body[i..i + 2]);
            i += 2;
        } else if is_word(b[i]) {
            let s = i;
            while i < b.len() && is_word(b[i]) {
                i += 1;
            }
            out.push(&body[s..i]);
        } else if matches!(b[i], b'{' | b'}' | b',' | b'*') {
            out.push(&body[i..=i]);
            i += 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Parse one use tree from `t` at `*k` under `pre`, recording the whole-crate
/// leaves: a leaf starting at a `tutti_` crate that is the crate itself (with or
/// without `as NAME`), `self`, or a glob at any depth.
fn tree(t: &[&str], k: &mut usize, pre: &[String], hit: &mut Vec<String>) {
    let mut path = pre.to_vec();
    if t.get(*k) == Some(&"::") {
        *k += 1;
    }
    let from_tutti = |path: &[String]| path.first().is_some_and(|p| p.starts_with("tutti_"));
    while *k < t.len() {
        let tok = t[*k];
        if tok == "{" {
            *k += 1;
            while *k < t.len() && t[*k] != "}" {
                tree(t, k, &path, hit);
                if t.get(*k) == Some(&",") {
                    *k += 1;
                }
            }
            *k += 1;
            return;
        }
        if tok == "*" {
            *k += 1;
            if from_tutti(&path) {
                hit.push(format!("{}::*", path.join("::")));
            }
            return;
        }
        if tok.bytes().all(is_word) {
            *k += 1;
            if t.get(*k) == Some(&"::") {
                path.push(tok.to_owned());
                *k += 1;
                continue;
            }
            let full = if path.is_empty() {
                tok.to_owned()
            } else {
                format!("{}::{tok}", path.join("::"))
            };
            if t.get(*k) == Some(&"as") {
                *k += 2;
            }
            if tok == "self" {
                if from_tutti(&path) {
                    hit.push(full);
                }
            } else if path.is_empty() && tok.starts_with("tutti_") {
                hit.push(full);
            }
            return;
        }
        *k += 1;
        return;
    }
}

/// `pub use` statements of lexed `code`, as `(start, end of the ';')`.
fn pub_uses(code: &str) -> Vec<(usize, usize)> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    for (at, _) in code.match_indices("pub") {
        if at > 0 && (is_word(b[at - 1]) || b[at - 1] == b')') {
            continue;
        }
        let rest = &code[at + 3..];
        let after = rest.trim_start();
        if after.len() == rest.len() {
            continue;
        }
        let Some(kw) = after.strip_prefix("use") else {
            continue;
        };
        if kw.bytes().next().is_some_and(is_word) {
            continue;
        }
        let body = code.len() - kw.len();
        if let Some(semi) = code[body..].find(';') {
            out.push((at, body + semi));
        }
    }
    out
}

pub(super) fn check(tree_: &dyn Tree, cargo: &dyn Cargo) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let mut r = Report::default();

    // 1. The dependency closure.
    match cargo.tree(&[
        "-p",
        SESSION,
        "-e",
        "normal",
        "--all-features",
        "--prefix",
        "none",
    ]) {
        Err(e) => bad.push(format!("cargo tree -p {SESSION} did not run: {}", e.trim())),
        Ok(out) => {
            let mut crates: Vec<&str> = out
                .lines()
                .filter(|l| {
                    l.strip_prefix("bevy")
                        .is_some_and(|r| r.is_empty() || r.starts_with(['_', ' ']))
                })
                .collect();
            crates.sort_unstable();
            crates.dedup();
            for c in crates {
                bad.push(c.to_owned());
            }
            if !bad.is_empty() {
                bad.push(format!(
                    "a Bevy crate is in {SESSION}'s normal dependency closure (with --all-features); \
                     Bevy code goes in bevy-tutti, which depends on {SESSION}, never the reverse"
                ));
            }
        }
    }

    // 2. The manifests' features.
    for name in NO_BEVY_FEATURE {
        let path = format!("crates/{name}/Cargo.toml");
        let Some(text) = tree_.read(&path) else {
            bad.push(format!("{path} does not exist; was {name} moved?"));
            continue;
        };
        let has = Toml::parse(&text).section("features").is_some_and(|s| {
            s.entries
                .iter()
                .any(|(k, _)| k == "bevy" || k == "\"bevy\"")
        });
        if has {
            bad.push(format!(
                "{path} declares a `bevy` feature; {name} has none and must never have one (the adapter is bevy-tutti)"
            ));
        }
    }

    // 3. Whole-crate re-exports.
    let files: Vec<String> = tree_
        .files(SESSION_SRC, &skipped)
        .into_iter()
        .filter(|p| p.ends_with(".rs"))
        .collect();
    if files.is_empty() {
        bad.push(format!(
            "{SESSION_SRC} holds no .rs file; was the crate moved?"
        ));
    }
    for path in &files {
        let src = tree_.read(path).unwrap_or_default();
        let code = blank(&src);
        for (at, semi) in pub_uses(&code) {
            let body = code[at + 3..semi].trim_start();
            let body = &body["use".len()..];
            let t = tokens(body);
            let (mut k, mut hit) = (0, Vec::new());
            tree(&t, &mut k, &[], &mut hit);
            if hit.is_empty() {
                continue;
            }
            let text = src[at..=semi]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            bad.push(format!(
                "{path}:{}: {text}\n    whole: {} (tutti-session is not an umbrella: the umbrella is `tutti`; \
                 re-export the single items its own signatures take)",
                line_of(&code, at),
                hit.join(", ")
            ));
        }
    }

    if bad.is_empty() {
        r.say(format!(
            "check-session: ok ({SESSION} is Bevy-free and re-exports no crate whole; {} files)",
            files.len()
        ));
    } else {
        r.fail("check-session: FAIL");
        for b in bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::{Mem, TreeText};
    use super::*;

    const LIB: &str = "crates/tutti-session/src/lib.rs";

    fn clean() -> Mem {
        let mut m = Mem::with(&[(
            LIB,
            "pub use tutti_graph::Editor;\npub(crate) use tutti_x;\n",
        )]);
        for c in NO_BEVY_FEATURE {
            m.files.insert(
                format!("crates/{c}/Cargo.toml"),
                "[features]\ndefault = []\n".into(),
            );
        }
        m
    }

    fn lib(src: &str) -> Report {
        let mut m = clean();
        m.files.insert(LIB.into(), src.into());
        check(
            &m,
            &TreeText("tutti-session v0\ntutti-graph v0\nbevytypo v0\n"),
        )
    }

    #[test]
    fn a_clean_session_passes() {
        let r = check(&clean(), &TreeText("tutti-session v0.1\n"));
        assert!(r.passed(), "{:?}", r.findings);
        assert_eq!(
            r.out[0],
            "check-session: ok (tutti-session is Bevy-free and re-exports no crate whole; 1 files)"
        );
    }

    #[test]
    fn a_bevy_crate_in_the_closure_fails_and_a_lookalike_does_not() {
        let r = check(
            &clean(),
            &TreeText("tutti-session v0\nbevy_ecs v0.19\nbevy v0\nbevy_ecs v0.19\n"),
        );
        assert_eq!(r.findings[1], "  bevy v0");
        assert_eq!(r.findings[2], "  bevy_ecs v0.19");
        assert!(r.findings[3].contains("normal dependency closure"));
        assert!(check(&clean(), &TreeText("tutti-bevy v0\nbevytypo v0\n")).passed());
        let r = check(&clean(), &crate::gates::fake::NoCargo);
        assert!(r.findings[1].contains("cargo tree -p tutti-session did not run"));
    }

    #[test]
    fn a_bevy_feature_fails_in_the_four_crates() {
        let mut m = clean();
        m.files.insert(
            "crates/tutti-ump/Cargo.toml".into(),
            "[features]\nbevy = [\"dep:bevy_reflect\"]\n".into(),
        );
        let r = check(&m, &TreeText(""));
        assert!(r.findings[1].contains("crates/tutti-ump/Cargo.toml declares a `bevy` feature"));
        // A dependency table entry named bevy is not a feature.
        m.files.insert(
            "crates/tutti-ump/Cargo.toml".into(),
            "[dependencies]\nbevy = \"1\"\n".into(),
        );
        assert!(check(&m, &TreeText("")).passed());
    }

    #[test]
    fn whole_crate_reexports_in_every_spelling_fail() {
        for src in [
            "pub use tutti_modulation as modulation;\n",
            "pub use tutti_modulation;\n",
            "pub use tutti_modulation::*;\n",
            "pub use tutti_engine::prelude::*;\n",
            "#[cfg(feature = \"modulation\")] pub use tutti_modulation as modulation;\n",
            "pub use tutti_modulation::{self as modulation};\n",
            "pub use tutti_modulation::{a::{B}, self};\n",
            "pub use {tutti_modulation as m};\n",
            "pub use ::tutti_modulation::{x::{y::{*}}};\n",
            "pub use tutti_modulation::{\n    a::{B, C},\n    self as m,\n};\n",
        ] {
            let r = lib(src);
            assert!(!r.passed(), "{src}");
            assert!(r.findings[1].contains("whole: "), "{src}");
        }
    }

    #[test]
    fn single_items_crate_private_uses_comments_and_strings_pass() {
        for src in [
            "pub use tutti_modulation::{a::{B}, C};\n",
            "pub use {tutti_modulation::A as M};\n",
            "pub(crate) use tutti_modulation;\n",
            "// pub use tutti_modulation;\nconst S: &str = \"pub use tutti_modulation;\";\n",
            "pub use std::*;\n",
        ] {
            assert!(lib(src).passed(), "{src}");
        }
    }

    #[test]
    fn the_report_names_file_line_and_statement() {
        let r = lib("\n\npub use tutti_modulation\n    as modulation;\n");
        assert!(r.findings[1].starts_with(&format!(
            "  {LIB}:3: pub use tutti_modulation as modulation;\n    whole: tutti_modulation"
        )));
    }

    #[test]
    fn a_session_with_no_source_fails() {
        let mut m = clean();
        m.files.remove(LIB);
        assert!(check(&m, &TreeText("")).findings[1].contains("holds no .rs file"));
    }
}
