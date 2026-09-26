//! fundsp's `Net` stays out of this crate's non-test code.
//!
//! Design doc 013, Phase 3 PR 13 deleted bevy-tutti's `Net` runtime; the
//! native graph is its only one. PR 15 retired the test-only `Net`-era
//! oracles that were left (in what is now `tests/scene_render.rs`,
//! `engine::build`'s `engine_tests` and `tests/export_fork.rs`), so nothing
//! in the crate names `Net` any more, tests included; the cut below stays
//! so a test module may name it, to say what replaced it.
//! Nothing stops a new `use tutti_core::dsp::Net` in `src/` from compiling,
//! and clippy's `disallowed_types` lives in the workspace-wide `clippy.toml`,
//! where the engine crates still use `Net` legitimately. So this text scan is
//! the enforcement, in the style of `no_graph_downcasts.rs`.
//!
//! **What counts as test code:** everything in a file from its first
//! `#[cfg(test)]` that opens a module onward (the crate keeps its test
//! modules at the end of a file), and comment lines, which name `Net` to say
//! what replaced it.

use std::path::{Path, PathBuf};

/// A code line that names fundsp's graph or its audio half.
fn names_net(line: &str) -> bool {
    [
        "dsp::Net",
        "dsp::{Net",
        "NetBackend",
        "Net::new",
        "Net::with_backend",
        "net_fade",
    ]
    .iter()
    .any(|p| line.contains(p))
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `file`'s non-test code lines, numbered from 1.
fn code_lines(text: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = text.lines().collect();
    let end = lines
        .windows(2)
        .position(|w| w[0].trim() == "#[cfg(test)]" && w[1].trim_start().starts_with("mod "))
        .unwrap_or(lines.len());
    lines[..end]
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
        .map(|(i, l)| (i + 1, *l))
        .collect()
}

/// **No `Net` in `src/` outside its test modules.**
///
/// Mutation (run): adding `use tutti_core::dsp::Net;` to `src/graph/mod.rs`
/// (or `pub use tutti_core::dsp::Net;` to `src/engine/mod.rs`, the re-export
/// PR 13 removed) fails this, naming the file and line.
#[test]
fn no_net_in_the_crates_code() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    assert!(
        files.iter().any(|f| f.ends_with("src/lib.rs")),
        "the scan must see the crate's own sources, or it passes having read nothing"
    );

    let mut found = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        for (line, code) in code_lines(&text) {
            if names_net(code) {
                found.push(format!(
                    "{}:{line}: {}",
                    file.strip_prefix(root).unwrap().display(),
                    code.trim()
                ));
            }
        }
    }
    assert!(
        found.is_empty(),
        "fundsp's `Net` in bevy-tutti's code (doc 013, PR 13 removed it; the \
         graph is `AudioGraphRes`, the native runtime):\n  {}",
        found.join("\n  ")
    );
}

/// **The scan sees test code as test code, and nothing else**: a code line
/// that names `Net` before a file's test module is found, the same line
/// after the cut is not, and neither is a comment, so the check above is not
/// passing for want of a pattern that matches.
///
/// It read `engine::build`'s `Net`-era oracle until doc 013 PR 15 removed
/// it; a file that names `Net` is written here instead.
///
/// Mutation (run): `code_lines` not cutting at the test module → this fails.
#[test]
fn the_cut_is_at_the_test_modules() {
    let text = "use tutti_core::dsp::Net;\n\
                // Net::new is gone\n\
                fn f() {}\n\
                #[cfg(test)]\n\
                mod tests {\n\
                    use tutti_core::dsp::Net;\n\
                }\n";
    assert!(
        text.lines().filter(|l| names_net(l)).count() >= 2,
        "the pattern is live"
    );
    let hits: Vec<usize> = code_lines(text)
        .iter()
        .filter(|(_, l)| names_net(l))
        .map(|(i, _)| *i)
        .collect();
    assert_eq!(hits, vec![1], "only the line before the cut");
}
