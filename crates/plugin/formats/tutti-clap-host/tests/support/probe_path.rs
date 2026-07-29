//! Resolves `tutti-clap-test-plugin`, the cdylib every integration suite here
//! drives. `build.rs` names the paths cargo can drop it at; this picks one.
//!
//! Absence is fatal, never a skip. The suites used to resolve the plugin
//! themselves and return `None`, read as `let Some(p) = acquire() else
//! { return };` — making a skip indistinguishable from a pass. Under an
//! isolated `CARGO_TARGET_DIR` the guessed path stopped resolving and all 55
//! integration tests reported `ok` having run nothing. The plugin is built by
//! the same `cargo test` that runs these tests, so its absence is a build
//! failure, not a property of the machine.

use std::path::Path;
use std::sync::OnceLock;

/// `;`-separated candidate paths emitted by `build.rs`.
const CANDIDATES: &str = env!("TUTTI_CLAP_TEST_PLUGIN_CANDIDATES");

/// Absolute path to the reference plugin cdylib, as a `&'static str` so it
/// suits both `libloading::Library::new` and `Path::new`.
///
/// Picks the **newest** existing candidate, not the first. Cargo builds into
/// `<profile>/deps/` and hardlinks up to `<profile>/` without always refreshing
/// it, so the two can hold different builds of the same plugin. Loading the
/// older one is worse than loading none: the suite runs green against a plugin
/// whose switches no longer match what the tests set, so a mutation that should
/// turn a test red silently does not.
///
/// # Panics
///
/// If no candidate exists — deliberate; see the module docs.
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
