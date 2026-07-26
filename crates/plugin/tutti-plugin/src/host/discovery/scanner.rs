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
    /// Plugins that failed to load.
    pub failed: usize,
    /// Plugins that were blacklisted (previously or newly).
    pub blacklisted: usize,
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
            Err(reason) => {
                warn!("failed to probe plugin {:?}: {reason}", path);
                ScanOutcome::Failed
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
    Blacklisted,
    UpToDate,
}

/// Pure classification: inspects the catalog, no side effects.
fn classify(catalog: &dyn PluginCatalog, path: &Path) -> ScanDecision {
    if catalog.is_blacklisted(path) {
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
        }
    }

    fn tally(mut self, outcome: ScanOutcome) -> Self {
        match outcome {
            ScanOutcome::New => self.new += 1,
            ScanOutcome::Failed => self.failed += 1,
            ScanOutcome::Blacklisted => self.blacklisted += 1,
            ScanOutcome::UpToDate => {}
        }
        self
    }
}

/// Probe a plugin by spawning a plugin-server subprocess and querying its
/// metadata. Falls back to filename-based metadata if the plugin-server
/// binary is not available.
fn probe_plugin(path: &Path, format: PluginFormat) -> Result<PluginDescriptor, String> {
    match crate::host::subprocess::probe_metadata(path) {
        Ok(descriptor) => Ok(descriptor),
        Err(crate::error::BridgeError::ServerNotFound) => {
            debug!(
                "plugin-server not available, using filename metadata for {:?}",
                path
            );
            Ok(probe_plugin_fallback(path, format))
        }
        Err(e) => Err(e.to_string()),
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

    #[test]
    fn probe_plugin_fallback_returns_metadata() {
        let meta = probe_plugin_fallback(Path::new("/plugins/My Reverb.vst3"), PluginFormat::Vst3);
        assert_eq!(meta.name, "My Reverb");
        assert_eq!(meta.id, "vst3.my_reverb");
    }

    /// Without a `plugin-server` binary, `probe_plugin` must degrade to filename
    /// metadata rather than fail — and must say so in the class it reports.
    ///
    /// This is the path a plain `cargo test` actually takes (the server is a
    /// separate binary and usually isn't built), so it is the one worth pinning
    /// hermetically. `Unknown` is the honest answer here: the name and id come
    /// from the filename, but nothing has inspected the plugin, so claiming a
    /// category would be a fabrication.
    #[test]
    fn probe_without_a_server_falls_back_to_filename_metadata() {
        let dir = TempDir::new().unwrap();
        let path = create_fake_plugin(dir.path(), "TAL-NoiseMaker.vst3");

        let descriptor = probe_plugin(&path, PluginFormat::Vst3)
            .expect("a missing plugin-server is a fallback, not an error");

        assert_eq!(descriptor.name, "TAL-NoiseMaker");
        assert_eq!(descriptor.id, "vst3.tal-noisemaker");
        assert!(
            matches!(descriptor.class, PluginClass::Unknown),
            "an unprobed plugin must not claim a category, got {:?}",
            descriptor.class
        );
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
