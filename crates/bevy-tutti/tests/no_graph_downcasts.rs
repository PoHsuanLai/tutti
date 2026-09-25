//! No code in this crate reaches into the graph for a node by type.
//!
//! `Net`'s typed node accessors (the `node` + `_as` / `_as_mut` pair) hand back
//! the graph's own copy of a node, downcast to a concrete type. This crate used to lean on them for three
//! things — a synth's MIDI port, a node's modulatable params, and a hosted
//! plugin's input slots and latency — and all three now come from components
//! captured off the unit as it is inserted (`bevy_tutti::graph::capture`).
//!
//! The downcast has to stay gone, because it is what ties a call site to one
//! graph implementation: a graph that owns its nodes outright, rather than
//! keeping a frontend clone of each, has nothing to downcast to. Neither rustc
//! nor clippy can say "not this inherent method on a vendored type, in this
//! crate only" — clippy's `disallowed_methods` lives in the workspace-wide
//! `clippy.toml`, and the node-level `Net` tests in `tutti-nodes` and
//! `tutti-sampler` use these methods legitimately — so this text scan is the
//! enforcement.
//!
//! It scans comments too. A doc example that downcasts teaches the pattern as
//! surely as code does.

use std::path::{Path, PathBuf};

/// The method-name prefix being banned, assembled so this file does not match
/// itself.
fn needle() -> String {
    ["node", "_as"].concat()
}

/// Files allowed to keep a downcast, with the exact number they may keep and
/// why.
///
/// A count rather than a path: a new downcast in an allow-listed file still
/// fails.
const ALLOWED: &[(&str, usize, &str)] = &[(
    "tests/mod_audio_rate.rs",
    2,
    "a_depth_edit_reaches_a_live_shaper and a_range_edit_reaches_a_live_clamp tick \
     the graph's own ParamShaperNode / ParamSumNode to prove the *rendered* node \
     changed, not the declaration. Neither node has a control handle that would \
     answer that (the shaper's LUT is baked at construction), so replacing the \
     read would weaken the test. They move with the native backend, which can \
     render the chain instead.",
)];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs" || e == "md") {
            out.push(path);
        }
    }
}

/// Every downcast in `src/`, `tests/`, `examples/` and the README, by file,
/// with the allow-list applied.
///
/// Mutation: appending one commented-out line to `src/plugin_host/latency.rs`
/// that calls the typed accessor on the graph (`node` + `_as_mut`, written as
/// one word) fails this, naming the file and line.
#[test]
fn no_graph_node_downcasts_outside_the_allow_list() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in ["src", "tests", "examples"] {
        rust_files(&root.join(dir), &mut files);
    }
    files.push(root.join("README.md"));
    assert!(
        files.iter().any(|f| f.ends_with("src/lib.rs")),
        "the scan must see the crate's own sources, or it passes having read nothing"
    );

    let needle = needle();
    let mut violations = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(file).unwrap();
        let hits: Vec<usize> = text
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains(&needle))
            .map(|(i, _)| i + 1)
            .collect();
        let allowed = ALLOWED
            .iter()
            .find(|(path, _, _)| *path == rel)
            .map_or(0, |(_, n, _)| *n);
        if hits.len() > allowed {
            violations.push(format!("{rel}: lines {hits:?} (allowed {allowed})"));
        }
    }

    assert!(
        violations.is_empty(),
        "graph downcasts found:\n  {}\n\nRead the node's controls from the entity \
         instead — `MidiTarget`, `ModParamsHandle`, `PluginShadow`, or a handle \
         taken from the unit before it was inserted. See `bevy_tutti::graph::capture`.",
        violations.join("\n  ")
    );
}

/// The allow-list names only files that exist and still need their exemption,
/// so it cannot outlive the reason for it.
#[test]
fn every_allow_listed_downcast_is_still_there() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let needle = needle();
    for (path, allowed, _) in ALLOWED {
        let text = std::fs::read_to_string(root.join(path))
            .unwrap_or_else(|e| panic!("allow-listed {path} is unreadable: {e}"));
        let hits = text.lines().filter(|l| l.contains(&needle)).count();
        assert_eq!(
            hits, *allowed,
            "{path} has {hits} downcasts but is allowed {allowed}: tighten the allow-list \
             when one goes"
        );
    }
}
