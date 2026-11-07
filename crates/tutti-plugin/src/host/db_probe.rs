//! The subprocess probe, as the probe `tutti-plugin-db` scans with.

use crate::error::{BridgeError, LoadStage};
use crate::protocol::PluginDescriptor;
use std::path::Path;
use tracing::debug;
use tutti_plugin_db::{
    Delta, FileFacts, FileRow, PluginFormat, ProbeFailure, probe_plugin_fallback,
};

/// The blacklist decision for a failed probe, kept as a conversion so tests
/// exercise this code rather than a copy of it.
///
/// The split is "did we learn something about the plugin, or about our own
/// environment?" — not severity. A crash and a broken library are equally
/// informative; a missing `plugin-server` tells us nothing.
impl From<BridgeError> for ProbeFailure {
    fn from(e: BridgeError) -> Self {
        match e {
            BridgeError::ProcessCrashed => Self {
                reason: format!("crashed during probe: {e}"),
                blacklistable: true,
            },
            BridgeError::Timeout { .. } => Self {
                reason: format!("timed out during probe: {e}"),
                blacklistable: true,
            },
            // The plugin was reached and its library would not load — a stub
            // file, a wrong-arch binary, a broken install. Deterministic, and each
            // retry costs a full subprocess spawn. Carries the loader's own stage
            // and reason so the catalog entry says *why*.
            BridgeError::LoadFailed { .. } => Self {
                reason: format!("failed to load during probe: {e}"),
                blacklistable: true,
            },
            // IO, a missing binary, a protocol mismatch — the environment's
            // fault, not the plugin's.
            other => Self {
                reason: other.to_string(),
                blacklistable: false,
            },
        }
    }
}

/// Probe a plugin by spawning a plugin-server subprocess and querying its
/// metadata. Falls back to filename-based metadata if the plugin-server
/// binary is not available.
pub(crate) fn probe_plugin(
    path: &Path,
    format: PluginFormat,
) -> Result<PluginDescriptor, ProbeFailure> {
    interpret_probe(super::subprocess::probe_metadata(path), path, format)
}

/// Decide what a raw probe result means. Split from [`probe_plugin`] so the
/// fallback rule is testable without a subprocess — a test that spawns a real
/// server depends on whether `target/debug/` happens to be warm.
fn interpret_probe(
    result: Result<PluginDescriptor, BridgeError>,
    path: &Path,
    format: PluginFormat,
) -> Result<PluginDescriptor, ProbeFailure> {
    match result {
        Ok(descriptor) => Ok(descriptor),
        // The only error that means "we could not look", as opposed to "we looked
        // and this plugin is bad". Widening it would hide real failures.
        Err(BridgeError::ServerNotFound) => {
            debug!(
                "plugin-server not available, using filename metadata for {:?}",
                path
            );
            Ok(probe_plugin_fallback(path, format))
        }
        Err(e) => Err(ProbeFailure::from(e)),
    }
}

/// Probes a plugin file into the delta that records it.
///
/// Spawns a short-lived `plugin-server` subprocess to read the plugin's
/// metadata, so a plugin that crashes while being probed does not take the
/// caller down. Blocks for up to a few seconds; call it off the audio and
/// UI threads. The delta puts the same row a scan stores; apply it with
/// [`PluginDb::apply`](tutti_plugin_db::PluginDb::apply).
///
/// # Errors
///
/// Returns [`BridgeError::LoadFailed`] for an unrecognized extension or a
/// plugin the server cannot load,
/// [`BridgeError::ServerNotFound`] when the `plugin-server` binary cannot
/// be located, and a timeout, I/O or protocol variant when the subprocess
/// cannot be reached.
pub fn probe_record(path: impl AsRef<Path>) -> Result<Delta, BridgeError> {
    let path = path.as_ref();
    let facts = FileFacts::read(path);
    let format = facts.format.ok_or_else(|| BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: LoadStage::Opening,
        reason: "unrecognized plugin extension".to_string(),
    })?;

    let descriptor = super::subprocess::probe_metadata(path)?;

    Ok(Delta::put(
        path,
        FileRow::probed(format, &facts, vec![descriptor]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PluginClass;
    use std::path::PathBuf;
    use tutti_plugin_db::format_from_path;

    /// A failure that is a property of the plugin must earn a catalog entry, not
    /// just a `warn!` — without one `needs_rescan` stays true and the plugin is
    /// re-probed at full cost (the crash, the 5 s stall, the failing `dlopen`) on
    /// every scan, forever.
    ///
    /// The dividing line is whether the failure taught us about the *plugin* or
    /// about the *environment*, not severity: a broken library is as informative
    /// as a crash, while a missing `plugin-server` is not informative at all.
    #[test]
    fn plugin_failures_are_blacklistable_environmental_ones_are_not() {
        use crate::error::BridgeError;

        // Exercises the *production* classifier, not a copy of it.
        let crashed = ProbeFailure::from(BridgeError::ProcessCrashed);
        assert!(
            crashed.blacklistable,
            "ProcessCrashed must be blacklistable"
        );
        assert!(crashed.reason.contains("crashed"));

        let timed_out = ProbeFailure::from(BridgeError::Timeout {
            operation: "probe".into(),
            duration_ms: 5000,
            // Irrelevant to the blacklist verdict, which is what this asserts:
            // a probe that timed out is unusable either way.
            partial: false,
        });
        assert!(timed_out.blacklistable, "Timeout must be blacklistable");
        assert!(timed_out.reason.contains("timed out"));

        // A load failure was the one path that recorded nothing.
        let load_failed = ProbeFailure::from(BridgeError::LoadFailed {
            path: PathBuf::from("/plugins/Broken.vst3"),
            stage: tutti_plugin_types::LoadStage::Opening,
            reason: "dlopen failed".into(),
        });
        assert!(
            load_failed.blacklistable,
            "a library that will not open fails identically on every scan, at a \
             full subprocess spawn each time — it must be recorded"
        );
        assert!(
            load_failed.reason.contains("dlopen failed"),
            "the loader's own reason must survive into the catalog entry, or the \
             user sees a hidden plugin with no explanation: {}",
            load_failed.reason
        );

        // Environmental failures say nothing about the plugin — never hide it.
        assert!(!ProbeFailure::from(BridgeError::IpcError("socket".into())).blacklistable);
        assert!(
            !ProbeFailure::from(BridgeError::ProtocolMismatch {
                expected: 2,
                got: 1
            })
            .blacklistable
        );
        assert!(
            !ProbeFailure::from(BridgeError::ServerNotFound).blacklistable,
            "a missing plugin-server must never blacklist a plugin"
        );
    }

    /// A missing `plugin-server` must degrade to filename metadata rather than
    /// fail, and must report `Unknown` when it does: name and id come from the
    /// filename, but nothing inspected the plugin, so claiming a category would be
    /// a fabrication.
    ///
    /// Driven through [`interpret_probe`] rather than by arranging for the server
    /// to be absent. An earlier version called `probe_plugin` and relied on the
    /// binary not being built — but `find_plugin_server` searches
    /// `current_exe().parent().parent()`, which for a test binary in
    /// `target/debug/deps/` is `target/debug/`, where the server lands as soon as
    /// anything in the workspace builds it. So it passed on a cold target dir and
    /// failed on a warm one, having found a real server that then reported
    /// `LoadFailed` on the fake plugin. `TUTTI_PLUGIN_SERVER` is no lever either:
    /// a nonexistent path there warns and falls through to the same search.
    #[test]
    fn probe_without_a_server_falls_back_to_filename_metadata() {
        use crate::error::BridgeError;

        let path = Path::new("/plugins/TAL-NoiseMaker.vst3");
        let descriptor =
            interpret_probe(Err(BridgeError::ServerNotFound), path, PluginFormat::Vst3)
                .expect("a missing plugin-server is a fallback, not an error");

        assert_eq!(descriptor.name, "TAL-NoiseMaker");
        assert_eq!(descriptor.id, "vst3.tal-noisemaker");
        assert!(
            matches!(descriptor.class, PluginClass::Unknown),
            "an unprobed plugin must not claim a category, got {:?}",
            descriptor.class
        );
    }

    /// The counterpart: only `ServerNotFound` falls back. A load failure is a real
    /// answer about a real plugin and must surface as an error, or a broken plugin
    /// would be silently catalogued under its filename as though it had been probed.
    #[test]
    fn a_failed_load_is_an_error_not_a_filename_fallback() {
        use crate::error::BridgeError;

        let path = Path::new("/plugins/Broken.vst3");
        let failure = interpret_probe(
            Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: tutti_plugin_types::LoadStage::Opening,
                reason: "dlopen failed".into(),
            }),
            path,
            PluginFormat::Vst3,
        )
        .expect_err("a load failure must not be reported as successful metadata");

        assert!(failure.reason.contains("dlopen failed"));
    }

    /// Probe a real installed plugin end-to-end. **Opt-in**: set
    /// `TUTTI_PROBE_PLUGIN` to a plugin path to run it.
    ///
    /// Gated rather than auto-detected, because the auto-detecting version of
    /// this test was wrong in three ways at once and failed for years: it took
    /// the first of two hardcoded paths (the VST3) and then asserted a *VST2*
    /// class, which `PluginClass` makes unsatisfiable — the enum is per-format
    /// by construction. It also could not reach a real probe at all without the
    /// `plugin-server` binary, since `probe_plugin` maps `ServerNotFound` to a
    /// filename fallback, so it was really asserting a synth category against
    /// `PluginClass::Unknown`.
    ///
    /// What it can honestly check, given the class vocabulary is deliberately
    /// native and uninterpreted: the probe returns a name, and the class it
    /// reports belongs to the format that was actually probed.
    #[test]
    fn probe_installed_plugin_reports_its_own_formats_class() {
        let Ok(raw) = std::env::var("TUTTI_PROBE_PLUGIN") else {
            eprintln!("TUTTI_PROBE_PLUGIN not set, skipping real-plugin probe");
            return;
        };
        let path = Path::new(&raw);
        assert!(path.exists(), "TUTTI_PROBE_PLUGIN={raw} does not exist");

        let format = format_from_path(path).expect("unrecognised plugin extension");
        let descriptor = probe_plugin(path, format).expect("probe failed");

        assert!(!descriptor.name.is_empty(), "name should not be empty");

        // The class must match the format probed — never another format's
        // vocabulary. `Unknown` is allowed: it is what the filename fallback
        // reports when no `plugin-server` is on hand.
        let class_format = descriptor.class.format_name();
        assert!(
            class_format == "unknown" || class_format == format.extension_id(),
            "a {:?} plugin reported a {class_format} class: {:?}",
            format,
            descriptor.class
        );
        println!("Probed {path:?}: {descriptor:?}");
    }
}
