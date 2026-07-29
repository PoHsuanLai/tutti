//! Resolving the reference plugin — and why absence must be fatal.
//!
//! Every integration suite in this crate drives `tutti-clap-test-plugin`, a
//! cdylib built as a dev-dependency of this crate. `build.rs` names the places
//! cargo can drop it; this module picks the one that exists.
//!
//! ## Why this panics instead of skipping
//!
//! Each suite used to resolve the plugin itself and `return None` when it was
//! missing, with callers written as `let Some(p) = acquire() else { return };`.
//! A skip was therefore indistinguishable from a pass.
//!
//! That is not a theoretical hazard. Run under an isolated `CARGO_TARGET_DIR`,
//! cargo wrote the cdylib to `<profile>/deps/` but not `<profile>/`, so the
//! single guessed path did not resolve and **all 55 integration tests across
//! four binaries reported `ok` having executed nothing** — the suite announced
//! success for work it never did.
//!
//! The plugin is built by the same `cargo test` invocation that runs these
//! tests. Its absence is a build failure, not a property of the machine, so the
//! only correct response is to be loud.
//!
//! The same mistake, in the same shape, previously shipped in the VST3 suite:
//! a bundle resolved by hardcoded filename, renamed, and nine tests skipped
//! while printing `test result: ok. 9 passed`.

use std::path::Path;
use std::sync::OnceLock;

/// `;`-separated candidate paths emitted by `build.rs`.
const CANDIDATES: &str = env!("TUTTI_CLAP_TEST_PLUGIN_CANDIDATES");

/// Absolute path to the reference plugin cdylib, as a `&'static str` so it can
/// be handed straight to `libloading::Library::new` as well as `Path::new`.
///
/// Picks the **newest** candidate that exists, not the first. Cargo builds into
/// `<profile>/deps/` and hardlinks up to `<profile>/`, but does not always
/// refresh that copy — so both paths can hold *different builds of the same
/// plugin*. Loading the older one is worse than loading none: the suite runs
/// green against a plugin whose behaviour switches no longer match what the
/// tests set, so a mutation that should turn a test red silently does not. That
/// happened during this crate's RT work and briefly reversed a verification
/// result.
///
/// # Panics
///
/// If no candidate exists. See the module docs: this is deliberate, and any
/// change that softens it back into a skip re-opens a bug that has now shipped
/// twice.
pub fn probe_path() -> &'static str {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            let candidates: Vec<&str> = CANDIDATES.split(';').filter(|s| !s.is_empty()).collect();

            let newest = candidates
                .iter()
                .filter(|c| Path::new(*c).is_file())
                .filter_map(|c| {
                    let mtime = std::fs::metadata(c).and_then(|m| m.modified()).ok()?;
                    Some((mtime, *c))
                })
                .max_by_key(|(mtime, _)| *mtime);
            if let Some((_, path)) = newest {
                return path.to_string();
            }

            panic!(
                "reference plugin `tutti-clap-test-plugin` not found.\n\
                 Looked in:\n{}\n\n\
                 It is a dev-dependency of this crate, so `cargo test -p \
                 tutti-clap-host` should have built it. Its absence is a build \
                 failure, not a reason to skip: these tests are meaningless \
                 without it, and skipping would report success for work never \
                 done.",
                candidates
                    .iter()
                    .map(|c| format!("  - {c}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        })
        .as_str()
}
