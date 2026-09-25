//! No code in this crate reaches into the graph for a node by type.
//!
//! `Net`'s typed node accessors (the `node` + `_as` / `_as_mut` pair) hand back
//! the graph's own copy of a node, downcast to a concrete type. This crate used
//! to lean on them for three things — a synth's MIDI port, a node's modulatable
//! params, and a hosted plugin's input slots and latency — and all three now
//! come from components captured off the unit as it is inserted
//! (`bevy_tutti::graph::capture`).
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
//! Three spellings are watched, because the typed accessor is not the only way
//! to write the downcast: `graph.0.node(id).as_any().downcast_ref::<T>()` is
//! the same thing by hand. So the scan also counts every `downcast_ref` /
//! `downcast_mut` and every raw `.0.node(` / `net.node(` access, and the
//! allow-list names the uses that are not graph downcasts.
//!
//! It scans comments too. A doc example that downcasts teaches the pattern as
//! surely as code does. This file is skipped, since it has to name what it
//! bans.

use std::path::{Path, PathBuf};

/// What is watched, as a name for messages and a line predicate.
struct Pattern {
    name: &'static str,
    matches: fn(&str) -> bool,
}

const PATTERNS: &[Pattern] = &[
    Pattern {
        name: "typed graph accessor (node_as / node_as_mut)",
        matches: |l| l.contains("node_as"),
    },
    Pattern {
        name: "downcast_ref / downcast_mut",
        // `try_downcast_ref` is `bevy_reflect`'s, on a reflected value, and
        // has nothing to do with the graph.
        matches: |l| {
            (l.contains("downcast_ref::<") || l.contains("downcast_mut::<"))
                && !l.contains("try_downcast")
        },
    },
    Pattern {
        // `.0.node(` is the spelling through the resource's field; `net.node(`
        // the one inside `AudioGraphRes`, whose `Net` arm binds it as `net`.
        name: "raw graph node access (.0.node( / net.node()",
        matches: |l| {
            l.contains(".0.node(")
                || l.contains(".0.node_mut(")
                || l.contains("net.node(")
                || l.contains("net.node_mut(")
        },
    },
];

/// `(file, pattern name, allowed count, why)`.
///
/// A count rather than a path: a new use in an allow-listed file still fails.
const ALLOWED: &[(&str, &str, usize, &str)] = &[
    (
        "tests/mod_audio_rate.rs",
        "downcast_ref / downcast_mut",
        1,
        "a_depth_edit_reaches_a_live_shaper ticks the graph's own ParamShaperNode \
         (through `AudioGraphRes::inspect`) to prove the node changed, not the \
         declaration: the shaper's LUT is baked at construction and has no control \
         handle. A depth edit rebuilds the shaper, so the inspected copy is the new \
         unit on both backends (on the native one, its shadow). \
         a_range_edit_reaches_a_live_clamp renders the chain instead, since its \
         clamp lives in a cell a native shadow does not share.",
    ),
    (
        "src/midi/endpoint/target.rs",
        "downcast_ref / downcast_mut",
        1,
        "MidiTargetRegistry's capture: a downcast of the *owned* unit before it is \
         inserted, which is the whole point of capturing — not a graph read.",
    ),
    (
        "src/modulation/target.rs",
        "downcast_ref / downcast_mut",
        1,
        "ModTargetRegistry's capture, on the owned unit before insertion.",
    ),
    (
        "src/graph/capture.rs",
        "downcast_ref / downcast_mut",
        1,
        "the PluginClient capture, on the owned unit before insertion.",
    ),
    (
        "src/graph/resources.rs",
        "raw graph node access (.0.node( / net.node()",
        3,
        "`AudioGraphRes`'s own implementation, the one place the raw graph is: \
         `inspect` hands a caller `&dyn AudioUnit` (a downcast of it is counted at \
         the caller), and two reads of `get_id()` recognise PDC delay nodes, which \
         go with `PdcDelay` itself (PR 13).",
    ),
];

fn scanned_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs" || e == "md") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    for dir in ["src", "tests", "examples"] {
        walk(&root.join(dir), &mut files);
    }
    files.push(root.join("README.md"));
    files.retain(|f| !f.ends_with("tests/no_graph_downcasts.rs"));
    files
}

fn rel(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

fn allowed(rel: &str, pattern: &str) -> usize {
    ALLOWED
        .iter()
        .find(|(path, name, _, _)| *path == rel && *name == pattern)
        .map_or(0, |(_, _, n, _)| *n)
}

/// Every watched use in `src/`, `tests/`, `examples/` and the README, by file,
/// with the allow-list applied.
///
/// Mutation: appending one commented-out line to `src/plugin_host/latency.rs`
/// that calls the typed accessor on the graph (`node` + `_as_mut`, written as
/// one word) fails this, naming the file and line; so does a line reading
/// `graph.0.node(id).as_any().downcast_ref::<PluginClient>()`, on two patterns.
#[test]
fn no_graph_node_downcasts_outside_the_allow_list() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = scanned_files(root);
    assert!(
        files.iter().any(|f| f.ends_with("src/lib.rs")),
        "the scan must see the crate's own sources, or it passes having read nothing"
    );

    let mut violations = Vec::new();
    for file in &files {
        let rel = rel(root, file);
        let text = std::fs::read_to_string(file).unwrap();
        for pattern in PATTERNS {
            let hits: Vec<usize> = text
                .lines()
                .enumerate()
                .filter(|(_, l)| (pattern.matches)(l))
                .map(|(i, _)| i + 1)
                .collect();
            let allowed = allowed(&rel, pattern.name);
            if hits.len() > allowed {
                violations.push(format!(
                    "{rel}: {} at lines {hits:?} (allowed {allowed})",
                    pattern.name
                ));
            }
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

/// The allow-list names only uses that still exist, so it cannot outlive the
/// reason for it.
#[test]
fn every_allow_listed_use_is_still_there() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (path, name, allowed, _) in ALLOWED {
        let pattern = PATTERNS
            .iter()
            .find(|p| p.name == *name)
            .unwrap_or_else(|| panic!("allow-list names unknown pattern {name:?}"));
        let text = std::fs::read_to_string(root.join(path))
            .unwrap_or_else(|e| panic!("allow-listed {path} is unreadable: {e}"));
        let hits = text.lines().filter(|l| (pattern.matches)(l)).count();
        assert_eq!(
            hits, *allowed,
            "{path} has {hits} of {name} but is allowed {allowed}: tighten the \
             allow-list when one goes"
        );
    }
}
