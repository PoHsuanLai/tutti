//! `check-doc-features`: rustdoc, one feature at a time, for the crates whose
//! public surface is feature-gated.
//!
//! `cargo doc --workspace` builds every crate with its default features, which
//! for these two is none, so it documents almost nothing of them; the
//! `--features full,plugin` line beside it sees the union but not a link that
//! only resolves with two features on. A doc comment under `#[cfg(feature =
//! "x")]` is compiled by rustdoc only when `x` is on, so a link out of it into
//! an item another feature gates breaks for exactly the consumer who turns on
//! `x` alone.
//!
//! The feature list is read from each Cargo.toml, so a new feature is covered
//! without editing [`CRATES`]. `RUSTDOCFLAGS` defaults to `-D warnings`.
//! (`crates/bevy-tutti` is parked, out of the workspace, so it is not
//! documented here; its feature docs come back with it.) `jack` is skipped: it
//! links libjack, as in `cargo xtask check-features`.
//!
//! This one is a list of commands, not a verdict over files: [`doc_runs`]
//! reads the manifests (pure, over a [`Tree`]) and the runner in `main.rs`
//! runs each [`DocRun`] and reports the ones that failed.
//!
//! The script read the manifests with `sed` and `awk`; both are written out:
//! - the name: the first line matching `^name = "(.*)"` (greedy, to the last
//!   quote on the line), with `sed -n 's/^name = "\(.*\)"/\1/p'`, which
//!   replaces only the matched part, so text after the last quote stays;
//! - the features: between a line starting `[features]` and the next line
//!   starting `[`, each line matching `^[a-z0-9_-]+ *=`, as its first
//!   whitespace-separated field (so `a=[]` is the field `a=[]`), minus the
//!   lines that are exactly `default` or `jack` (`grep -vx`).
//!
//! Mutation record (each planted, seen to fail the named test, removed):
//! - keep `jack`: `default_and_jack_are_not_documented_alone`;
//! - end the section at `[features.` only: `only_the_features_table_is_read`;
//! - drop the `Cargo.toml` existence check: `a_missing_manifest_is_an_error`.

use super::Tree;
use crate::cmd::Cmd;

/// The crates whose features are documented one at a time.
const CRATES: &[&str] = &["crates/tutti-session", "crates/tutti"];

/// Features that are not documented alone.
const SKIPPED: &[&str] = &["default", "jack"];

/// One `cargo doc` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocRun {
    pub(crate) package: String,
    pub(crate) feature: String,
}

impl DocRun {
    /// The line printed before it runs.
    pub(crate) fn heading(&self) -> String {
        format!(
            "== cargo doc -p {} --features {}",
            self.package, self.feature
        )
    }

    /// `RUSTDOCFLAGS` is the caller's unless it is unset or empty.
    pub(crate) fn command(&self, rustdocflags: Option<&str>) -> Cmd {
        Cmd::sh("cargo doc --locked -p")
            .arg(&self.package)
            .args(["--no-deps", "--features", &self.feature])
            .env(
                "RUSTDOCFLAGS",
                rustdocflags
                    .filter(|f| !f.is_empty())
                    .unwrap_or("-D warnings"),
            )
    }

    /// What the failure list names: `name/feature`.
    pub(crate) fn label(&self) -> String {
        format!("{}/{}", self.package, self.feature)
    }
}

/// Every `cargo doc` to run, in order, and the error that ends the list: a
/// crate that moved. The script went crate by crate, so the runs of the crates
/// before the missing one still happen, then the error stops everything (the
/// failures so far are not summarized).
pub(crate) fn doc_runs(tree: &dyn Tree) -> (Vec<DocRun>, Option<String>) {
    let mut runs = Vec::new();
    for dir in CRATES {
        let Some(manifest) = tree.read(&format!("{dir}/Cargo.toml")) else {
            return (
                runs,
                Some(format!(
                    "FAIL: {dir}/Cargo.toml does not exist; was the crate moved?"
                )),
            );
        };
        let name = package_name(&manifest).unwrap_or_default();
        for feature in features(&manifest) {
            runs.push(DocRun {
                package: name.clone(),
                feature,
            });
        }
    }
    (runs, None)
}

/// `sed -n 's/^name = "\(.*\)"/\1/p' | head -1`.
fn package_name(manifest: &str) -> Option<String> {
    manifest.split('\n').find_map(|l| {
        let rest = l.strip_prefix("name = \"")?;
        let last = rest.rfind('"')?;
        Some(format!("{}{}", &rest[..last], &rest[last + 1..]))
    })
}

fn features(manifest: &str) -> Vec<String> {
    let mut inside = false;
    let mut out = Vec::new();
    for l in manifest.split('\n') {
        if l.starts_with("[features]") {
            inside = true;
            continue;
        }
        if l.starts_with('[') {
            inside = false;
        }
        if inside && is_feature_line(l) {
            let first = l.split([' ', '\t']).next().unwrap_or(l);
            if !SKIPPED.contains(&first) {
                out.push(first.to_owned());
            }
        }
    }
    out
}

/// `^[a-z0-9_-]+ *=`
fn is_feature_line(l: &str) -> bool {
    let key = l
        .bytes()
        .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
        .count();
    key > 0 && l[key..].trim_start_matches(' ').starts_with('=')
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const SESSION: &str = r#"[package]
name = "tutti-session"
version = "0.1.0"

[features]
default = []
jack = ["x/jack"]
audio-io = ["a"]
clap = ["b"]   # trailing
midi_2 = []

[dependencies]
x = "1"
wasm = []
"#;
    const FACADE: &str = "[package]\nname = \"tutti\"\n[features]\nfull = []\nio=[]\n";

    fn tree() -> Mem {
        Mem::with(&[
            ("crates/tutti-session/Cargo.toml", SESSION),
            ("crates/tutti/Cargo.toml", FACADE),
        ])
    }

    fn runs(m: &Mem) -> Vec<String> {
        doc_runs(m).0.iter().map(DocRun::label).collect()
    }

    // Mutation: add "`jack`" to nothing, i.e. empty `SKIPPED`: this fails.
    #[test]
    fn default_and_jack_are_not_documented_alone() {
        assert_eq!(
            runs(&tree()),
            [
                "tutti-session/audio-io",
                "tutti-session/clap",
                "tutti-session/midi_2",
                "tutti/full",
                // awk's $1 of `io=[]` is the whole word, as the script had it.
                "tutti/io=[]",
            ]
        );
    }

    // Mutation: never reset `inside` on a `[` line: `wasm` under
    // [dependencies] is documented too.
    #[test]
    fn only_the_features_table_is_read() {
        let r = runs(&tree());
        assert!(!r.iter().any(|f| f.contains("wasm")));
        let m = Mem::with(&[
            (
                "crates/tutti-session/Cargo.toml",
                "name = \"s\"\n[features.x]\na = []\n[features]\nb = []\n[other]\nc = []\n",
            ),
            ("crates/tutti/Cargo.toml", "name = \"t\"\n"),
        ]);
        assert_eq!(runs(&m), ["s/b"]);
    }

    #[test]
    fn the_name_is_the_seds() {
        assert_eq!(package_name("name = \"a\"\n").as_deref(), Some("a"));
        // `sed s///` replaces only the match: the rest of the line stays.
        assert_eq!(package_name("name = \"a\" # c\n").as_deref(), Some("a # c"));
        // Only a line that starts with it, and the first such.
        assert_eq!(
            package_name(" name = \"x\"\nname = \"y\"\nname = \"z\"\n").as_deref(),
            Some("y")
        );
        assert_eq!(package_name("name=\"a\"\n"), None);
    }

    #[test]
    fn a_feature_line_is_the_awks() {
        for l in ["a = []", "a=[]", "a-b_1  =x", "a ="] {
            assert!(is_feature_line(l), "{l}");
        }
        for l in ["A = []", "= []", " a = []", "a b = []", "# a = []", ""] {
            assert!(!is_feature_line(l), "{l}");
        }
    }

    // Mutation: drop the existence check (use "" for a missing manifest): this
    // fails.
    #[test]
    fn a_missing_manifest_is_an_error() {
        let mut m = tree();
        m.files.remove("crates/tutti/Cargo.toml");
        let (runs, err) = doc_runs(&m);
        assert_eq!(
            err.as_deref(),
            Some("FAIL: crates/tutti/Cargo.toml does not exist; was the crate moved?")
        );
        // The crate before it was still documented.
        assert_eq!(runs.len(), 3);
        m.files.remove("crates/tutti-session/Cargo.toml");
        let (runs, err) = doc_runs(&m);
        assert!(runs.is_empty() && err.unwrap().contains("tutti-session/Cargo.toml"));
    }

    #[test]
    fn a_run_is_cargo_doc_for_one_feature() {
        let r = DocRun {
            package: "tutti-session".into(),
            feature: "audio-io".into(),
        };
        assert_eq!(
            r.heading(),
            "== cargo doc -p tutti-session --features audio-io"
        );
        assert_eq!(
            r.command(None).display(),
            "RUSTDOCFLAGS='-D warnings' cargo doc --locked -p tutti-session --no-deps --features audio-io"
        );
        // The caller's flags win; an empty value is the default, as `${X:-d}`.
        assert!(
            r.command(Some("-D missing_docs"))
                .display()
                .starts_with("RUSTDOCFLAGS='-D missing_docs' ")
        );
        assert!(
            r.command(Some(""))
                .display()
                .starts_with("RUSTDOCFLAGS='-D warnings' ")
        );
    }
}
