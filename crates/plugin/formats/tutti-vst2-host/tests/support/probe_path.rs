//! Resolve the reference VST2 plugin (`tutti-vst2-test-plugin`) that the
//! conformance tests load.
//!
//! **Absence panics, never skips.** The probe is built from this tree by a
//! dev-dependency edge in the same `cargo test` invocation, so its absence is
//! a build failure, not a property of the machine. A skip would put the suite
//! one typo away from reporting success having executed nothing — which has
//! happened here twice, most memorably nine VST3 tests skipping while
//! printing `test result: ok. 9 passed`.
//!
//! **The newest candidate wins, not the first.** Cargo writes the cdylib to
//! `<profile>/deps/<name>` and hardlinks it up to `<profile>/<name>` without
//! always refreshing the uplifted copy, so the more obvious path can be stale.
//! A stale probe reverses results silently: the misbehaviour switch the test
//! set does not exist in the old image, the plugin behaves well, and the test
//! asserting the host survives misbehaviour passes for the wrong reason.

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
/// If no candidate exists, listing every path searched.
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

/// Every place Cargo might have left the cdylib. Order is not significant:
/// the caller compares mtimes.
fn candidate_paths() -> Vec<PathBuf> {
    let dir = PathBuf::from(PROBE_DIR);
    vec![
        // The uplifted hardlink. Most obvious, most likely to be stale.
        dir.join(PROBE_LIB),
        // Where Cargo actually writes it.
        dir.join("deps").join(PROBE_LIB),
    ]
}
