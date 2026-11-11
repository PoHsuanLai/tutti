//! `check-layout`: the repo layout and the crate layering, read from the
//! workspace's manifests (no build, no `cargo metadata`; the C-library check is
//! the one place a `cargo tree` is asked, through [`Cargo`]).
//!
//! Layout:
//! - every workspace member under `crates/` is directly `crates/<package-name>`,
//!   a nested directory is refused, and so is a directory under `crates/` or
//!   `testing/` that is not a member (the parked crates in [`PARKED`] are their
//!   own `[workspace]` root, out of the member list on purpose);
//! - every member under `testing/` is directly `testing/<dir>`;
//! - no member lives anywhere else but `xtask`, whose manifest has no dependency
//!   at all (std only) and which nothing depends on;
//! - nothing under `crates/` has a normal or build dependency on `testing/`.
//!
//! Layering (CLAUDE.md, "Workspace"): a normal or build dependency between
//! workspace crates points strictly down [`LAYERS`]; a crate missing from the
//! table is an error, because placing it is the point. Cargo already refuses a
//! cycle; it does not refuse a dependency that points sideways or up. On top of
//! that [`ONLY`] fixes the whole dependency set of the crates whose purity is
//! the point (a layer alone would let a layer-1 crate depend on any layer-0
//! one), and a few named bans follow in [`check`].
//!
//! The codec libraries are `tutti-codec`'s alone, and `winnow` is named in
//! [`WINNOW_HOMES`]'s parser modules alone; in `tutti-codec`'s manifest a
//! `decode-*` feature enables no encoder library and an `encode-*` feature no
//! `symphonia`. `export` alone on `tutti-session`, `tutti` and `bevy-tutti`
//! (while it is a member) reaches no C library: `cargo tree -i ogg_next_sys`
//! finds nothing, and with `export-ogg` it still does, so the check cannot pass
//! by looking at nothing.
//!
//! A rule the layers already state is not stated twice: `tutti-types`,
//! `tutti-plugin-ipc-ffi` and every other layer-0 crate depend on no workspace
//! crate because nothing sits below layer 0.

use super::toml::{Kind, Toml, strings};
use super::{Cargo, Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

const SKIPPED_DIRS: &[&str] = &["target", ".git"];

fn skipped(dir: &str) -> bool {
    SKIPPED_DIRS.contains(&dir)
}

/// Lowest first. A normal or build dependency must land in a strictly lower layer.
pub(super) const LAYERS: &[&[&str]] = &[
    &[
        "arc-lend",
        "tutti-alsa-ffi",
        "tutti-coremidi-ffi",
        "tutti-ump",
        "tutti-types",
        "tutti-plugin-ipc-ffi",
    ],
    &[
        "tutti-rt",
        "tutti-midi-protocol",
        "tutti-midi-device",
        "tutti-curve",
    ],
    &[
        "tutti-graph",
        "tutti-dsp",
        "tutti-modulation",
        "tutti-metering",
        "tutti-codec",
        "tutti-tempo",
        "tutti-voicing",
        "tutti-sf2",
    ],
    &["tutti-transport", "tutti-plugin-types", "tutti-measure"],
    &[
        "tutti-engine",
        "tutti-nodes",
        "tutti-midi-runtime",
        "tutti-midi-file",
        "tutti-clap-ffi",
        "tutti-vst3-ffi",
        "tutti-au-ffi",
        "tutti-vst2-ffi",
        "tutti-plugin-db",
        "tutti-plugin-ipc",
    ],
    &[
        "tutti-cpal",
        "tutti-au-host",
        "tutti-export",
        "tutti-sampler",
        "tutti-polysynth",
        "tutti-soundfont",
        "tutti-midi-hardware",
        "tutti-clap-host",
        "tutti-vst3-host",
        "tutti-vst2-host",
        "tutti-plugin-host",
    ],
    &["tutti-plugin", "tutti-plugin-probe"],
    &["tutti-plugin-server", "tutti-session"],
    // `bevy-tutti` is parked, so never a member here; its place stays so that it
    // is ranked, and its edges checked, the day it is back.
    &["tutti", "bevy-tutti"],
];

/// Crates kept in the tree and out of the workspace: each its own `[workspace]` root.
const PARKED: &[&str] = &["crates/bevy-tutti"];

/// The crates whose normal and build dependencies on workspace crates are exactly these.
const ONLY: &[(&str, &[&str])] = &[
    ("tutti-rt", &["arc-lend"]),
    ("tutti-midi-protocol", &["tutti-ump"]),
    ("tutti-midi-device", &["tutti-ump"]),
    ("tutti-metering", &["tutti-types"]),
    ("tutti-codec", &["tutti-types"]),
    ("tutti-tempo", &["tutti-types"]),
    (
        "tutti-voicing",
        &["tutti-ump", "tutti-midi-protocol", "tutti-types"],
    ),
    (
        "tutti-transport",
        &["tutti-types", "tutti-rt", "tutti-graph", "tutti-tempo"],
    ),
    ("tutti-plugin-db", &["tutti-plugin-types"]),
    (
        "tutti-plugin-ipc",
        &[
            "tutti-types",
            "tutti-rt",
            "tutti-ump",
            "tutti-plugin-types",
            "tutti-plugin-ipc-ffi",
        ],
    ),
    (
        "tutti-plugin-host",
        &[
            "tutti-plugin-types",
            "tutti-graph",
            "tutti-transport",
            "tutti-tempo",
            "tutti-types",
            "tutti-rt",
            "tutti-ump",
        ],
    ),
    (
        "tutti-plugin-probe",
        &[
            "tutti-plugin-types",
            "tutti-clap-host",
            "tutti-vst3-host",
            "tutti-vst2-host",
            "tutti-au-host",
        ],
    ),
    ("tutti-dsp", &["tutti-types"]),
    ("tutti-curve", &["tutti-types"]),
    ("tutti-modulation", &["tutti-types", "tutti-curve"]),
    ("tutti-measure", &["tutti-types", "tutti-dsp"]),
    (
        "tutti-graph",
        &["tutti-types", "tutti-rt", "tutti-ump", "tutti-curve"],
    ),
];

/// Depend on no workspace crate, of any kind (a dev-dependency too).
const ALONE: &[&str] = &["arc-lend", "tutti-ump"];

const CODEC_LIBS: &[&str] = &["symphonia", "flacenc", "aifc", "vorbis_rs"];
const ENCODERS: &[&str] = &["flacenc", "aifc", "vorbis_rs", "md-5"];

/// A crate that names `winnow`, and the parser modules that may.
pub(super) const WINNOW_HOMES: &[(&str, &[&str])] = &[(
    "tutti-midi-file",
    &[
        "crates/tutti-midi-file/src/smf/read.rs",
        "crates/tutti-midi-file/src/clip_file/read.rs",
    ],
)];

/// The crates that can reach a C library through `export-ogg`.
const OGG_PACKAGES: &[&str] = &["tutti-session", "tutti", "bevy-tutti"];

struct Pkg {
    name: String,
    dir: String,
    toml: Toml,
    deps: Vec<(String, Kind)>,
}

fn layer_of(name: &str) -> Option<usize> {
    LAYERS.iter().position(|l| l.contains(&name))
}

/// The workspace members' manifests, from the root's `members` list.
fn packages(tree: &dyn Tree, bad: &mut Vec<String>) -> Vec<Pkg> {
    let root = Toml::parse(&tree.read("Cargo.toml").unwrap_or_default());
    let members = root
        .value("workspace", "members")
        .map(strings)
        .unwrap_or_default();
    if members.is_empty() {
        bad.push("Cargo.toml: no workspace members found; the scan is broken".into());
    }
    let mut out = Vec::new();
    for dir in members {
        let Some(text) = tree.read(&format!("{dir}/Cargo.toml")) else {
            bad.push(format!("{dir}: a workspace member with no Cargo.toml"));
            continue;
        };
        let toml = Toml::parse(&text);
        let Some(name) = toml
            .value("package", "name")
            .and_then(|v| strings(v).into_iter().next())
        else {
            bad.push(format!("{dir}/Cargo.toml: no package name"));
            continue;
        };
        let deps = toml.dependencies();
        out.push(Pkg {
            name,
            dir,
            toml,
            deps,
        });
    }
    out
}

pub(super) fn check(tree: &dyn Tree, cargo: &dyn Cargo) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let pkgs = packages(tree, &mut bad);
    let by_name: BTreeMap<&str, &Pkg> = pkgs.iter().map(|p| (p.name.as_str(), p)).collect();

    // Layout.
    let mut member_dirs: BTreeSet<&str> = BTreeSet::new();
    for p in &pkgs {
        member_dirs.insert(&p.dir);
        let parts: Vec<&str> = p.dir.split('/').collect();
        match parts[0] {
            "crates" if parts.len() != 2 => bad.push(format!(
                "{}: {} is a nested directory under crates/; members sit directly in crates/",
                p.name, p.dir
            )),
            "crates" if parts[1] != p.name => {
                bad.push(format!("{}: {} must be crates/{}", p.name, p.dir, p.name));
            }
            "testing" if parts.len() != 2 => bad.push(format!(
                "{}: {} is a nested directory under testing/; members sit directly in testing/",
                p.name, p.dir
            )),
            "crates" | "testing" => {}
            _ if p.dir == "xtask" && p.name == "xtask" => {}
            _ => bad.push(format!(
                "{}: {} is outside crates/ and testing/ (vendor/ holds third-party code, never a member; xtask is the one exception)",
                p.name, p.dir
            )),
        }
    }
    for top in ["crates", "testing"] {
        let mut entries: BTreeSet<String> = BTreeSet::new();
        for f in tree.files(top, &skipped) {
            if let Some(e) = f[top.len() + 1..].split_once('/') {
                entries.insert(format!("{top}/{}", e.0));
            }
        }
        for dir in entries {
            if member_dirs.contains(dir.as_str()) {
                continue;
            }
            if PARKED.contains(&dir.as_str()) {
                let manifest = tree.read(&format!("{dir}/Cargo.toml")).unwrap_or_default();
                if !manifest.contains("\n[workspace]") {
                    bad.push(format!(
                        "{dir} is parked and must be its own `[workspace]` root"
                    ));
                }
                continue;
            }
            bad.push(format!("{dir} is not a workspace member"));
        }
    }

    let internal = |p: &Pkg| -> Vec<(String, Kind)> {
        p.deps
            .iter()
            .filter(|(d, _)| by_name.contains_key(d.as_str()) && *d != p.name)
            .cloned()
            .collect()
    };
    let dir_of = |n: &str| by_name[n].dir.as_str();

    // xtask stands alone.
    match by_name.get("xtask") {
        Some(x) => {
            if !x.deps.is_empty() {
                bad.push("xtask has dependencies; it is std only".into());
            }
            for (dep, kind) in internal(x) {
                bad.push(format!(
                    "xtask: {} dependency on {dep}; xtask depends on no workspace crate",
                    kind.label()
                ));
            }
            for p in &pkgs {
                if p.name != "xtask" && internal(p).iter().any(|(d, _)| d == "xtask") {
                    bad.push(format!(
                        "{} depends on xtask; nothing depends on it",
                        p.name
                    ));
                }
            }
        }
        None => bad.push("xtask is not a workspace member".into()),
    }

    // crates/ never builds on testing/.
    for p in pkgs.iter().filter(|p| p.dir.starts_with("crates/")) {
        for (dep, kind) in internal(p) {
            if kind != Kind::Dev && dir_of(&dep).starts_with("testing/") {
                bad.push(format!(
                    "{}: {} dependency on {dep}; crates/ depends on testing/",
                    p.name,
                    kind.label()
                ));
            }
        }
    }

    // Layering.
    for p in pkgs.iter().filter(|p| p.dir.starts_with("crates/")) {
        let Some(mine) = layer_of(&p.name) else {
            bad.push(format!(
                "{} is not in LAYERS; place it (xtask/src/gates/layout.rs)",
                p.name
            ));
            continue;
        };
        for (dep, kind) in internal(p) {
            let Some(theirs) = layer_of(&dep) else {
                continue;
            };
            if kind != Kind::Dev && theirs >= mine {
                bad.push(format!(
                    "{} -> {dep} ({}) points up or sideways (layer {mine} -> {theirs}); dependencies point down",
                    p.name,
                    kind.label()
                ));
            }
        }
    }

    // The codec libraries stay behind tutti-codec.
    for p in pkgs.iter().filter(|p| p.name != "tutti-codec") {
        for (dep, kind) in &p.deps {
            if CODEC_LIBS.contains(&dep.as_str()) && *kind != Kind::Dev {
                bad.push(format!(
                    "{dep} is a {} dependency of {}; the codec libraries are tutti-codec's alone",
                    kind.label(),
                    p.name
                ));
            }
        }
    }

    // `winnow` is a parser module's alone.
    let homes: Vec<&str> = WINNOW_HOMES.iter().map(|(c, _)| *c).collect();
    for p in &pkgs {
        for (dep, kind) in &p.deps {
            if dep == "winnow" && *kind != Kind::Dev && !homes.contains(&p.name.as_str()) {
                bad.push(format!(
                    "winnow is a {} dependency of {}; only {} may name it, in their listed parser modules",
                    kind.label(),
                    p.name,
                    homes.join(", ")
                ));
            }
        }
    }
    for (krate, modules) in WINNOW_HOMES {
        let dir = format!("crates/{krate}/src");
        for path in tree.files(&dir, &skipped) {
            if !path.ends_with(".rs") || modules.contains(&path.as_str()) {
                continue;
            }
            let text = tree.read(&path).unwrap_or_default();
            if super::lex::blank(&text).contains("winnow") {
                bad.push(format!(
                    "{path} names winnow outside {}",
                    modules.join(", ")
                ));
            }
        }
    }

    // A decoder's features pull no encoder, an encoder's no decoder.
    if let Some(codec) = by_name.get("tutti-codec")
        && let Some(features) = codec.toml.section("features")
    {
        for (feat, value) in &features.entries {
            let names: BTreeSet<String> = strings(value)
                .iter()
                .map(|e| {
                    let head = e.split('/').next().unwrap_or(e);
                    let head = head.strip_prefix("dep:").unwrap_or(head);
                    head.strip_suffix('?').unwrap_or(head).to_owned()
                })
                .collect();
            let hit: Vec<&str> = ENCODERS
                .iter()
                .copied()
                .filter(|e| names.contains(*e))
                .collect();
            if feat.starts_with("decode-") && !hit.is_empty() {
                bad.push(format!(
                    "{feat} enables an encoder library ({hit:?}); a build that reads pulls no encoder"
                ));
            }
            if feat.starts_with("encode-") && names.contains("symphonia") {
                bad.push(format!(
                    "{feat} enables symphonia; a build that writes pulls no decoder"
                ));
            }
        }
    }

    // `export` alone pulls no C library.
    for pkg in OGG_PACKAGES.iter().filter(|p| by_name.contains_key(*p)) {
        let reaches = |features: &str| {
            cargo.tree(&[
                "-p",
                pkg,
                "-e",
                "normal",
                "-i",
                "ogg_next_sys",
                "--features",
                features,
                "--no-default-features",
            ])
        };
        if reaches("export").is_ok_and(|o| o.contains("ogg_next_sys")) {
            bad.push(format!(
                "{pkg} with `export` alone reaches ogg_next_sys; only `export-ogg` may"
            ));
        }
        match reaches("export,export-ogg") {
            Ok(o) if o.contains("ogg_next_sys") => {}
            Ok(_) => bad.push(format!(
                "{pkg} with `export,export-ogg` no longer reaches ogg_next_sys; the check sees nothing ()"
            )),
            Err(e) => {
                let e: String = e.trim().chars().take(200).collect();
                bad.push(format!(
                    "{pkg} with `export,export-ogg` no longer reaches ogg_next_sys; the check sees nothing ({e})"
                ));
            }
        }
    }

    // Whole dependency sets.
    let deps_of = |name: &str, kinds: &[Kind]| -> BTreeSet<String> {
        by_name
            .get(name)
            .map(|p| {
                internal(p)
                    .into_iter()
                    .filter(|(_, k)| kinds.contains(k))
                    .map(|(d, _)| d)
                    .collect()
            })
            .unwrap_or_default()
    };
    let all = [Kind::Normal, Kind::Build, Kind::Dev];
    let built = [Kind::Normal, Kind::Build];
    for name in ALONE.iter().filter(|n| by_name.contains_key(*n)) {
        let extra = deps_of(name, &all);
        if !extra.is_empty() {
            bad.push(format!(
                "{name} may depend on no workspace crate; it depends on {extra:?}"
            ));
        }
    }
    for (name, allowed) in ONLY.iter().filter(|(n, _)| by_name.contains_key(n)) {
        let extra: BTreeSet<String> = deps_of(name, &built)
            .into_iter()
            .filter(|d| !allowed.contains(&d.as_str()))
            .collect();
        if !extra.is_empty() {
            let mut sorted = allowed.to_vec();
            sorted.sort_unstable();
            bad.push(format!(
                "{name} may depend on {sorted:?} only; it depends on {extra:?}"
            ));
        }
    }
    // The process edge knows the wire's vocabulary and no plugin format, and no
    // format host knows it: a format is served through it, never built on it.
    if by_name.contains_key("tutti-plugin-ipc") {
        for p in &pkgs {
            if p.name.starts_with("tutti-")
                && p.name.ends_with("-host")
                && deps_of(&p.name, &all).contains("tutti-plugin-ipc")
            {
                bad.push(format!(
                    "{} depends on tutti-plugin-ipc; a format host knows no hosting",
                    p.name
                ));
            }
        }
    }
    // The format-generic half of hosting names no facade above it, and the probe
    // binary none either, nor does anything depend on it (a binary).
    for (name, banned) in [
        ("tutti-plugin-host", "tutti-plugin"),
        ("tutti-plugin-probe", "tutti-plugin"),
    ] {
        if deps_of(name, &all).contains(banned) {
            bad.push(format!(
                "{name} depends on {banned}, even as a dev-dependency"
            ));
        }
    }
    for p in &pkgs {
        if deps_of(&p.name, &all).contains("tutti-plugin-probe") {
            bad.push(format!(
                "{} depends on tutti-plugin-probe; nothing does, it is a binary",
                p.name
            ));
        }
        let users: BTreeSet<String> = internal(p).into_iter().map(|(d, _)| d).collect();
        if users.contains("tutti-session") && !["tutti", "bevy-tutti"].contains(&p.name.as_str()) {
            bad.push(format!(
                "{} depends on tutti-session; only tutti and bevy-tutti do",
                p.name
            ));
        }
        for top in ["tutti", "bevy-tutti"] {
            if users.contains(top) {
                bad.push(format!("{} depends on {top}, the top of the tree", p.name));
            }
        }
    }

    super::decode_boundary::check(tree, &mut bad);
    super::test_paths::check(tree, &mut bad);

    let mut r = Report::default();
    if bad.is_empty() {
        let ranked: usize = LAYERS.iter().map(|l| l.len()).sum();
        r.say(format!(
            "check-layout: ok ({} members, {ranked} ranked)",
            pkgs.len()
        ));
        r.say("check-layout: no depth-dependent test paths");
    } else {
        r.fail("check-layout: FAIL");
        for b in bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::{Mem, NoCargo, OggCargo};
    use super::*;

    /// A workspace of `(dir, name, manifest body)` members.
    fn ws(members: &[(&str, &str, &str)]) -> Mem {
        let list: Vec<String> = members
            .iter()
            .map(|(d, ..)| format!("    \"{d}\","))
            .collect();
        let mut m = Mem::with(&[(
            "Cargo.toml",
            &format!(
                "[workspace]\nmembers = [\n{}\n]\nexclude = [\"crates/bevy-tutti\"]\n",
                list.join("\n")
            ),
        )]);
        for (dir, name, body) in members {
            m.files.insert(
                format!("{dir}/Cargo.toml"),
                format!("[package]\nname = \"{name}\"\n{body}"),
            );
            m.files.insert(format!("{dir}/src/lib.rs"), String::new());
        }
        m
    }

    fn base() -> Vec<(&'static str, &'static str, &'static str)> {
        vec![
            ("xtask", "xtask", "[dependencies]\n"),
            ("crates/tutti-types", "tutti-types", ""),
            ("crates/tutti-ump", "tutti-ump", ""),
            (
                "crates/tutti-rt",
                "tutti-rt",
                "[dependencies]\narc-lend = { path = \"../arc-lend\" }\n",
            ),
            ("crates/arc-lend", "arc-lend", ""),
            (
                "crates/tutti-graph",
                "tutti-graph",
                "[dependencies]\ntutti-types.path = \"../tutti-types\"\n",
            ),
            ("testing/tutti-fixtures", "tutti-fixtures", ""),
        ]
    }

    fn run(members: &[(&str, &str, &str)]) -> Report {
        check(&ws(members), &NoCargo)
    }

    fn with(extra: (&'static str, &'static str, &'static str)) -> Report {
        let mut v = base();
        v.retain(|m| m.1 != extra.1);
        v.push(extra);
        run(&v)
    }

    #[test]
    fn a_layered_workspace_passes() {
        let r = run(&base());
        assert!(r.passed(), "{:?}", r.findings);
        assert_eq!(r.out[0], "check-layout: ok (7 members, 48 ranked)");
    }

    #[test]
    fn a_dependency_that_points_up_or_sideways_fails() {
        let r = with((
            "crates/tutti-types",
            "tutti-types",
            "[dependencies]\ntutti-graph = { path = \"../tutti-graph\" }\n",
        ));
        assert!(r.findings.iter().any(|l| {
            l.contains("tutti-types -> tutti-graph (normal) points up or sideways (layer 0 -> 2)")
        }));
        // A dev-dependency may point anywhere, except where a crate is held to none.
        let r = with((
            "crates/tutti-types",
            "tutti-types",
            "[dev-dependencies]\ntutti-graph = { path = \"../tutti-graph\" }\n",
        ));
        assert!(r.passed());
        let r = with((
            "crates/tutti-ump",
            "tutti-ump",
            "[dev-dependencies]\ntutti-types = { path = \"../tutti-types\" }\n",
        ));
        assert!(r.findings[1].contains("tutti-ump may depend on no workspace crate"));
    }

    #[test]
    fn a_crate_not_in_layers_and_a_wrong_directory_fail() {
        let mut v = base();
        v.push(("crates/tutti-new", "tutti-new", ""));
        assert!(run(&v).findings[1].contains("tutti-new is not in LAYERS"));
        let mut v = base();
        v.push(("crates/modx", "tutti-curve", ""));
        assert!(
            run(&v)
                .findings
                .iter()
                .any(|l| l.contains("crates/modx must be crates/tutti-curve"))
        );
        let mut v = base();
        v.push(("crates/nest/tutti-curve", "tutti-curve", ""));
        assert!(
            run(&v)
                .findings
                .iter()
                .any(|l| l.contains("nested directory under crates/"))
        );
    }

    #[test]
    fn a_directory_that_is_not_a_member_fails_unless_it_is_parked() {
        let mut m = ws(&base());
        m.files
            .insert("crates/stray/Cargo.toml".into(), String::new());
        assert!(check(&m, &NoCargo).findings[1].contains("crates/stray is not a workspace member"));
        let mut m = ws(&base());
        m.files.insert(
            "crates/bevy-tutti/Cargo.toml".into(),
            "[package]\nname = \"bevy-tutti\"\n".into(),
        );
        assert!(
            check(&m, &NoCargo).findings[1]
                .contains("crates/bevy-tutti is parked and must be its own")
        );
        m.files.insert(
            "crates/bevy-tutti/Cargo.toml".into(),
            "[package]\n\n[workspace]\n".into(),
        );
        assert!(check(&m, &NoCargo).passed());
    }

    #[test]
    fn crates_do_not_build_on_testing_and_xtask_stands_alone() {
        let r = with((
            "crates/tutti-graph",
            "tutti-graph",
            "[build-dependencies]\ntutti-fixtures = { path = \"../../testing/tutti-fixtures\" }\n",
        ));
        assert!(r.findings.iter().any(|l| {
            l.contains("build dependency on tutti-fixtures; crates/ depends on testing/")
        }));
        let r = with((
            "xtask",
            "xtask",
            "[dependencies]\narc-lend = { path = \"../crates/arc-lend\" }\n",
        ));
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("xtask has dependencies; it is std only"))
        );
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("xtask: normal dependency on arc-lend"))
        );
        let r = with((
            "crates/tutti-ump",
            "tutti-ump",
            "[dev-dependencies]\nxtask = { path = \"../../xtask\" }\n",
        ));
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("tutti-ump depends on xtask"))
        );
    }

    #[test]
    fn a_crate_held_to_its_set_fails_on_one_more() {
        let r = with((
            "crates/tutti-graph",
            "tutti-graph",
            "[dependencies]\ntutti-types.path = \"x\"\ntutti-rt = { path = \"x\" }\ntutti-curve = { path = \"x\" }\n",
        ));
        assert!(
            r.findings.iter().all(|l| !l.contains("may depend on")),
            "{:?}",
            r.findings
        );
        let mut v = base();
        v.retain(|m| m.1 != "tutti-rt");
        v.push((
            "crates/tutti-rt",
            "tutti-rt",
            "[dependencies]\narc-lend = { path = \"x\" }\ntutti-types = { path = \"x\" }\n",
        ));
        let r = run(&v);
        assert!(r.findings.iter().any(|l| l.contains(
            "tutti-rt may depend on [\"arc-lend\"] only; it depends on {\"tutti-types\"}"
        )));
    }

    #[test]
    fn the_codec_libraries_and_winnow_have_homes() {
        let r = with((
            "crates/tutti-graph",
            "tutti-graph",
            "[dependencies]\nsymphonia = \"0.5\"\nwinnow = \"1\"\n",
        ));
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("symphonia is a normal dependency of tutti-graph"))
        );
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("winnow is a normal dependency of tutti-graph"))
        );
        let r = with((
            "crates/tutti-graph",
            "tutti-graph",
            "[dev-dependencies]\nsymphonia = \"0.5\"\n",
        ));
        assert!(r.passed());
        let mut m = ws(&base());
        m.files.insert(
            "crates/tutti-midi-file/src/lib.rs".into(),
            "// winnow\nuse winnow::Parser;\n".into(),
        );
        assert!(
            check(&m, &NoCargo)
                .findings
                .iter()
                .any(|l| l.contains("crates/tutti-midi-file/src/lib.rs names winnow outside"))
        );
        m.files.insert(
            "crates/tutti-midi-file/src/lib.rs".into(),
            "// winnow\n".into(),
        );
        m.files.insert(
            "crates/tutti-midi-file/src/smf/read.rs".into(),
            "use winnow::Parser;\n".into(),
        );
        assert!(
            check(&m, &NoCargo)
                .findings
                .iter()
                .all(|l| !l.contains("winnow"))
        );
    }

    #[test]
    fn codec_features_keep_readers_and_writers_apart() {
        let features = "[features]\ndecode-ogg = [\n  \"dep:symphonia\",\n  \"dep:vorbis_rs\",\n]\nencode-wav = [\"symphonia?/wav\"]\nencode-flac = [\"dep:flacenc\"]\n";
        let mut v = base();
        v.push(("crates/tutti-codec", "tutti-codec", features));
        let r = run(&v);
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("decode-ogg enables an encoder library ([\"vorbis_rs\"])"))
        );
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("encode-wav enables symphonia"))
        );
        assert_eq!(
            r.findings.iter().filter(|l| l.contains("enables")).count(),
            2
        );
    }

    #[test]
    fn export_alone_reaches_no_c_library_and_export_ogg_still_does() {
        let mut v = base();
        v.push(("crates/tutti-session", "tutti-session", ""));
        let m = ws(&v);
        // The tree the check expects: ogg only through `export-ogg`.
        let r = check(
            &m,
            &OggCargo {
                export_reaches: false,
                ogg_reaches: true,
            },
        );
        assert!(r.passed(), "{:?}", r.findings);
        let r = check(
            &m,
            &OggCargo {
                export_reaches: true,
                ogg_reaches: true,
            },
        );
        assert!(r.findings[1].contains("tutti-session with `export` alone reaches ogg_next_sys"));
        let r = check(
            &m,
            &OggCargo {
                export_reaches: false,
                ogg_reaches: false,
            },
        );
        assert!(r.findings[1].contains("no longer reaches ogg_next_sys"));
    }

    #[test]
    fn the_session_and_the_top_have_few_users_and_hosts_know_no_hosting() {
        let r = with((
            "crates/tutti-graph",
            "tutti-graph",
            "[dev-dependencies]\ntutti-session = { path = \"x\" }\n",
        ));
        let mut v = base();
        v.push(("crates/tutti-session", "tutti-session", ""));
        v.retain(|m| m.1 != "tutti-graph");
        v.push((
            "crates/tutti-graph",
            "tutti-graph",
            "[dev-dependencies]\ntutti-session = { path = \"x\" }\n",
        ));
        assert!(run(&v).findings.iter().any(|l| {
            l.contains("tutti-graph depends on tutti-session; only tutti and bevy-tutti do")
        }));
        let _ = r;
        let mut v = base();
        v.push(("crates/tutti-plugin-ipc", "tutti-plugin-ipc", ""));
        v.push((
            "crates/tutti-clap-host",
            "tutti-clap-host",
            "[dev-dependencies]\ntutti-plugin-ipc = { path = \"x\" }\n",
        ));
        assert!(run(&v).findings.iter().any(|l| l.contains(
            "tutti-clap-host depends on tutti-plugin-ipc; a format host knows no hosting"
        )));
    }
}
