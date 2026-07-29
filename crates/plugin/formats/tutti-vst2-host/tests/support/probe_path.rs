//! Resolve the reference VST2 plugin (`tutti-vst2-test-plugin`) that the
//! conformance tests load.
//!
//! # Absence is a hard failure, never a skip
//!
//! [`probe_path`] **panics** with every path it searched. It does not return
//! `Option`, and there is deliberately no `_or_skip` variant to reach for.
//!
//! The probe is built from this tree, by a dev-dependency edge, in the same
//! `cargo test` invocation. Its absence is therefore a build failure — never
//! a property of the machine, the way a missing commercial plugin would be.
//! Making it skippable would put the suite one typo away from reporting
//! success having executed nothing.
//!
//! That is not hypothetical. Two incidents in this repo:
//!
//! - The VST3 probe bundle lived at a machine-specific path outside the repo
//!   and the test resolved it by a hardcoded filename. Building it under a
//!   different name made all nine tests skip **while printing
//!   `test result: ok. 9 passed`**.
//! - Every VST2 integration test in this crate was `#[ignore]`d against
//!   `/Library/Audio/Plug-Ins/VST/TAL-NoiseMaker.vst` — a macOS path, on a
//!   Linux host. Fifteen tests, dead for the entire life of the crate,
//!   reported as a clean suite.
//!
//! # Why the *newest* candidate, not the first
//!
//! Cargo writes the cdylib to `<profile>/deps/<name>` and hardlinks it up to
//! `<profile>/<name>`. It does not always refresh the uplifted copy — an
//! interrupted build, a switched feature set, or a shared target dir across
//! worktrees can leave a stale file at the more obvious path. Loading a
//! stale probe silently reverses test results: the misbehaviour switch the
//! test just set does not exist in the old image, so the plugin behaves
//! well, and the test asserting that the host survives misbehaviour passes
//! for entirely the wrong reason.
//!
//! So every candidate is stat'd and the newest mtime wins. This is the same
//! resolution the CLAP probe uses and for the same reason.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Directory Cargo drops the profile's artifacts in, from `build.rs`.
const PROBE_DIR: &str = env!("TUTTI_VST2_PROBE_DIR");

/// Platform cdylib filename, from `build.rs`.
const PROBE_LIB: &str = env!("TUTTI_VST2_PROBE_LIB");

/// Absolute path to the freshest built copy of the reference plugin.
///
/// # Panics
///
/// If no candidate exists. The message lists every path searched — a
/// missing probe is a build problem, and the reader needs to know where to
/// look, not merely that something was absent.
pub fn probe_path() -> &'static PathBuf {
    static RESOLVED: OnceLock<PathBuf> = OnceLock::new();
    RESOLVED.get_or_init(|| {
        let candidates = candidate_paths();

        let newest = candidates
            .iter()
            .filter_map(|p| {
                let mtime = std::fs::metadata(p).ok()?.modified().ok()?;
                Some((mtime, p.clone()))
            })
            .max_by_key(|(mtime, _)| *mtime)
            .map(|(_, p)| p);

        match newest {
            Some(p) => p,
            None => panic!(
                "reference VST2 plugin `{PROBE_LIB}` not found.\n\
                 \n\
                 It is a dev-dependency of this crate (tutti-vst2-test-plugin, \
                 crate-type = [\"cdylib\", \"rlib\"]), so `cargo test -p tutti-vst2-host` \
                 builds it. Its absence means the build did not produce a cdylib, \
                 not that this machine lacks a plugin — do NOT make this a skip.\n\
                 \n\
                 Searched:\n{}",
                candidates
                    .iter()
                    .map(|p| format!("  {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        }
    })
}

/// Every place Cargo might have left the cdylib, in no meaningful order —
/// the caller compares mtimes rather than trusting position.
fn candidate_paths() -> Vec<PathBuf> {
    let dir = PathBuf::from(PROBE_DIR);
    vec![
        // The uplifted hardlink. Most obvious, most likely to be stale.
        dir.join(PROBE_LIB),
        // Where Cargo actually writes it.
        dir.join("deps").join(PROBE_LIB),
    ]
}
