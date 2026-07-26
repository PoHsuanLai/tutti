//! Crash-recovery plugin scanner.
//!
//! Discovers plugin files in directories, validates them against a
//! [`PluginCatalog`], and reports progress via a channel. An internal
//! dead-man's pedal auto-blacklists plugins that crash during scanning.

use super::catalog::{CatalogExt, PluginCatalog};
use super::fs::{discover_plugins, file_modification_time};
use super::pedal::Pedal;
use super::record::{Blacklist, PluginClass, PluginDescriptor, PluginFormat, PluginRecord};
use crossbeam_channel::{Receiver, Sender};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Progress information emitted during a scan.
#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub current: usize,
    pub total: usize,
    pub current_path: PathBuf,
    pub phase: ScanPhase,
}

/// Which phase the scanner is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanPhase {
    /// Finding plugin files on disk.
    Discovery,
    /// Loading and validating individual plugins.
    Scanning,
    /// Scan is finished.
    Complete,
}

/// Handle returned by [`PluginScanner::scan_async`] for monitoring progress.
pub struct ScanHandle {
    pub progress_rx: Receiver<ScanProgress>,
    pub result_rx: Receiver<ScanResult>,
}

/// Summary of a completed scan.
#[derive(Debug, Clone)]
pub struct ScanResult {
    /// Total plugins examined (including skipped).
    pub scanned: usize,
    /// Newly added or updated records.
    pub new: usize,
    /// Plugins that failed to load (including those newly blacklisted).
    pub failed: usize,
    /// Plugins that were blacklisted (previously or newly).
    pub blacklisted: usize,
    /// Plugins this scan added to the blacklist because they crashed, hung, or
    /// failed to load. Surface this so a UI can tell the user something was hidden
    /// and point at [`CatalogExt::unblacklist`] / [`CatalogExt::clear_blacklist`].
    pub newly_blacklisted: usize,
}

/// Plugin scanner with crash recovery. Operates on any [`PluginCatalog`].
pub struct PluginScanner {
    catalog: Box<dyn PluginCatalog>,
    pedal: Pedal,
}

impl PluginScanner {
    /// Construct a scanner around `catalog`. `pedal_path` is where the
    /// dead-man's pedal sentinel file lives (crashes leave it behind,
    /// next run blacklists whatever it points at).
    pub fn new(catalog: Box<dyn PluginCatalog>, pedal_path: impl Into<PathBuf>) -> Self {
        let pedal = Pedal::new(pedal_path.into());
        Self { catalog, pedal }
    }

    /// If a previous scan crashed, blacklist the offending plugin.
    /// Safe to call multiple times; a no-op when no pedal is present.
    pub fn recover_crash(&mut self) {
        self.pedal.recover(self.catalog.as_mut());
    }

    /// Consume the scanner and return the inner catalog.
    pub fn into_catalog(self) -> Box<dyn PluginCatalog> {
        self.catalog
    }

    /// Scan directories asynchronously on a background thread.
    /// Runs crash recovery before scanning.
    pub fn scan_async(mut self, directories: Vec<PathBuf>) -> ScanHandle {
        let (progress_tx, progress_rx) = crossbeam_channel::unbounded();
        let (result_tx, result_rx) = crossbeam_channel::bounded(1);

        std::thread::Builder::new()
            .name("plugin-scanner".into())
            .spawn(move || {
                self.recover_crash();
                let result = self.scan_inner(&directories, Some(&progress_tx));
                let _ = result_tx.send(result);
                if let Err(e) = self.catalog.flush() {
                    warn!("failed to flush plugin catalog after async scan: {e}");
                }
            })
            .expect("failed to spawn plugin scanner thread");

        ScanHandle {
            progress_rx,
            result_rx,
        }
    }

    /// Scan directories synchronously (blocking). Runs crash recovery,
    /// scans, then flushes the catalog.
    pub fn scan_sync(&mut self, directories: Vec<PathBuf>) -> ScanResult {
        self.recover_crash();
        let result = self.scan_inner(&directories, None);
        if let Err(e) = self.catalog.flush() {
            warn!("failed to flush plugin catalog after sync scan: {e}");
        }
        result
    }

    /// Core scanning logic shared between sync and async paths.
    fn scan_inner(
        &mut self,
        directories: &[PathBuf],
        progress_tx: Option<&Sender<ScanProgress>>,
    ) -> ScanResult {
        let emit = |p: ScanProgress| {
            if let Some(tx) = progress_tx {
                let _ = tx.send(p);
            }
        };

        // Phase 1: Discovery
        let plugin_paths: Vec<(PathBuf, PluginFormat)> = directories
            .iter()
            .flat_map(|dir| {
                emit(ScanProgress {
                    current: 0,
                    total: 0,
                    current_path: dir.clone(),
                    phase: ScanPhase::Discovery,
                });
                discover_plugins(dir)
            })
            .collect();

        let total = plugin_paths.len();
        info!(
            "discovered {total} plugin files across {} directories",
            directories.len()
        );

        // Phase 2: Scanning — classify (pure) then execute (effectful).
        let result = plugin_paths
            .iter()
            .enumerate()
            .map(|(i, (path, format))| {
                emit(ScanProgress {
                    current: i + 1,
                    total,
                    current_path: path.clone(),
                    phase: ScanPhase::Scanning,
                });
                match classify(self.catalog.as_ref(), path) {
                    ScanDecision::Skip(outcome) => outcome,
                    ScanDecision::Probe => self.probe_and_record(path, *format),
                }
            })
            .fold(ScanResult::with_total(total), ScanResult::tally);

        emit(ScanProgress {
            current: total,
            total,
            current_path: PathBuf::new(),
            phase: ScanPhase::Complete,
        });

        info!(
            "scan complete: {} scanned, {} new, {} failed, {} blacklisted",
            result.scanned, result.new, result.failed, result.blacklisted
        );

        result
    }

    /// Probe one plugin with pedal protection and upsert into the catalog.
    ///
    /// A failing probe is *recorded*, not merely warned about. Previously the
    /// pedal was armed and disarmed unconditionally and a failure produced no
    /// catalog write at all, so `needs_rescan` stayed true and the plugin was
    /// re-probed at full cost — a 5 s hang, or a crashing subprocess — on
    /// every scan, forever. Crash and timeout go to the blacklist, which
    /// is exactly the case the module doc advertises and the only case the
    /// pedal could not cover (the pedal fires when the *scanner host* dies,
    /// which is what out-of-process probing prevents).
    ///
    /// JUCE does the same: a scan attempt that yields nothing goes into
    /// `failedFiles` and then `addToBlacklist` — failure, not just a hard
    /// crash, earns the blacklist.
    ///
    /// **R5:** that reasoning applies to a *load* failure too, and it used to be
    /// the one path that still wrote nothing. A plugin whose library will not open
    /// — a stub file, a wrong-arch binary, a broken install — fails identically on
    /// every future scan, and each attempt costs a full subprocess spawn. It is
    /// recorded for the same reason a crash is. What it is *not* is silently
    /// dropped: the entry carries the loader's own reason string, `blacklisted()`
    /// surfaces it to a UI, and [`CatalogExt::unblacklist`] is its inverse.
    ///
    /// The mtime stamp is what makes this safe to be wrong about — reinstalling or
    /// updating the plugin re-admits it automatically, without the user knowing the
    /// blacklist exists.
    fn probe_and_record(&mut self, path: &Path, format: PluginFormat) -> ScanOutcome {
        if let Err(e) = self.pedal.arm(path) {
            warn!("failed to arm dead-man's pedal: {e}");
        }
        let outcome = match probe_plugin(path, format) {
            Ok(descriptor) => {
                self.catalog.upsert(PluginRecord {
                    path: path.to_path_buf(),
                    format,
                    descriptor,
                    modification_time: file_modification_time(path).unwrap_or(0),
                    blacklist: Blacklist::Ok,
                    extension_id: None,
                    manifest_index: None,
                });
                ScanOutcome::New
            }
            Err(failure) => {
                warn!("failed to probe plugin {:?}: {}", path, failure.reason);
                if failure.blacklistable {
                    // The blacklist stamps the file's current mtime, so a
                    // reinstall or update re-admits the plugin (see
                    // `CatalogExt::is_blacklisted_and_unchanged`).
                    self.catalog.blacklist(path, failure.reason);
                    ScanOutcome::NewlyBlacklisted
                } else {
                    // Environmental failure — no plugin-server, an IO error, a
                    // protocol mismatch. Says nothing about the plugin, so write
                    // nothing: `needs_rescan` must stay true so the plugin is
                    // retried once the environment is fixed. Re-probing is the
                    // *correct* behaviour here, and it is cheap because these
                    // failures do not reach a subprocess spawn.
                    ScanOutcome::Failed
                }
            }
        };
        self.pedal.disarm();
        outcome
    }
}

/// What the scanner decided to do with a discovered plugin.
enum ScanDecision {
    Skip(ScanOutcome),
    Probe,
}

/// Per-plugin outcome tallied into `ScanResult`.
#[derive(Clone, Copy)]
enum ScanOutcome {
    New,
    Failed,
    /// Already blacklisted before this scan; skipped.
    Blacklisted,
    /// Blacklisted *by* this scan: a crash, a timeout, or a failed load.
    NewlyBlacklisted,
    UpToDate,
}

/// A probe failure, plus whether it earns a blacklist entry.
#[derive(Debug)]
struct ProbeFailure {
    reason: String,
    /// Set when the failure is a property of *this plugin* — it will recur
    /// identically on every future scan, at the same cost — so recording it beats
    /// re-probing forever. Clear for environmental failures, which say nothing
    /// about the plugin and must stay retryable.
    blacklistable: bool,
}

impl ProbeFailure {
    /// The whole blacklist decision, in one pure function so tests exercise
    /// *this* code rather than a copy of it.
    ///
    /// The split is "did we learn something about the plugin, or about our own
    /// environment?" — not severity. A crash and a broken library are equally
    /// informative; a missing `plugin-server` tells us nothing.
    fn from_bridge_error(e: crate::error::BridgeError) -> Self {
        use crate::error::BridgeError;
        match e {
            BridgeError::ProcessCrashed => Self {
                reason: format!("crashed during probe: {e}"),
                blacklistable: true,
            },
            BridgeError::Timeout { .. } => Self {
                reason: format!("timed out during probe: {e}"),
                blacklistable: true,
            },
            // R5: the plugin was reached and its library would not load — a stub
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

/// Pure classification: inspects the catalog, no side effects.
///
/// The blacklist check is mtime-aware
/// ([`CatalogExt::is_blacklisted_and_unchanged`]): a blacklisted plugin whose
/// file changed on disk — reinstall, vendor update — is re-probed rather than
/// skipped forever. Checking the raw flag first meant nothing short of
/// hand-editing the DB could clear a false positive.
fn classify(catalog: &dyn PluginCatalog, path: &Path) -> ScanDecision {
    if catalog.is_blacklisted_and_unchanged(path) {
        debug!("skipping blacklisted plugin: {:?}", path);
        ScanDecision::Skip(ScanOutcome::Blacklisted)
    } else if !catalog.needs_rescan(path) {
        debug!("skipping up-to-date plugin: {:?}", path);
        ScanDecision::Skip(ScanOutcome::UpToDate)
    } else {
        ScanDecision::Probe
    }
}

impl ScanResult {
    fn with_total(scanned: usize) -> Self {
        Self {
            scanned,
            new: 0,
            failed: 0,
            blacklisted: 0,
            newly_blacklisted: 0,
        }
    }

    fn tally(mut self, outcome: ScanOutcome) -> Self {
        match outcome {
            ScanOutcome::New => self.new += 1,
            ScanOutcome::Failed => self.failed += 1,
            ScanOutcome::Blacklisted => self.blacklisted += 1,
            // A newly blacklisted plugin is both a failure this scan and a
            // blacklist entry going forward; count it in both tallies so
            // `failed` still means "did not load".
            ScanOutcome::NewlyBlacklisted => {
                self.failed += 1;
                self.blacklisted += 1;
                self.newly_blacklisted += 1;
            }
            ScanOutcome::UpToDate => {}
        }
        self
    }
}

/// Probe a plugin by spawning a plugin-server subprocess and querying its
/// metadata. Falls back to filename-based metadata if the plugin-server
/// binary is not available.
fn probe_plugin(path: &Path, format: PluginFormat) -> Result<PluginDescriptor, ProbeFailure> {
    interpret_probe(crate::host::subprocess::probe_metadata(path), path, format)
}

/// Decide what a raw probe result means. Split from [`probe_plugin`] so the
/// fallback rule is testable without a subprocess: spawning a real server is what
/// made the old test depend on whether `target/debug/` happened to be warm.
fn interpret_probe(
    result: Result<PluginDescriptor, crate::error::BridgeError>,
    path: &Path,
    format: PluginFormat,
) -> Result<PluginDescriptor, ProbeFailure> {
    use crate::error::BridgeError;
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
        Err(e) => Err(ProbeFailure::from_bridge_error(e)),
    }
}

/// Fallback: create minimal metadata from filename when plugin-server is
/// unavailable. The native classification is unknown without a probe, so the
/// descriptor carries [`PluginClass::Unknown`].
fn probe_plugin_fallback(path: &Path, format: PluginFormat) -> PluginDescriptor {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let id = format!(
        "{}.{}",
        format.extension_id(),
        name.to_lowercase().replace(' ', "_")
    );

    PluginDescriptor::new(id, name, PluginClass::Unknown)
}

#[cfg(test)]
mod tests {
    use super::super::database::JsonCatalog;
    use super::*;
    use tempfile::TempDir;

    fn create_fake_plugin(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"fake plugin binary").unwrap();
        path
    }

    fn scanner_for(dir: &Path) -> PluginScanner {
        let db = JsonCatalog::empty(dir.join("db.json"));
        PluginScanner::new(Box::new(db), dir.join(".scanning"))
    }

    #[test]
    fn sync_scan_discovers_and_records() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        create_fake_plugin(&plugins_dir, "synth.vst3");
        create_fake_plugin(&plugins_dir, "comp.clap");

        let mut scanner = scanner_for(dir.path());

        let result = scanner.scan_sync(vec![plugins_dir]);
        assert_eq!(result.scanned, 2);
        assert_eq!(result.new + result.failed, 2);
    }

    #[test]
    fn sync_scan_skips_blacklisted() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        let bad = create_fake_plugin(&plugins_dir, "bad.vst3");
        create_fake_plugin(&plugins_dir, "good.vst3");

        let mut db = JsonCatalog::empty(dir.path().join("db.json"));
        db.blacklist(&bad, "known crasher".into());

        let mut scanner = PluginScanner::new(Box::new(db), dir.path().join(".scanning"));
        let result = scanner.scan_sync(vec![plugins_dir]);

        assert_eq!(result.scanned, 2);
        assert_eq!(result.blacklisted, 1);
        assert_eq!(result.new + result.failed, 1);
    }

    #[test]
    fn sync_scan_skips_up_to_date() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        let plugin = create_fake_plugin(&plugins_dir, "cached.vst3");
        let mtime = file_modification_time(&plugin).unwrap();

        let mut db = JsonCatalog::empty(dir.path().join("db.json"));
        db.upsert(PluginRecord {
            path: plugin,
            format: PluginFormat::Vst3,
            descriptor: PluginDescriptor::new("cached", "cached", PluginClass::Unknown),
            modification_time: mtime,
            blacklist: Blacklist::Ok,
            extension_id: None,
            manifest_index: None,
        });

        let mut scanner = PluginScanner::new(Box::new(db), dir.path().join(".scanning"));
        let result = scanner.scan_sync(vec![plugins_dir]);

        assert_eq!(result.scanned, 1);
        assert_eq!(result.new, 0);
    }

    #[test]
    fn recover_crash_blacklists_pedal_plugin() {
        let dir = TempDir::new().unwrap();
        let pedal_file = dir.path().join(".scanning");

        // Simulate a previous crash: write a pedal file.
        std::fs::write(&pedal_file, "/plugins/crashy.vst3").unwrap();

        let db = JsonCatalog::empty(dir.path().join("plugins.json"));
        let mut scanner = PluginScanner::new(Box::new(db), &pedal_file);
        scanner.recover_crash();

        let catalog = scanner.into_catalog();
        assert!(catalog.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert!(!pedal_file.exists());
    }

    #[test]
    fn async_scan_with_progress() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        create_fake_plugin(&plugins_dir, "a.vst3");
        create_fake_plugin(&plugins_dir, "b.clap");

        let scanner = scanner_for(dir.path());

        let handle = scanner.scan_async(vec![plugins_dir]);

        let mut saw_discovery = false;
        let mut saw_scanning = false;
        let mut saw_complete = false;

        while let Ok(p) = handle
            .progress_rx
            .recv_timeout(std::time::Duration::from_secs(30))
        {
            match p.phase {
                ScanPhase::Discovery => saw_discovery = true,
                ScanPhase::Scanning => saw_scanning = true,
                ScanPhase::Complete => {
                    saw_complete = true;
                    break;
                }
            }
        }

        assert!(saw_discovery);
        assert!(saw_scanning);
        assert!(saw_complete);

        let result = handle
            .result_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(result.new + result.failed, 2);
    }

    /// Regression for DISC-C1 and R5: a failure that is a property of the plugin
    /// must earn a catalog entry, not just a `warn!`. `probe_and_record` used to
    /// arm and disarm the pedal unconditionally and upsert nothing on failure, so
    /// `needs_rescan` stayed true and the plugin was re-probed at full cost — the
    /// crash, the 5 s stall, or the failing `dlopen` — on every scan, forever.
    ///
    /// The dividing line is "did we learn about the plugin or about ourselves?",
    /// not severity: a broken library is as informative as a crash, while a missing
    /// `plugin-server` is not informative at all.
    #[test]
    fn plugin_failures_are_blacklistable_environmental_ones_are_not() {
        use crate::error::BridgeError;

        // Exercises the *production* classifier, not a copy of it.
        let crashed = ProbeFailure::from_bridge_error(BridgeError::ProcessCrashed);
        assert!(
            crashed.blacklistable,
            "ProcessCrashed must be blacklistable"
        );
        assert!(crashed.reason.contains("crashed"));

        let timed_out = ProbeFailure::from_bridge_error(BridgeError::Timeout {
            operation: "probe".into(),
            duration_ms: 5000,
        });
        assert!(timed_out.blacklistable, "Timeout must be blacklistable");
        assert!(timed_out.reason.contains("timed out"));

        // R5: this was the one path that recorded nothing.
        let load_failed = ProbeFailure::from_bridge_error(BridgeError::LoadFailed {
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
        assert!(
            !ProbeFailure::from_bridge_error(BridgeError::IpcError("socket".into())).blacklistable
        );
        assert!(
            !ProbeFailure::from_bridge_error(BridgeError::ProtocolMismatch {
                expected: 2,
                got: 1
            })
            .blacklistable
        );
        assert!(
            !ProbeFailure::from_bridge_error(BridgeError::ServerNotFound).blacklistable,
            "a missing plugin-server must never blacklist a plugin"
        );
    }

    /// Regression for DISC-C1 (catalog half): a blacklisted probe result must
    /// actually land in the catalog and hide the plugin from `plugins()`.
    #[test]
    fn blacklisting_from_a_failed_probe_hides_the_plugin() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        let bad = create_fake_plugin(&plugins_dir, "crashy.vst3");

        let mut db = JsonCatalog::empty(dir.path().join("db.json"));
        db.blacklist(&bad, "crashed during probe".into());

        assert!(db.is_blacklisted(&bad));
        assert_eq!(db.plugins().count(), 0);
        // The blacklist stamps the *current* mtime, so the plugin is skipped
        // while unchanged...
        assert!(db.is_blacklisted_and_unchanged(&bad));
        assert!(matches!(
            classify(&db, &bad),
            ScanDecision::Skip(ScanOutcome::Blacklisted)
        ));
    }

    /// Regression for DISC-C2: an mtime change (reinstall, vendor update)
    /// must re-admit a blacklisted plugin. `classify` used to check the raw
    /// `is_blacklisted` flag *before* `needs_rescan`, so a false positive was
    /// permanent short of hand-editing the JSON.
    #[test]
    fn mtime_change_readmits_a_blacklisted_plugin() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        let bad = create_fake_plugin(&plugins_dir, "was-crashy.vst3");

        let mut db = JsonCatalog::empty(dir.path().join("db.json"));
        db.blacklist(&bad, "crashed during probe".into());
        assert!(matches!(
            classify(&db, &bad),
            ScanDecision::Skip(ScanOutcome::Blacklisted)
        ));

        // Simulate a reinstall: the file's mtime moves.
        let record = db.get(&bad).unwrap().clone();
        db.upsert(PluginRecord {
            modification_time: record.modification_time.wrapping_sub(1),
            ..record
        });

        assert!(db.is_blacklisted(&bad), "flag is still set");
        assert!(
            !db.is_blacklisted_and_unchanged(&bad),
            "but the file changed, so it must be re-probed"
        );
        assert!(
            matches!(classify(&db, &bad), ScanDecision::Probe),
            "a changed blacklisted plugin must be re-probed, not skipped forever"
        );
    }

    /// Regression for DISC-C2: blacklisting must have an inverse.
    #[test]
    fn unblacklist_and_clear_blacklist_are_the_inverse() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        // Real files, so their mtimes are real and non-zero — the whole point
        // of `unblacklist` zeroing `modification_time` is to force a rescan.
        let a = create_fake_plugin(&plugins_dir, "a.vst3");
        let b = create_fake_plugin(&plugins_dir, "b.vst3");

        let mut db = JsonCatalog::empty(dir.path().join("db.json"));
        db.blacklist(&a, "pedal misfire".into());
        db.blacklist(&b, "pedal misfire".into());
        assert_eq!(db.blacklisted().count(), 2);
        assert_eq!(db.plugins().count(), 0);
        // Blacklisting stamps the live mtime, so the entry is "unchanged".
        assert!(!db.needs_rescan(&a));

        assert!(db.unblacklist(&a));
        assert!(!db.is_blacklisted(&a));
        // Cleared records are forced to re-probe: the blacklisted stub's
        // descriptor is a placeholder, so it must not be served as-is.
        assert!(db.needs_rescan(&a));
        assert!(matches!(classify(&db, &a), ScanDecision::Probe));
        // Second call is a no-op, not a lie.
        assert!(!db.unblacklist(&a));

        let cleared = db.clear_blacklist();
        assert_eq!(cleared, vec![b.clone()]);
        assert_eq!(db.blacklisted().count(), 0);
        assert!(db.needs_rescan(&b));
    }

    #[test]
    fn probe_plugin_fallback_returns_metadata() {
        let meta = probe_plugin_fallback(Path::new("/plugins/My Reverb.vst3"), PluginFormat::Vst3);
        assert_eq!(meta.name, "My Reverb");
        assert_eq!(meta.id, "vst3.my_reverb");
    }

    /// R5, the consequence rather than the classification: once a load failure is
    /// recorded, the scanner must stop re-probing it.
    ///
    /// This is the property that was actually broken — `blacklistable: false` meant
    /// no catalog write at all, so `needs_rescan` stayed true and every scan paid a
    /// fresh subprocess spawn to rediscover the same broken library. Asserting the
    /// classifier alone would not have caught it, because the classifier was only
    /// half the path.
    #[test]
    fn a_recorded_load_failure_is_not_reprobed() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        let broken = create_fake_plugin(&plugins_dir, "Broken.vst3");

        let mut db = JsonCatalog::empty(dir.path().join("db.json"));
        // What `probe_and_record` now does for a LoadFailed.
        let failure = ProbeFailure::from_bridge_error(crate::error::BridgeError::LoadFailed {
            path: broken.clone(),
            stage: tutti_plugin_types::LoadStage::Opening,
            reason: "dlopen failed".into(),
        });
        assert!(failure.blacklistable);
        db.blacklist(&broken, failure.reason);

        // The re-probe loop is closed.
        assert!(
            matches!(
                classify(&db, &broken),
                ScanDecision::Skip(ScanOutcome::Blacklisted)
            ),
            "a recorded load failure must be skipped, not re-probed at full \
             subprocess cost on every scan"
        );

        // Hidden from `plugins()`, but not silently: the reason is retrievable.
        assert_eq!(db.plugins().count(), 0);
        let entry = db.blacklisted().next().expect("must be listed as hidden");
        assert!(
            entry
                .blacklist
                .reason()
                .is_some_and(|r| r.contains("dlopen")),
            "a UI must be able to tell the user why the plugin vanished"
        );

        // And it is not permanent: reinstalling the plugin re-admits it.
        let record = db.get(&broken).unwrap().clone();
        db.upsert(PluginRecord {
            modification_time: record.modification_time.wrapping_sub(1),
            ..record
        });
        assert!(
            matches!(classify(&db, &broken), ScanDecision::Probe),
            "a rebuilt or reinstalled plugin must be re-probed"
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
        use super::super::fs::format_from_path;

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
