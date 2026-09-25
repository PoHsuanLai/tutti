//! `Engine` renders the native graph only: fundsp's `NetBackend` stays out
//! of this crate's code.
//!
//! Design doc 013, Phase 3 PR 15 removed `Engine::new(MotionFsm, NetBackend)`,
//! the `Backend::Net` arm and `render_net`; `Engine::new` takes a
//! `tutti_graph::Executor`. `Net` itself is still here, legitimately, until
//! Phase 5: `topology::compile` builds one and `dsp::Net` re-exports it for
//! the nodes' own tests. What must not come back is the engine running one,
//! and the only way in is its audio half, so that is what this scans for.
//! Nothing else stops a new `use fundsp::realnet::NetBackend` from compiling,
//! and clippy's `disallowed_types` is workspace-wide, where fundsp's own
//! crate still names the type. So this text scan is the enforcement, in the
//! style of bevy-tutti's `tests/no_net.rs`.
//!
//! Comment lines are skipped: they name `NetBackend` to say what replaced
//! it. Test modules are **not** skipped: nothing in this crate's `src/` has a
//! reason to build a `NetBackend`, tests included.

use std::path::{Path, PathBuf};

/// A code line that names the `Net`'s audio half or the engine's old arm.
fn names_net_backend(line: &str) -> bool {
    [
        "NetBackend",
        "realnet",
        "Backend::Net",
        "render_net",
        // The method and its path form (`Net::backend(net)`, UFCS), which
        // `.backend()` alone does not see.
        ".backend()",
        "::backend(",
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

/// `text`'s code lines (comments skipped), numbered from 1.
fn code_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
        .map(|(i, l)| (i + 1, l))
}

/// **No `NetBackend` anywhere in `src/`.**
///
/// Mutation (run): adding `pub use fundsp::realnet::NetBackend;` back to
/// `src/lib.rs` (the re-export PR 15 removed) fails this, naming the file and
/// line; so does a `Backend::Net(..)` arm in `src/engine.rs`.
#[test]
fn no_net_backend_in_the_crates_code() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    for must in ["src/lib.rs", "src/engine.rs"] {
        assert!(
            files.iter().any(|f| f.ends_with(must)),
            "the scan must see {must}, or it passes having read nothing"
        );
    }

    let mut found = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        for (line, code) in code_lines(&text) {
            if names_net_backend(code) {
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
        "fundsp's `NetBackend` in tutti-core's code (doc 013, PR 15 removed \
         the engine's `Net` backend; `Engine::new` takes the native graph):\n  {}",
        found.join("\n  ")
    );
}

/// **The scan skips comments and nothing else**: each pattern is live on a
/// code line, and dead on a comment line, so the check above is not passing
/// for want of a pattern that matches.
///
/// Mutation (run): `code_lines` keeping comment lines → this fails (and the
/// check above fails on the docs in `src/lib.rs` that name `NetBackend`).
/// Mutation (run): the `::backend(` pattern removed → the UFCS line on the
/// fifth line is missed → fails.
#[test]
fn the_scan_skips_only_comments() {
    let text = "use fundsp::realnet::NetBackend;\n    Backend::Net(net) => {}\n    \
                // NetBackend is gone\n    /// `render_net` was here\n    \
                let b = Box::new(dsp::Net::backend(net));\n";
    let hits: Vec<usize> = code_lines(text)
        .filter(|(_, l)| names_net_backend(l))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(hits, vec![1, 2, 5]);
}
