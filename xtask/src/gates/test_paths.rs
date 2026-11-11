//! A test finds a repo file through `tutti_fixtures`, never through a path that
//! depends on its own depth (part of `check-layout`).
//!
//! In a `.rs` file under `crates/` or `testing/` (build scripts and
//! [`EXEMPT_FILES`] excepted; `include_str!`, `include!`, `include_bytes!` and
//! `#[path]` are source-relative and ignored) it denies a string literal with
//! `..` as a path component when the file mentions `CARGO_MANIFEST_DIR` or the
//! literal is an argument of `Path::new`, `PathBuf::from`, `.join`, `.push`,
//! `.starts_with` or `concat!`; a literal climbing to `assets`; and
//! `.ancestors()` or `.parent()` in a file that mentions `CARGO_MANIFEST_DIR`.
//!
//! Limits: a textual scan. A `..` built at run time (`format!`, a non-literal)
//! or a literal passed through a helper in a file that does not mention
//! `CARGO_MANIFEST_DIR` is not seen.

use super::Tree;
use super::lex::line_of;

const SKIPPED_DIRS: &[&str] = &["target", ".git"];

fn skipped(dir: &str) -> bool {
    SKIPPED_DIRS.contains(&dir)
}

/// Any other legitimate use, with the reason.
const EXEMPT_FILES: &[(&str, &str)] = &[(
    "testing/tutti-fixtures/src/lib.rs",
    "implements repo_root and profile_dir",
)];

/// A string literal `"(?:[^"\\]|\\.)*"` starting at `at` (a `"`): its end.
fn literal_at(text: &str, at: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = at + 1;
    while i < b.len() {
        match b[i] {
            b'"' => return Some(i + 1),
            b'\\' if b.get(i + 1).is_some_and(|&c| c != b'\n') => i += 2,
            b'\\' => return None,
            _ => i += 1,
        }
    }
    None
}

/// Every literal, left to right, a failed opening quote skipped.
fn literals(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = text[i..].find('"') {
        let at = i + p;
        match literal_at(text, at) {
            Some(end) => {
                out.push((at, end));
                i = end;
            }
            None => i = at + 1,
        }
    }
    out
}

/// The text with the crate-relative macros blanked, offsets and newlines kept.
fn without_source_relative(text: &str) -> String {
    let mut out = text.to_owned().into_bytes();
    let mut blank = |from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    let ws = |s: &str| s.len() - s.trim_start().len();
    for name in ["include_str!(", "include_bytes!(", "include!("] {
        for (at, _) in text.match_indices(name) {
            let mut p = at + name.len();
            p += ws(&text[p..]);
            if text[p..].starts_with('"')
                && let Some(end) = literal_at(text, p)
            {
                let q = end + ws(&text[end..]);
                if text[q..].starts_with(')') {
                    blank(at, q + 1);
                }
            }
        }
    }
    for (at, _) in text.match_indices("#[path") {
        let mut p = at + "#[path".len();
        p += ws(&text[p..]);
        if !text[p..].starts_with('=') {
            continue;
        }
        p += 1;
        p += ws(&text[p..]);
        if text[p..].starts_with('"')
            && let Some(end) = literal_at(text, p)
        {
            let q = end + ws(&text[end..]);
            if text[q..].starts_with(']') {
                blank(at, q + 1);
            }
        }
    }
    String::from_utf8(out).expect("only ASCII spaces were written over whole spans")
}

fn has_dotdot(lit: &str) -> bool {
    lit[1..lit.len() - 1].split(['/', '\\']).any(|c| c == "..")
}

/// A `Path::new("..")`-like call whose first argument is a literal, in `window`.
fn path_call(window: &str) -> bool {
    [
        "Path::new",
        "PathBuf::from",
        ".join",
        ".push",
        ".starts_with",
        "concat!",
    ]
    .iter()
    .any(|f| {
        window.match_indices(f).any(|(at, _)| {
            let rest = window[at + f.len()..].strip_prefix('(');
            rest.map(str::trim_start).is_some_and(|r| {
                r.starts_with('"') && literal_at(window, window.len() - r.len()).is_some()
            })
        })
    })
}

/// `(?:\.\./)+[^"]*assets` in a literal.
fn climbs_to_assets(lit: &str) -> bool {
    lit.find("../").is_some_and(|p| {
        lit[p + 3..]
            .split('"')
            .next()
            .unwrap_or("")
            .contains("assets")
    })
}

pub(super) fn check(tree: &dyn Tree, bad: &mut Vec<String>) {
    for top in ["crates", "testing"] {
        for path in tree.files(top, &skipped) {
            let name = path.rsplit('/').next().unwrap_or(&path);
            if !path.ends_with(".rs")
                || name == "build.rs"
                || EXEMPT_FILES.iter().any(|(f, _)| *f == path)
            {
                continue;
            }
            let raw = tree.read(&path).unwrap_or_default();
            let text = without_source_relative(&raw);
            let manifest = text.contains("CARGO_MANIFEST_DIR");
            let mut report = |at: usize, why: String| {
                bad.push(format!(
                    "{path}:{}: depth-dependent path ({why}); use tutti_fixtures::asset or repo_root",
                    line_of(&text, at)
                ));
            };
            for (s, e) in literals(&text) {
                let lit = &text[s..e];
                if has_dotdot(lit)
                    && (manifest || {
                        let from = (s.saturating_sub(40)..=s)
                            .find(|&i| text.is_char_boundary(i))
                            .unwrap_or(s);
                        path_call(&text[from..e])
                    })
                {
                    report(s, format!("{lit} climbs with `..`"));
                } else if climbs_to_assets(lit) {
                    report(s, format!("{lit} climbs to assets"));
                }
            }
            if manifest {
                for pat in [".ancestors()", ".parent()"] {
                    for (at, _) in text.match_indices(pat) {
                        report(
                            at,
                            format!("`{pat}` in a file that reads CARGO_MANIFEST_DIR"),
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    fn run(src: &str) -> Vec<String> {
        let mut bad = Vec::new();
        check(
            &Mem::with(&[("crates/tutti-dsp/src/lib.rs", src)]),
            &mut bad,
        );
        bad
    }

    fn at(line: usize, lit: &str, why: &str) -> String {
        format!(
            "crates/tutti-dsp/src/lib.rs:{line}: depth-dependent path ({lit} {why}); use tutti_fixtures::asset or repo_root"
        )
    }

    #[test]
    fn a_relative_climb_in_a_path_call_or_beside_the_manifest_dir_fails() {
        assert_eq!(
            run("let p = Path::new(\"../../samples/x.sf2\");\n"),
            [at(1, "\"../../samples/x.sf2\"", "climbs with `..`")]
        );
        assert_eq!(run("let m = env!(\"CARGO_MANIFEST_DIR\");\nlet p = m.to_owned() + \"/../../fixtures/x\";\n").len(), 1);
        assert_eq!(
            run("let m = env!(\"CARGO_MANIFEST_DIR\");\np.join(\"..\").join(\"..\");\n").len(),
            2
        );
        assert_eq!(
            run("let p = concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../assets/x\");\n").len(),
            1
        );
    }

    #[test]
    fn ancestors_and_parent_are_depth_walks_beside_the_manifest_dir() {
        let bad = run("let m = Path::new(env!(\"CARGO_MANIFEST_DIR\")).ancestors().nth(2);\n");
        assert_eq!(bad.len(), 1);
        assert!(bad[0].contains("`.ancestors()` in a file that reads CARGO_MANIFEST_DIR"));
    }

    #[test]
    fn source_relative_macros_a_plain_literal_and_exempt_files_pass() {
        assert!(
            run("let s = include_str!(\"../data/x.txt\");\n#[path = \"../x.rs\"]\nmod x;\n")
                .is_empty()
        );
        assert!(run("let s = \"..\";\nlet t = \"a/../b\";\n").is_empty());
        assert!(run("let p = tutti_fixtures::asset(\"audio/stereo.flac\");\n").is_empty());
        let mut bad = Vec::new();
        check(
            &Mem::with(&[
                (
                    "testing/tutti-fixtures/src/lib.rs",
                    "env!(\"CARGO_MANIFEST_DIR\"); a.parent();\n",
                ),
                ("crates/c/build.rs", "Path::new(\"../x\");\n"),
            ]),
            &mut bad,
        );
        assert!(bad.is_empty());
    }
}
