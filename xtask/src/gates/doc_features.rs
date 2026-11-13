//! `check-doc-features`: rustdoc over the features a workspace build leaves
//! off.
//!
//! `cargo doc --workspace` builds every crate with the features some member's
//! edge turns on, so an item behind a feature nothing in the workspace enables
//! (a `contract` harness, `test-support`, a codec, a plugin format) and the
//! links in its docs are never documented there. A doc comment under
//! `#[cfg(feature = "x")]` is compiled by rustdoc only when `x` is on. Two kinds
//! of run close that:
//!
//! - **Each member with its features together**, private items included: one
//!   `cargo doc -p <member> --features <every feature>` per workspace member
//!   that has features, with `--document-private-items`. A broken or redundant
//!   link in gated code reached main from five crates this way before this run
//!   existed (`tutti-codec`, `tutti-cpal`, `tutti-nodes`, `tutti-sampler`,
//!   `tutti-vst3-ffi`).
//! - **Each feature alone**, for the crates whose public surface is
//!   feature-gated ([`ONE_AT_A_TIME`]): the union cannot see a link out of one
//!   feature's docs into an item only another feature gates, which breaks for
//!   exactly the consumer who turns on the first alone.
//!
//! The members come from the root manifest's `members` and the features from
//! each member's `[features]` table, so a new crate or feature is covered with
//! no edit here. Skipped: `default` (not a feature of its own), `jack` (its
//! build script wants the JACK headers, as in `cargo xtask check-features`; the
//! CI rustdoc job documents the umbrellas with `--all-features`, JACK included).
//! `RUSTDOCFLAGS` is the caller's when set.
//!
//! This one is a list of commands, not a verdict over files: [`doc_runs`]
//! reads the manifests (pure, over a [`Tree`]) and the runner in `main.rs`
//! runs each [`DocRun`] and reports the ones that failed.

use super::Tree;
use super::toml::{Toml, strings};
use crate::cmd::Cmd;

/// The crates whose features are also documented one at a time.
const ONE_AT_A_TIME: &[&str] = &["crates/tutti-session", "crates/tutti"];

/// Features never documented, alone or together.
const SKIPPED: &[&str] = &["default", "jack"];

/// One `cargo doc` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocRun {
    pub(crate) package: String,
    /// The features turned on, in the manifest's order.
    pub(crate) features: Vec<String>,
    /// Whether this is a member's union (with private items) or one feature.
    pub(crate) union: bool,
}

impl DocRun {
    /// The line printed before it runs.
    pub(crate) fn heading(&self) -> String {
        format!(
            "== cargo doc -p {} --features {}{}",
            self.package,
            self.features.join(","),
            if self.union { " (private items)" } else { "" }
        )
    }

    /// `RUSTDOCFLAGS` is the caller's unless it is unset or empty; the default
    /// denies warnings, and a union run also documents private items.
    pub(crate) fn command(&self, rustdocflags: Option<&str>) -> Cmd {
        let default = if self.union {
            "-D warnings --document-private-items"
        } else {
            "-D warnings"
        };
        Cmd::sh("cargo doc --locked -p")
            .arg(&self.package)
            .args(["--no-deps", "--features", &self.features.join(",")])
            .env(
                "RUSTDOCFLAGS",
                rustdocflags.filter(|f| !f.is_empty()).unwrap_or(default),
            )
    }

    /// What the failure list names: `name/feature`, or `name/*` for a union.
    pub(crate) fn label(&self) -> String {
        if self.union {
            format!("{}/*", self.package)
        } else {
            format!("{}/{}", self.package, self.features.join(","))
        }
    }
}

/// Every `cargo doc` to run, in order (each member's union in the root's
/// member order, then the one-at-a-time crates' features), and the errors that
/// fail the gate whatever the runs say: a member or a listed crate with no
/// manifest, or a manifest with no name.
pub(crate) fn doc_runs(tree: &dyn Tree) -> (Vec<DocRun>, Vec<String>) {
    let mut runs = Vec::new();
    let mut errors = Vec::new();
    let root = Toml::parse(&tree.read("Cargo.toml").unwrap_or_default());
    let members = root
        .value("workspace", "members")
        .map(strings)
        .unwrap_or_default();
    if members.is_empty() {
        errors.push("FAIL: Cargo.toml: no workspace members found; the scan is broken".into());
    }
    for dir in &members {
        let Some((package, features)) = manifest(tree, dir, &mut errors) else {
            continue;
        };
        if !features.is_empty() {
            runs.push(DocRun {
                package,
                features,
                union: true,
            });
        }
    }
    for dir in ONE_AT_A_TIME {
        let Some((package, features)) = manifest(tree, dir, &mut errors) else {
            continue;
        };
        for feature in features {
            runs.push(DocRun {
                package: package.clone(),
                features: vec![feature],
                union: false,
            });
        }
    }
    (runs, errors)
}

/// The package name and the features (less [`SKIPPED`]) of `dir`'s manifest.
fn manifest(tree: &dyn Tree, dir: &str, errors: &mut Vec<String>) -> Option<(String, Vec<String>)> {
    let Some(text) = tree.read(&format!("{dir}/Cargo.toml")) else {
        errors.push(format!(
            "FAIL: {dir}/Cargo.toml does not exist; was the crate moved?"
        ));
        return None;
    };
    let toml = Toml::parse(&text);
    let Some(name) = toml
        .value("package", "name")
        .and_then(|v| strings(v).into_iter().next())
    else {
        errors.push(format!("FAIL: {dir}/Cargo.toml has no package name"));
        return None;
    };
    let features = toml
        .section("features")
        .map(|s| {
            s.entries
                .iter()
                .map(|(k, _)| k.clone())
                .filter(|k| !SKIPPED.contains(&k.as_str()))
                .collect()
        })
        .unwrap_or_default();
    Some((name, features))
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const ROOT: &str = "[workspace]\nmembers = [\n  \"crates/tutti-session\",\n  \"crates/tutti\",\n  \"crates/plain\",\n  \"crates/tutti-types\",\n]\n";

    const SESSION: &str = r#"[package]
name = "tutti-session"
version = "0.1.0"

[features]
default = ["audio-io"]
jack = ["x/jack"]
audio-io = ["a"]
clap = ["b"]   # trailing
midi_2 = []

[dependencies]
x = "1"
wasm = []
"#;
    const FACADE: &str = "[package]\nname = \"tutti\"\n[features]\nfull = []\nio=[]\n";
    const PLAIN: &str = "[package]\nname = \"plain\"\n[dependencies]\nx = \"1\"\n";
    const TYPES: &str = "[package]\nname = \"tutti-types\"\n[features]\nserde = []\n";

    fn tree() -> Mem {
        Mem::with(&[
            ("Cargo.toml", ROOT),
            ("crates/tutti-session/Cargo.toml", SESSION),
            ("crates/tutti/Cargo.toml", FACADE),
            ("crates/plain/Cargo.toml", PLAIN),
            ("crates/tutti-types/Cargo.toml", TYPES),
        ])
    }

    fn labels(m: &Mem) -> Vec<String> {
        doc_runs(m).0.iter().map(DocRun::label).collect()
    }

    /// Every member with features is documented with them together, in member
    /// order, then the listed crates one feature at a time; a member with no
    /// features has no run.
    #[test]
    fn each_member_with_features_is_documented_whole_then_the_listed_ones_alone() {
        assert_eq!(
            labels(&tree()),
            [
                "tutti-session/*",
                "tutti/*",
                "tutti-types/*",
                "tutti-session/audio-io",
                "tutti-session/clap",
                "tutti-session/midi_2",
                "tutti/full",
                "tutti/io",
            ]
        );
        let (runs, errors) = doc_runs(&tree());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(runs[0].features, ["audio-io", "clap", "midi_2"]);
    }

    /// `default` and `jack` are never documented.
    #[test]
    fn default_and_jack_are_left_out() {
        let (runs, _) = doc_runs(&tree());
        for r in &runs {
            for f in &r.features {
                assert!(!["default", "jack"].contains(&f.as_str()), "{r:?}");
            }
        }
        let types = runs.iter().find(|r| r.package == "tutti-types").unwrap();
        assert_eq!(types.features, ["serde"]);
    }

    /// Only the `[features]` table is read: a `[dependencies]` key is not a
    /// feature.
    #[test]
    fn only_the_features_table_is_read() {
        assert!(!labels(&tree()).iter().any(|f| f.contains("wasm")));
        let (runs, _) = doc_runs(&tree());
        assert!(!runs.iter().any(|r| r.features.iter().any(|f| f == "x")));
    }

    /// A member or a listed crate without a manifest, or one with no name, is a
    /// failure of the gate, and the other crates are still documented.
    #[test]
    fn a_missing_manifest_or_name_is_an_error() {
        let mut m = tree();
        m.files.remove("crates/tutti/Cargo.toml");
        let (runs, errors) = doc_runs(&m);
        assert_eq!(
            errors,
            [
                "FAIL: crates/tutti/Cargo.toml does not exist; was the crate moved?",
                "FAIL: crates/tutti/Cargo.toml does not exist; was the crate moved?",
            ]
        );
        assert!(runs.iter().any(|r| r.package == "tutti-types"));
        let mut m = tree();
        m.files.insert(
            "crates/plain/Cargo.toml".into(),
            "[features]\na = []\n".into(),
        );
        assert_eq!(
            doc_runs(&m).1,
            ["FAIL: crates/plain/Cargo.toml has no package name"]
        );
        let m = Mem::with(&[("Cargo.toml", "[workspace]\n")]);
        assert!(doc_runs(&m).1[0].contains("no workspace members"));
    }

    /// A union documents private items and denies warnings; a single feature
    /// denies warnings; the caller's flags win unless empty.
    #[test]
    fn a_run_is_cargo_doc_over_its_features() {
        let one = DocRun {
            package: "tutti-session".into(),
            features: vec!["audio-io".into()],
            union: false,
        };
        assert_eq!(
            one.heading(),
            "== cargo doc -p tutti-session --features audio-io"
        );
        assert_eq!(
            one.command(None).display(),
            "RUSTDOCFLAGS='-D warnings' cargo doc --locked -p tutti-session --no-deps --features audio-io"
        );
        let all = DocRun {
            package: "tutti-codec".into(),
            features: vec!["decode-wav".into(), "serde".into()],
            union: true,
        };
        assert_eq!(all.label(), "tutti-codec/*");
        assert_eq!(
            all.command(None).display(),
            "RUSTDOCFLAGS='-D warnings --document-private-items' cargo doc --locked -p tutti-codec --no-deps --features decode-wav,serde"
        );
        assert!(
            all.command(Some("-D missing_docs"))
                .display()
                .starts_with("RUSTDOCFLAGS='-D missing_docs' ")
        );
        assert!(
            all.command(Some(""))
                .display()
                .starts_with("RUSTDOCFLAGS='-D warnings --document-private-items' ")
        );
    }
}
