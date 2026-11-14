//! `check-errors`: an error type is `thiserror`'s, and every public one is an error.
//!
//! - **Every `pub` type named `...Error`**, or deriving `thiserror::Error` (spelled
//!   `Error` once imported), implements
//!   `std::error::Error`: by the derive, or by an `impl ... Error for` in its crate. A
//!   `pub` type with the name of an error and no impl cannot be `?`-converted into
//!   `anyhow::Error` or `Box<dyn Error>`.
//! - **No hand-written `impl (core::|std::)?(error::)?Error for`** in a crate's
//!   `src/`, macro bodies included: the derive writes the `Display` beside the
//!   variant it describes, so the two cannot drift. A hand impl is a row of
//!   [`ALLOW`], `(crate, type, reason)`, and a row that matches nothing is stale.
//!   A row is `permanent:` (the derive cannot express the type, and the reason
//!   says why) or `temporary:` (a lane converts it, and the row leaves with it).
//!
//! An allowed type is exempt from both rules. Comments, string literals and
//! `#[cfg(test)]` items are not code, and a file named `tests.rs` or under a
//! `tests/` directory of `src/` is a test module. `Send + Sync + 'static` is not
//! text-checkable: the umbrella's `error_bounds` test holds it.

use std::collections::BTreeSet;

use super::lex::{blank, find_word, find_word_from, line_of, matching};
use super::{Report, Tree};

const SKIPPED_DIRS: &[&str] = &["target", ".git"];

fn skipped(dir: &str) -> bool {
    SKIPPED_DIRS.contains(&dir)
}

/// `(crate directory, type, reason)`. The reason starts with `permanent:` or
/// `temporary:`.
pub(super) const ALLOW: &[(&str, &str, &str)] = &[
    (
        "tutti-graph",
        "ForkFault",
        "permanent: `source()` is `ForkCause::get()`, and `ForkCause` is a wrapper that is not itself an `Error`, so `#[source]` cannot name it",
    ),
    (
        "tutti-graph",
        "ForkError",
        "permanent: `source()` is `ForkCause::get()`, and `ForkCause` is a wrapper that is not itself an `Error`, so `#[source]` cannot name it",
    ),
    (
        "tutti-session",
        "RecoveryFailure",
        "permanent: `source()` is the pointee of an `Arc`; `#[source]` would expose the `Arc`, which a chain walker cannot downcast",
    ),
    (
        "tutti-session",
        "RestartError",
        "permanent: `source()` is the pointee of an `Arc`; `#[source]` would expose the `Arc`, which a chain walker cannot downcast",
    ),
    (
        "tutti-session",
        "Problem",
        "permanent: `Unfed` has a field named `source` that is a `SourceName`, not an `Error`, and `thiserror` takes that name as the source",
    ),
    (
        "tutti-au-ffi",
        "BundleError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-au-ffi",
        "OsError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-au-ffi",
        "Error",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-au-ffi",
        "Status",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-clap-ffi",
        "GuiError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-clap-ffi",
        "LoadError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-plugin-types",
        "PluginError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-plugin-types",
        "PluginFailure",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-au-ffi",
        "LastRenderError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-au-host",
        "LoadFailedError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-au-host",
        "PresetFileError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-clap-ffi",
        "ProcessError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-clap-ffi",
        "CallError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-clap-ffi",
        "ActivateError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-plugin-ipc",
        "ChunkError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-vst3-ffi",
        "CreateError",
        "temporary: plugins session converts it",
    ),
    (
        "tutti-vst3-ffi",
        "EditorError",
        "temporary: plugins session converts it",
    ),
];

/// The spellings of the trait in an `impl ... for` header.
const TRAIT_PATHS: &[&str] = &[
    "Error",
    "error::Error",
    "std::error::Error",
    "core::error::Error",
];

/// Where in the source, and which type.
struct Item {
    krate: String,
    name: String,
    at: String,
}

/// What one file holds.
#[derive(Default)]
struct Scan {
    /// `impl ... Error for Type` headers.
    impls: Vec<Item>,
    /// `pub struct`/`pub enum` items whose name ends in `Error`.
    named: Vec<Item>,
    /// Types with `#[derive(.. thiserror::Error ..)]`.
    derived: Vec<Item>,
}

pub(super) fn check(tree: &dyn Tree, allow: &[(&str, &str, &str)]) -> Report {
    let files = tree.files("crates", &skipped);
    let mut scan = Scan::default();
    let mut crates = BTreeSet::new();
    let mut read = 0;
    for path in &files {
        let Some(rest) = path.strip_prefix("crates/") else {
            continue;
        };
        let Some((krate, rel)) = rest.split_once('/') else {
            continue;
        };
        let Some(rel) = rel.strip_prefix("src/") else {
            continue;
        };
        if !rel.ends_with(".rs") || is_test_file(rel) {
            continue;
        }
        let Some(text) = tree.read(path) else {
            continue;
        };
        crates.insert(krate.to_owned());
        read += 1;
        scan_file(&mut scan, krate, path, &text);
    }

    let mut bad: Vec<String> = Vec::new();
    if crates.len() < 20 {
        bad.push(format!(
            "only {} crates with sources found under crates/; the scan is broken",
            crates.len()
        ));
    }
    for (krate, name, reason) in allow {
        if !(reason.starts_with("permanent:") || reason.starts_with("temporary:")) {
            bad.push(format!(
                "ALLOW {krate}::{name}: the reason must start with `permanent:` or `temporary:`"
            ));
        }
    }

    let allowed = |i: &Item| allow.iter().any(|(k, n, _)| *k == i.krate && *n == i.name);
    let implements = |i: &Item| {
        scan.derived
            .iter()
            .chain(&scan.impls)
            .any(|d| d.krate == i.krate && d.name == i.name)
    };

    let mut used: BTreeSet<(String, String)> = BTreeSet::new();
    for i in &scan.impls {
        if allowed(i) {
            used.insert((i.krate.clone(), i.name.clone()));
        } else {
            bad.push(format!(
                "{}: hand-written `impl Error for {}`; derive `thiserror::Error` and write the \
                 text in `#[error(..)]` (a type the derive cannot express goes in ALLOW in \
                 xtask/src/gates/errors.rs, with the reason)",
                i.at, i.name
            ));
        }
    }
    for i in &scan.named {
        if implements(i) {
            continue;
        }
        if allowed(i) {
            used.insert((i.krate.clone(), i.name.clone()));
        } else {
            bad.push(format!(
                "{}: `pub` type `{}` is not an `Error`; derive `thiserror::Error` (or rename it)",
                i.at, i.name
            ));
        }
    }
    for (krate, name, _) in allow {
        if !used.contains(&((*krate).to_owned(), (*name).to_owned())) {
            bad.push(format!(
                "ALLOW {krate}::{name}: stale; nothing hand-implements it or leaves it without an \
                 `Error` impl any more, remove the row"
            ));
        }
    }

    let mut r = Report::default();
    if bad.is_empty() {
        r.say(format!(
            "check-errors: ok ({read} files, {} derived, {} hand impls allowed)",
            scan.derived.len(),
            used.len()
        ));
    } else {
        r.fail("check-errors: FAIL");
        for b in bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

/// Whether a path under `src/` is a test module by name.
fn is_test_file(rel: &str) -> bool {
    rel.split('/').any(|c| c == "tests" || c == "tests.rs")
}

fn scan_file(scan: &mut Scan, krate: &str, path: &str, text: &str) {
    let code = without_tests(&blank(text));
    let item = |name: &str, at: usize| Item {
        krate: krate.to_owned(),
        name: name.to_owned(),
        at: format!("{path}:{}", line_of(&code, at)),
    };

    let mut from = 0;
    while let Some(at) = find_word_from(&code, "impl", from) {
        from = at + 4;
        let Some(end) = code[at..].find(['{', ';']) else {
            continue;
        };
        let header = &code[at + 4..at + end];
        let Some((tr, ty)) = split_impl(header) else {
            continue;
        };
        if TRAIT_PATHS.contains(&tr.as_str()) {
            scan.impls.push(item(&type_name(&ty), at));
        }
    }

    for kw in ["struct", "enum"] {
        let mut from = 0;
        while let Some(at) = find_word_from(&code, kw, from) {
            from = at + kw.len();
            let name = ident_after(&code, from);
            if name.is_empty() {
                continue;
            }
            let head = code[..at].trim_end();
            let public = head.ends_with("pub");
            if public && name.ends_with("Error") {
                scan.named.push(item(&name, at));
            }
        }
    }

    let mut from = 0;
    while let Some(at) = code[from..].find("#[derive(").map(|p| from + p) {
        let open = at + "#[derive".len();
        let Some(close) = matching(&code, open, b'(', b')') else {
            break;
        };
        from = close;
        let list: String = code[open..close].split_whitespace().collect();
        if !list
            .split([',', '('])
            .any(|d| d == "Error" || d == "thiserror::Error")
        {
            continue;
        }
        let rest = &code[close..];
        let next = ["struct", "enum"]
            .iter()
            .filter_map(|kw| find_word(rest, kw).map(|p| (p, kw.len())))
            .min();
        if let Some((p, len)) = next {
            let name = ident_after(rest, p + len);
            if !name.is_empty() {
                scan.derived.push(item(&name, at));
            }
        }
    }
}

/// `(trait path, self type)` of an `impl` header (the text between `impl` and
/// the opening brace), or `None` for an inherent impl.
fn split_impl(header: &str) -> Option<(String, String)> {
    let mut h = header.trim_start();
    if h.starts_with('<') {
        let close = matching(h, 0, b'<', b'>')?;
        h = h[close + 1..].trim_start();
    }
    let at = find_word(h, "for")?;
    let tr: String = h[..at].split_whitespace().collect();
    let ty = h[at + 3..].trim().to_owned();
    Some((tr, ty))
}

/// The bare name of a type in an `impl` header: its path's last segment, a
/// macro variable kept (`$name`), generics and a `where` clause dropped.
fn type_name(ty: &str) -> String {
    let ty = ty.split(" where").next().unwrap_or(ty);
    let ty = ty.split('<').next().unwrap_or(ty).trim();
    ty.rsplit("::").next().unwrap_or(ty).to_owned()
}

/// The identifier that follows `at` (after whitespace); a macro variable keeps
/// its `$`.
fn ident_after(code: &str, at: usize) -> String {
    code[at..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
        .collect()
}

/// `code` with every `#[cfg(test)]` item blanked (newlines kept).
fn without_tests(code: &str) -> String {
    let mut out = code.to_owned();
    let mut from = 0;
    while let Some(p) = out[from..].find("#[cfg(test)]").map(|p| from + p) {
        let attr_end = p + "#[cfg(test)]".len();
        let Some(stop) = out[attr_end..].find(['{', ';']).map(|q| attr_end + q) else {
            break;
        };
        let end = if out.as_bytes()[stop] == b'{' {
            matching(&out, stop, b'{', b'}').map_or(out.len(), |c| c + 1)
        } else {
            stop + 1
        };
        let blanked: String = out[p..end]
            .chars()
            .map(|c| if c == '\n' { '\n' } else { ' ' })
            .collect();
        out.replace_range(p..end, &blanked);
        from = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    /// Enough crates for the scan to believe it found the repo.
    fn tree(lib: &str) -> Mem {
        let mut files: Vec<(String, String)> = (0..20)
            .map(|i| (format!("crates/c{i}/src/lib.rs"), String::new()))
            .collect();
        files.push(("crates/tutti-x/src/lib.rs".to_owned(), lib.to_owned()));
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        Mem::with(&refs)
    }

    fn findings(lib: &str, allow: &[(&str, &str, &str)]) -> Vec<String> {
        check(&tree(lib), allow).findings
    }

    const HAND: &str =
        "pub struct E;\nimpl std::fmt::Display for E {}\nimpl std::error::Error for E {}\n";

    #[test]
    fn a_hand_written_impl_is_flagged() {
        for spelling in [
            "Error",
            "error::Error",
            "std::error::Error",
            "core::error::Error",
        ] {
            let src = format!("struct E;\nimpl {spelling} for E {{}}\n");
            let f = findings(&src, &[]);
            assert_eq!(f.len(), 2, "{spelling}: {f:?}");
            assert!(f[1].contains("crates/tutti-x/src/lib.rs:2"), "{f:?}");
            assert!(f[1].contains("hand-written `impl Error for E`"), "{f:?}");
        }
        let generic = "impl<T: Debug> std::error::Error for Wrap<T> where T: Send {}\n";
        assert!(findings(generic, &[])[1].contains("for Wrap`"));
    }

    #[test]
    fn an_allowed_impl_passes_and_is_counted() {
        let allow = [("tutti-x", "E", "permanent: the source is an Arc")];
        let r = check(&tree(HAND), &allow);
        assert!(r.passed(), "{:?}", r.findings);
        assert!(r.out[0].contains("1 hand impls allowed"), "{:?}", r.out);
    }

    #[test]
    fn a_stale_allow_entry_fails() {
        let allow = [("tutti-x", "Gone", "temporary: a lane converts it")];
        let f = findings(
            "#[derive(thiserror::Error)]\n#[error(\"x\")]\npub struct E;\n",
            &allow,
        );
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[1].contains("ALLOW tutti-x::Gone: stale"), "{f:?}");
        let other_crate = [("tutti-y", "E", "temporary: a lane converts it")];
        assert!(
            findings(HAND, &other_crate)
                .iter()
                .any(|l| l.contains("stale"))
        );
    }

    #[test]
    fn an_allow_reason_names_its_kind() {
        let allow = [("tutti-x", "E", "the source is an Arc")];
        let f = findings(HAND, &allow);
        assert!(
            f.iter().any(|l| l.contains("`permanent:` or `temporary:`")),
            "{f:?}"
        );
    }

    #[test]
    fn a_macro_generated_impl_is_flagged() {
        let src = "macro_rules! refusal {\n    ($name:ident) => {\n        impl std::error::Error for $name {}\n    };\n}\n";
        let f = findings(src, &[]);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(
            f[1].contains("lib.rs:3") && f[1].contains("for $name"),
            "{f:?}"
        );
    }

    #[test]
    fn a_test_module_impl_is_ignored() {
        let src =
            "#[cfg(test)]\nmod tests {\n    struct E;\n    impl std::error::Error for E {}\n}\n";
        assert!(findings(src, &[]).is_empty());
        let item = "#[cfg(test)]\nimpl std::error::Error for E {}\n";
        assert!(findings(item, &[]).is_empty());
        let mut files = tree("").files;
        files.insert(
            "crates/tutti-x/src/voice/tests/e.rs".to_owned(),
            "impl std::error::Error for E {}\n".to_owned(),
        );
        assert!(
            check(
                &Mem {
                    files,
                    ..Mem::default()
                },
                &[]
            )
            .passed()
        );
    }

    #[test]
    fn code_after_a_test_module_is_still_read() {
        let src = "#[cfg(test)]\nmod tests {\n    fn f() {}\n}\nimpl std::error::Error for E {}\n";
        assert_eq!(findings(src, &[]).len(), 2);
    }

    #[test]
    fn a_commented_out_or_quoted_impl_is_ignored() {
        let src = "// impl std::error::Error for E {}\n/* impl Error for E {} */\nconst S: &str = \"impl Error for E {}\";\n/// impl Error for E {}\npub struct NotErrorError;\n";
        let f = findings(src, &[]);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[1].contains("`NotErrorError` is not an `Error`"), "{f:?}");
    }

    #[test]
    fn a_derive_passes() {
        let src = "#[derive(Debug, thiserror::Error)]\n#[error(\"x\")]\npub struct FooError;\n\
                   #[derive(\n    Clone,\n    thiserror::Error,\n)]\npub enum Bar {\n    #[error(\"y\")]\n    Y,\n}\n";
        let r = check(&tree(src), &[]);
        assert!(r.passed(), "{:?}", r.findings);
        assert!(r.out[0].contains("2 derived"), "{:?}", r.out);
    }

    #[test]
    fn a_public_error_without_an_impl_is_flagged() {
        let f = findings("#[derive(Debug)]\npub struct FooError;\n", &[]);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(
            f[1].contains("lib.rs:2") && f[1].contains("`FooError` is not an `Error`"),
            "{f:?}"
        );
        let en = findings("pub enum BarError { A }\n", &[]);
        assert!(en[1].contains("`BarError`"), "{en:?}");
    }

    #[test]
    fn a_private_or_crate_visible_error_name_is_not_a_public_error() {
        assert!(findings("struct FooError;\npub(crate) struct BarError;\n", &[]).is_empty());
        assert!(findings("pub struct Failure;\n", &[]).is_empty());
    }

    #[test]
    fn an_allowed_public_error_without_an_impl_passes() {
        let allow = [("tutti-x", "FooError", "temporary: a lane converts it")];
        assert!(check(&tree("pub struct FooError;\n"), &allow).passed());
    }

    #[test]
    fn a_hand_impl_satisfies_the_public_error_rule_but_not_the_hand_rule() {
        let src = "pub struct FooError;\nimpl std::error::Error for FooError {}\n";
        let f = findings(src, &[]);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[1].contains("hand-written"), "{f:?}");
    }

    #[test]
    fn a_scan_that_finds_no_crates_fails() {
        let r = check(&Mem::default(), &[]);
        assert!(!r.passed());
    }
}
