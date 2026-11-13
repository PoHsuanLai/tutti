//! `check-passive-tests`: a `#[test]` asserts something.
//!
//! A test that only reaches its last line proves that nothing panicked,
//! which the policy deletes. The gate reads every `.rs` file under `crates/`,
//! `testing/` and `xtask/` with its comments and literals blanked, and
//! finds each `fn` whose attributes include `#[test]` and none include
//! `#[should_panic]`. Its body is *active* when it holds any of:
//!
//! - a call or macro whose name contains `assert` (`assert_eq!`,
//!   `debug_assert!`, `assert_golden(..)`), or `panic!` or `unreachable!`;
//! - a call of `unwrap`, `expect`, `unwrap_err` or `expect_err`, or a `?`: the
//!   test fails when the value is not the one it names;
//! - a call of a function or macro of the same file whose body is active (at
//!   any depth);
//! - a call of a helper that [`HELPERS`] names, the cross-file ones.
//!
//! A test named by a macro placeholder (`fn $case()`) is not read: the
//! macro's argument names the case it runs. A call of a placeholder
//! (`super::$case()`) is active, for the same reason.
//!
//! `matches!` alone is not an assertion: it computes a `bool`.
//!
//! Each test that is none of these is a site. [`BASELINE`] holds the count of
//! sites each file had when the gate began; a file fails when it holds more
//! than its baseline, and the report says how far the baseline can be lowered.
//!
//! Limits: a helper is matched by its name, so a same-file function of the
//! name of an active one makes every call of that name active.

use super::lex::{blank, line_of, matching};
use super::prose::word;
use super::ratchet::{self, Found, Wording};
use super::{Report, Tree};
use std::collections::BTreeSet;

/// `<count> <path>` lines: each file's passive tests when the gate began.
pub(crate) const BASELINE: &str = "xtask/passive-tests.txt";

const WORDING: Wording = Wording {
    gate: "check-passive-tests",
    baseline: BASELINE,
    noun: "passive tests",
    headline: "a test asserts nothing; pin a property or delete it",
};

const ROOTS: [&str; 3] = ["crates", "testing", "xtask"];

/// Helpers defined in another file that assert, by name; a trailing `*`
/// matches any suffix. Each has the reason it is trusted.
pub(super) const HELPERS: &[(&str, &str)] = &[
    (
        "check_*",
        "the fixtures' contract checkers (`check_range_contract`) and a suite's `check_*` helpers assert",
    ),
    (
        "expect_*",
        "an `expect_*` helper fails the test on a value it does not expect",
    ),
    (
        "check",
        "a `.check()` builder runs the assertions of the row it was built for",
    ),
    (
        "verify_*",
        "a `verify_*` helper fails the test on a value it does not accept",
    ),
];

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git" | "node_modules")
}

fn helper(name: &str) -> bool {
    HELPERS.iter().any(|(pat, _)| match pat.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == *pat,
    })
}

/// A function or macro definition: its name and its body's byte range.
struct Def {
    name: String,
    body: (usize, usize),
}

fn ident_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && word(b[i]) {
        i += 1;
    }
    i
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// The `fn` and `macro_rules!` definitions with a body, and the `#[test]`
/// functions among them (index into the definitions) that are not
/// `#[should_panic]`.
fn definitions(code: &str) -> (Vec<Def>, Vec<(usize, usize)>) {
    let b = code.as_bytes();
    let mut defs: Vec<Def> = Vec::new();
    let mut tests = Vec::new();
    let mut attrs = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'#' && b.get(i + 1) == Some(&b'[') {
            let Some(close) = matching(code, i + 1, b'[', b']') else {
                break;
            };
            attrs.push_str(&code[i..=close]);
            i = close + 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if word(c) {
            let end = ident_end(b, i);
            let tok = &code[i..end];
            let preceded = i > 0 && (b[i - 1] == b'.' || b[i - 1] == b':');
            let is_def = !preceded && (tok == "fn" || tok == "macro_rules");
            if is_def {
                let mut s = skip_space(b, end);
                if tok == "macro_rules" {
                    if b.get(s) != Some(&b'!') {
                        i = end;
                        continue;
                    }
                    s = skip_space(b, s + 1);
                }
                let dollar = usize::from(b.get(s) == Some(&b'$'));
                let name_end = ident_end(b, s + dollar);
                if name_end > s + dollar {
                    let name = code[s..name_end].to_owned();
                    let body = if tok == "fn" {
                        fn_body(code, name_end)
                    } else {
                        let at = skip_space(b, name_end);
                        let (o, c) = match b.get(at) {
                            Some(b'{') => (b'{', b'}'),
                            Some(b'(') => (b'(', b')'),
                            _ => (b'[', b']'),
                        };
                        matching(code, at, o, c).map(|e| (at, e))
                    };
                    if let Some(body) = body {
                        if tok == "fn"
                            && dollar == 0
                            && attrs.contains("#[test]")
                            && !attrs.contains("should_panic")
                        {
                            tests.push((defs.len(), s));
                        }
                        defs.push(Def { name, body });
                    }
                }
                attrs.clear();
                i = name_end.max(end);
                continue;
            }
            if !matches!(
                tok,
                "pub" | "async" | "unsafe" | "const" | "extern" | "crate" | "in"
            ) {
                attrs.clear();
            }
            i = end;
            continue;
        }
        if c == b'(' && attrs.is_empty() {
            i += 1;
            continue;
        }
        if !matches!(c, b'(' | b')' | b'"') {
            attrs.clear();
        }
        i += 1;
    }
    (defs, tests)
}

/// The body of the `fn` whose name ends at `from`: from its `{` to the
/// matching `}`; `None` for a declaration that ends at `;`.
fn fn_body(code: &str, from: usize) -> Option<(usize, usize)> {
    let b = code.as_bytes();
    let mut i = from;
    let mut parens = 0usize;
    while i < b.len() {
        match b[i] {
            b'(' => parens += 1,
            b')' => parens = parens.saturating_sub(1),
            b'{' if parens == 0 => return matching(code, i, b'{', b'}').map(|e| (i, e)),
            b';' if parens == 0 => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Whether `body` asserts, given the names of same-file active definitions.
fn active(body: &str, known: &BTreeSet<&str>) -> bool {
    if body.contains('?') {
        return true;
    }
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let end = ident_end(b, i);
            let id = &body[i..end];
            let next = b.get(skip_space(b, end)).copied();
            let called = matches!(next, Some(b'(' | b'!' | b':'));
            let placeholder = i > 0 && b[i - 1] == b'$';
            if called && id.contains("assert")
                || matches!(id, "panic" | "unreachable") && next == Some(b'!')
                || matches!(id, "unwrap" | "expect" | "unwrap_err" | "expect_err")
                    && next == Some(b'(')
                || called && (placeholder || known.contains(id) || helper(id))
            {
                return true;
            }
            i = end;
        } else if b[i].is_ascii_digit() {
            i = ident_end(b, i);
        } else {
            i += 1;
        }
    }
    false
}

/// The passive tests of one file's text: (line, name).
fn passive(src: &str) -> Vec<(usize, String)> {
    let code = blank(src);
    let (defs, tests) = definitions(&code);
    let text = |d: &Def| &code[d.body.0..=d.body.1];
    let mut known: BTreeSet<&str> = BTreeSet::new();
    loop {
        let before = known.len();
        for d in &defs {
            if !known.contains(d.name.as_str()) && active(text(d), &known) {
                known.insert(&d.name);
            }
        }
        if known.len() == before {
            break;
        }
    }
    tests
        .into_iter()
        .filter(|(d, _)| !active(text(&defs[*d]), &known))
        .map(|(d, at)| (line_of(&code, at), defs[d].name.clone()))
        .collect()
}

pub(super) fn check(tree: &dyn Tree) -> Report {
    let baseline = match ratchet::load(tree, &WORDING) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let mut paths: Vec<String> = ROOTS
        .iter()
        .flat_map(|r| tree.files(r, &skipped))
        .filter(|p| p.ends_with(".rs"))
        .collect();
    paths.sort();
    let mut found = Vec::new();
    for path in paths {
        let Some(src) = tree.read(&path) else {
            continue;
        };
        let sites = passive(&src)
            .into_iter()
            .map(|(line, name)| format!("    {path}:{line}: `{name}` asserts nothing"))
            .collect();
        found.push(Found { path, sites });
    }
    ratchet::settle(&WORDING, &baseline, found)
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    fn names(src: &str) -> Vec<String> {
        passive(src).into_iter().map(|(_, n)| n).collect()
    }

    #[test]
    fn a_test_that_only_runs_is_passive_and_one_that_asserts_is_not() {
        let src = "#[test]\nfn runs() { f(); }\n#[test]\nfn pins() { assert_eq!(f(), 1); }\n\
                   #[test]\nfn pins_debug() { debug_assert!(f()); }\n";
        assert_eq!(passive(src), [(2, "runs".to_owned())]);
    }

    #[test]
    fn each_assertion_like_token_makes_a_test_active() {
        for body in [
            "assert!(x)",
            "assert_golden(x)",
            "panic!(\"no\")",
            "x.expect(\"a\")",
            "x.unwrap()",
            "x.unwrap_err()",
            "f()?",
            "if bad { unreachable!() }",
        ] {
            let src = format!("#[test]\nfn t() {{ {body}; }}\n");
            assert!(names(&src).is_empty(), "{body}");
        }
        for body in [
            "x.unwrap_or(1)",
            "matches!(x, A)",
            "let assertion = 1",
            "expected(x)",
        ] {
            let src = format!("#[test]\nfn t() {{ {body}; }}\n");
            assert_eq!(names(&src), ["t"], "{body}");
        }
    }

    #[test]
    fn a_should_panic_test_is_not_counted_whatever_order_its_attributes_come_in() {
        let src = "#[test]\n#[should_panic]\nfn a() { f(); }\n#[should_panic(expected = \"x\")]\n#[test]\nfn b() { f(); }\n\
                   #[test]\n#[ignore]\nfn c() { f(); }\n";
        assert_eq!(names(src), ["c"]);
    }

    #[test]
    fn a_call_of_a_same_file_helper_that_asserts_makes_a_test_active_through_any_depth() {
        let src = "fn inner() { assert!(true); }\nfn outer() { inner(); }\nfn quiet() { g(); }\n\
                   macro_rules! check { ($x:expr) => { assert!($x) }; }\n\
                   #[test]\nfn a() { outer(); }\n#[test]\nfn b() { quiet(); }\n\
                   #[test]\nfn c() { check!(1); }\n#[test]\nfn d() { let outer = 1; }\n";
        assert_eq!(names(src), ["b", "d"]);
    }

    #[test]
    fn a_cross_file_helper_by_name_makes_a_test_active_and_a_lookalike_does_not() {
        let src = "#[test]\nfn a() { check_range_contract::<X>(); }\n#[test]\nfn b() { checked(); }\n\
                   #[test]\nfn c() { expect_ok(); }\n";
        assert_eq!(names(src), ["b"]);
    }

    #[test]
    fn assertions_in_comments_and_strings_are_not_assertions() {
        let src = "#[test]\nfn a() {\n // assert_eq!(1, 1);\n let s = \"assert!(x)\";\n}\n";
        assert_eq!(names(src), ["a"]);
    }

    #[test]
    fn a_test_with_generics_a_return_type_and_visibility_is_found() {
        let src = "#[test]\npub fn a<T: Default>() -> Result<(), E> where T: Sized { Ok(()) }\n\
                   #[test]\nfn b() -> Result<(), E> { f()?; Ok(()) }\n\
                   trait T { fn decl(&self); }\n";
        assert_eq!(names(src), ["a"]);
    }

    #[test]
    fn a_macro_test_named_by_a_placeholder_is_skipped_and_one_calling_a_placeholder_is_active() {
        let src = "macro_rules! suite { ($n:ident) => { #[test] fn $n() { f(); } \
                   #[test] fn fixed() { g(); } #[test] fn via() { super::$n(1); } }; }\n";
        assert_eq!(names(src), ["fixed"]);
    }

    #[test]
    fn a_file_over_its_baseline_fails_with_each_test_and_a_missing_baseline_fails() {
        let files = [(
            "crates/a/src/lib.rs",
            "#[test]\nfn a() {}\n#[test]\nfn b() { assert!(1 < 2); }\n#[test]\nfn c() {}\n",
        )];
        let r = check(&Mem::with(&[
            (BASELINE, "1 crates/a/src/lib.rs\n"),
            files[0],
        ]));
        assert_eq!(
            r.findings,
            [
                "check-passive-tests: FAIL (a test asserts nothing; pin a property or delete it):",
                "  crates/a/src/lib.rs: 2 passive tests, baseline 1",
                "    crates/a/src/lib.rs:2: `a` asserts nothing",
                "    crates/a/src/lib.rs:6: `c` asserts nothing",
            ]
        );
        assert!(!check(&Mem::with(&files)).passed());
    }
}
