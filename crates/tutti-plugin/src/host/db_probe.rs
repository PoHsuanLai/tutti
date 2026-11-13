//! The subprocess probe, as the probe `tutti-plugin-db` scans with.

pub use super::probe::DEFAULT_PROBE_TIMEOUT;
use super::probe::{measure_metadata_with, probe_metadata_with};
use std::path::Path;
use std::time::Duration;
use tracing::debug;
use tutti_plugin_db::{
    Delta, FileFacts, FileRow, PluginFormat, ProbeFailure, probe_plugin_fallback,
};
use tutti_plugin_types::BridgeError;
use tutti_plugin_types::LoadStage;
use tutti_plugin_types::Locator;
use tutti_plugin_types::PluginDescriptor;

/// How long a measuring probe may take, spawn, load and both renders included
/// (doc 023 section 6: two configurations, each up to the render budget of `MAX_NODE_TAIL` and 5 s of
/// audio, which a plugin slower than real time cannot meet in 15 s).
pub const DEFAULT_MEASURE_TIMEOUT: Duration = Duration::from_secs(180);

/// The blacklist decision for a failed probe: whether the scan may blacklist
/// the plugin, and why. One function, so the tests exercise this code rather
/// than a copy of it. (It was `From<BridgeError> for ProbeFailure`; the two
/// types are in different crates from this one since design 021 step 3.)
///
/// The split is "did we learn something about the plugin, or about our own
/// environment?" — not severity. A crash and a broken library are equally
/// informative; a missing `plugin-probe` tells us nothing.
#[must_use]
pub fn probe_failure(e: BridgeError) -> ProbeFailure {
    match e {
        BridgeError::ProcessCrashed => ProbeFailure {
            reason: format!("crashed during probe: {e}"),
            blacklistable: true,
        },
        BridgeError::Timeout { .. } => ProbeFailure {
            reason: format!("timed out during probe: {e}"),
            blacklistable: true,
        },
        // The plugin was reached and its library would not load — a stub
        // file, a wrong-arch binary, a broken install. Deterministic, and each
        // retry costs a full subprocess spawn. Carries the loader's own stage
        // and reason so the catalog entry says *why*.
        BridgeError::LoadFailed { .. } => ProbeFailure {
            reason: format!("failed to load during probe: {e}"),
            blacklistable: true,
        },
        // IO, a missing binary, a protocol mismatch — the environment's
        // fault, not the plugin's.
        other => ProbeFailure {
            reason: other.to_string(),
            blacklistable: false,
        },
    }
}

/// Probe a plugin file by spawning a `plugin-probe` subprocess and reading the
/// descriptor of every plugin it holds. Falls back to one filename-based
/// descriptor if the `plugin-probe` binary is not available.
pub(crate) fn probe_plugin(
    path: &Path,
    format: PluginFormat,
) -> Result<Vec<PluginDescriptor>, ProbeFailure> {
    interpret_probe(
        probe_metadata_with(&Locator::Search, path, DEFAULT_PROBE_TIMEOUT),
        path,
        format,
    )
}

/// [`probe_plugin`] that also measures: what a catalog scans with once it has
/// opted in ([`Catalog::with_measuring`](crate::catalog::Catalog::with_measuring)).
/// A plugin that crashes or hangs while rendering is blacklisted as it is for
/// one that crashes in its load.
pub(crate) fn probe_plugin_measured(
    path: &Path,
    format: PluginFormat,
) -> Result<Vec<PluginDescriptor>, ProbeFailure> {
    interpret_probe(
        measure_metadata_with(&Locator::Search, path, DEFAULT_MEASURE_TIMEOUT, None),
        path,
        format,
    )
}

/// Decide what a raw probe result means. Split from [`probe_plugin`] so the
/// fallback rule is testable without a subprocess — a test that spawns a real
/// probe depends on whether `target/debug/` happens to be warm.
fn interpret_probe(
    result: Result<Vec<PluginDescriptor>, BridgeError>,
    path: &Path,
    format: PluginFormat,
) -> Result<Vec<PluginDescriptor>, ProbeFailure> {
    match result {
        Ok(plugins) => Ok(plugins),
        // The only error that means "we could not look", as opposed to "we looked
        // and this plugin is bad". Widening it would hide real failures.
        Err(BridgeError::ProbeNotFound) => {
            debug!(
                "plugin-probe not available, using filename metadata for {:?}",
                path
            );
            Ok(vec![probe_plugin_fallback(path, format)])
        }
        Err(e) => Err(probe_failure(e)),
    }
}

/// Probes a plugin file into the delta that records it: one row holding every
/// plugin the file has (a CLAP factory's, a VST3 factory's audio classes, an AU
/// bundle's units, a VST2 shell's sub-plugins).
///
/// Spawns a short-lived `plugin-probe` subprocess to read the file's
/// metadata, so a plugin that crashes while being probed does not take the
/// caller down. Blocks for up to the deadline (fifteen seconds by default); call
/// it off the audio and UI threads. The delta puts the same row a scan stores; apply it with
/// [`PluginDb::apply`](tutti_plugin_db::PluginDb::apply).
///
/// # Errors
///
/// Returns [`BridgeError::LoadFailed`] for an unrecognized extension or a
/// plugin the probe cannot load,
/// [`BridgeError::ProbeNotFound`] when the `plugin-probe` binary cannot
/// be located, [`BridgeError::ProcessCrashed`] or [`BridgeError::Timeout`] when
/// the plugin brought the probe down, and an I/O variant when it cannot be
/// spawned.
pub fn probe_record(path: impl AsRef<Path>) -> Result<Delta, BridgeError> {
    probe_record_with(path, &Locator::Search, DEFAULT_PROBE_TIMEOUT)
}

/// [`probe_record`] with the `plugin-probe` binary named by `locator` and the
/// deadline `timeout` (the default is [`DEFAULT_PROBE_TIMEOUT`], fifteen
/// seconds, which a plugin that loads slowly from cold may need).
///
/// # Errors
///
/// As [`probe_record`].
pub fn probe_record_with(
    path: impl AsRef<Path>,
    locator: &Locator,
    timeout: Duration,
) -> Result<Delta, BridgeError> {
    let path = path.as_ref();
    let facts = FileFacts::read(path);
    let format = facts.format.ok_or_else(|| BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: LoadStage::Opening,
        reason: "unrecognized plugin extension".to_string(),
    })?;

    let plugins = probe_metadata_with(locator, path, timeout)?;

    // Keyed like a scan keys it, by canonical path, so an alias and the real
    // path are one row.
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    Ok(Delta::put(key, FileRow::probed(format, &facts, plugins)))
}

/// [`probe_record`] that also **measures** each plugin the file holds (doc 023
/// section 6): the delta's row carries every descriptor's
/// [`measured`](PluginDescriptor::measured), the tail and latency the plugin
/// reports against what it renders. The way to measure one file, or one a scan
/// already probed without measuring (a scan skips a file whose time has not
/// changed). The row replaces the file's old one, as a probe's does.
///
/// Renders the plugin at two configurations, so it takes longer
/// ([`DEFAULT_MEASURE_TIMEOUT`]); call it off the audio and UI threads.
///
/// # Errors
///
/// As [`probe_record`]; a plugin that crashes or hangs while rendering is
/// [`BridgeError::ProcessCrashed`] or [`BridgeError::Timeout`].
pub fn measure_record(path: impl AsRef<Path>) -> Result<Delta, BridgeError> {
    measure_record_with(path, &Locator::Search, DEFAULT_MEASURE_TIMEOUT, None)
}

/// [`measure_record`] with the `plugin-probe` binary named by `locator`, the
/// deadline `timeout`, and, if `state` is given, the file of bytes a plugin's own
/// `save_state` wrote, which the plugin is put in before it is measured (for a
/// file of one plugin; the record's `ProbeConfig::state` hashes it).
///
/// # Errors
///
/// As [`measure_record`].
pub fn measure_record_with(
    path: impl AsRef<Path>,
    locator: &Locator,
    timeout: Duration,
    state: Option<&Path>,
) -> Result<Delta, BridgeError> {
    let path = path.as_ref();
    let facts = FileFacts::read(path);
    let format = facts.format.ok_or_else(|| BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: LoadStage::Opening,
        reason: "unrecognized plugin extension".to_string(),
    })?;
    let plugins = measure_metadata_with(locator, path, timeout, state)?;
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    Ok(Delta::put(key, FileRow::probed(format, &facts, plugins)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tutti_plugin_types::PluginClass;

    /// A failure that is a property of the plugin must earn a catalog entry, not
    /// just a `warn!` — without one `needs_rescan` stays true and the plugin is
    /// re-probed at full cost (the crash, the 5 s stall, the failing `dlopen`) on
    /// every scan, forever.
    ///
    /// The dividing line is whether the failure taught us about the *plugin* or
    /// about the *environment*, not severity: a broken library is as informative
    /// as a crash, while a missing `plugin-probe` is not informative at all.
    #[test]
    fn plugin_failures_are_blacklistable_environmental_ones_are_not() {
        use tutti_plugin_types::BridgeError;

        // Exercises the *production* classifier, not a copy of it.
        let crashed = probe_failure(BridgeError::ProcessCrashed);
        assert!(
            crashed.blacklistable,
            "ProcessCrashed must be blacklistable"
        );
        assert!(crashed.reason.contains("crashed"));

        let timed_out = probe_failure(BridgeError::Timeout {
            operation: "probe".into(),
            duration_ms: 5000,
            // Irrelevant to the blacklist verdict, which is what this asserts:
            // a probe that timed out is unusable either way.
            partial: false,
        });
        assert!(timed_out.blacklistable, "Timeout must be blacklistable");
        assert!(timed_out.reason.contains("timed out"));

        // A load failure was the one path that recorded nothing.
        let load_failed = probe_failure(BridgeError::LoadFailed {
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
        assert!(!probe_failure(BridgeError::IpcError("socket".into())).blacklistable);
        assert!(
            !probe_failure(BridgeError::ProtocolMismatch {
                expected: 2,
                got: 1
            })
            .blacklistable
        );
        assert!(
            !probe_failure(BridgeError::ProbeNotFound).blacklistable,
            "a missing plugin-probe must never blacklist a plugin"
        );
    }

    /// A missing `plugin-probe` must degrade to filename metadata rather than
    /// fail, and must report `Unknown` when it does: name and id come from the
    /// filename, but nothing inspected the plugin, so claiming a category would be
    /// a fabrication.
    ///
    /// Driven through [`interpret_probe`] rather than by arranging for the probe
    /// to be absent. An earlier version called `probe_plugin` and relied on the
    /// binary not being built — but the search covers
    /// `current_exe().parent().parent()`, which for a test binary in
    /// `target/debug/deps/` is `target/debug/`, where the binary lands as soon as
    /// anything in the workspace builds it. So it passed on a cold target dir and
    /// failed on a warm one, having found a real binary that then reported
    /// `LoadFailed` on the fake plugin. `TUTTI_PLUGIN_PROBE` is no lever either:
    /// a nonexistent path there warns and falls through to the same search.
    #[test]
    fn probe_without_a_probe_binary_falls_back_to_filename_metadata() {
        use tutti_plugin_types::BridgeError;

        let path = Path::new("/plugins/Plain.vst3");
        let plugins = interpret_probe(Err(BridgeError::ProbeNotFound), path, PluginFormat::Vst3)
            .expect("a missing plugin-probe is a fallback, not an error");
        let [descriptor] = plugins.as_slice() else {
            panic!("the filename names one plugin: {plugins:?}");
        };

        assert_eq!(descriptor.name, "Plain");
        assert_eq!(descriptor.id, "vst3.plain");
        assert!(
            matches!(descriptor.class, PluginClass::Unknown),
            "an unprobed plugin must not claim a category, got {:?}",
            descriptor.class
        );
    }

    /// The counterpart: only `ProbeNotFound` falls back. A load failure is a real
    /// answer about a real plugin and must surface as an error, or a broken plugin
    /// would be silently catalogued under its filename as though it had been probed.
    #[test]
    fn a_failed_load_is_an_error_not_a_filename_fallback() {
        use tutti_plugin_types::BridgeError;

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
}
