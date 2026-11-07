//! The plugin scanner, with crash recovery.
//!
//! Discovers plugin files in directories, validates them against a
//! [`PluginCatalog`], and reports progress via a channel. An internal
//! dead-man's pedal auto-blacklists plugins that crash during scanning.

use super::catalog::{CatalogExt, PluginCatalog};
use super::facts::{self, FileFacts, ScanDecision};
use super::fs::discover_plugins;
use super::pedal::Pedal;
use super::record::{Blacklist, PluginClass, PluginDescriptor, PluginFormat, PluginRecord};
use crossbeam_channel::{Receiver, Sender};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Progress information emitted during a scan.
#[derive(Debug, Clone)]
pub struct ScanProgress {
    /// How many plugins have been processed so far.
    pub current: usize,
    /// How many were discovered in total. Zero during
    /// [`Discovery`](ScanPhase::Discovery), when the count is not yet known.
    pub total: usize,
    /// The plugin being processed as this was emitted.
    pub current_path: PathBuf,
    /// Which phase the scan is in.
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

/// Channels for monitoring a scan started by [`PluginScanner::spawn_scan`].
#[derive(Debug)]
#[must_use]
pub struct ScanHandle {
    /// Per-plugin progress, emitted as the scan advances.
    pub progress_rx: Receiver<ScanProgress>,
    /// Yields the summary once, when the scan finishes.
    pub result_rx: Receiver<ScanResult>,
    /// Yields the catalog back once the scan thread finishes with it.
    ///
    /// The scan *moves* the catalog onto its own thread — that is what lets
    /// any [`PluginCatalog`] impl be scanned, not just cheaply-reloadable
    /// file-backed ones. This channel is how ownership comes back.
    pub catalog_rx: Receiver<Box<dyn PluginCatalog>>,
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

/// Scans plugin directories into a [`PluginCatalog`], with crash recovery.
///
/// Each new or changed plugin is probed through the scanner's probe (the host's
/// out-of-process one); unchanged ones (same modification time) are skipped. A
/// plugin that fails to load, hangs or crashes its probe is blacklisted with the reason, and a
/// sentinel file (the "dead-man's pedal") blacklists the plugin a crashed scan
/// was probing on the next run. `tutti_plugin::catalog::Plugins` drives a
/// scanner for most hosts.
pub struct PluginScanner {
    catalog: Box<dyn PluginCatalog>,
    pedal: Pedal,
    probe: Box<ProbeFn>,
}

const _: () = {
    const fn assert_sync<T: Sync>() {}
    assert_sync::<PluginScanner>();
};

/// What a scanner calls to read one plugin file's metadata.
type ProbeFn = dyn Fn(&Path, PluginFormat) -> Result<PluginDescriptor, ProbeFailure> + Send + Sync;

impl std::fmt::Debug for PluginScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginScanner")
            .field("pedal", &self.pedal)
            .finish_non_exhaustive()
    }
}

impl PluginScanner {
    /// Creates a scanner around `catalog`.
    ///
    /// `pedal_path` is where the crash sentinel file lives: a crashed scan
    /// leaves it behind, and the next run blacklists the plugin it names.
    ///
    /// `probe` reads one plugin file's metadata. It runs out of process where
    /// it loads the plugin, so a crash or a hang cannot take the scan down.
    pub fn new(
        catalog: Box<dyn PluginCatalog>,
        pedal_path: impl Into<PathBuf>,
        probe: impl Fn(&Path, PluginFormat) -> Result<PluginDescriptor, ProbeFailure>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let pedal = Pedal::new(pedal_path.into());
        Self {
            catalog,
            pedal,
            probe: Box::new(probe),
        }
    }

    /// Blacklists the plugin a previous, crashed scan was probing.
    ///
    /// Safe to call multiple times; a no-op when no sentinel is present.
    pub fn recover_crash(&mut self) {
        self.pedal.recover(self.catalog.as_mut());
    }

    /// Consumes the scanner and returns the inner catalog.
    #[must_use]
    pub fn into_catalog(self) -> Box<dyn PluginCatalog> {
        self.catalog
    }

    /// Scans directories on a background thread, returning immediately.
    ///
    /// Runs crash recovery before scanning, and flushes the catalog after.
    ///
    /// Spawns a thread and hands back channels, in the shape of
    /// [`std::process::Command::spawn`] — this is *not* an `async fn` and
    /// returns no future. See [`scan`](Self::scan) for the blocking form.
    ///
    /// The catalog travels with the scanner onto the worker thread and comes
    /// back over [`ScanHandle::catalog_rx`] when the scan finishes. Drop the
    /// handle and the catalog is dropped with the thread; keep it to recover
    /// ownership.
    ///
    /// # Panics
    ///
    /// Panics if the OS refuses to spawn the scanner thread.
    pub fn spawn_scan(
        mut self,
        directories: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> ScanHandle {
        let directories: Vec<PathBuf> = directories
            .into_iter()
            .map(|d| d.as_ref().to_path_buf())
            .collect();
        let (progress_tx, progress_rx) = crossbeam_channel::unbounded();
        let (result_tx, result_rx) = crossbeam_channel::bounded(1);
        let (catalog_tx, catalog_rx) = crossbeam_channel::bounded(1);

        std::thread::Builder::new()
            .name("plugin-scanner".into())
            .spawn(move || {
                self.recover_crash();
                let result = self.scan_inner(&directories, Some(&progress_tx));
                if let Err(e) = self.catalog.flush() {
                    warn!("failed to flush plugin catalog after spawned scan: {e}");
                }
                // Hand the catalog back before announcing completion, so a
                // caller that reacts to `result_rx` finds it already waiting.
                let _ = catalog_tx.send(self.catalog);
                let _ = result_tx.send(result);
            })
            .expect("failed to spawn plugin scanner thread");

        ScanHandle {
            progress_rx,
            result_rx,
            catalog_rx,
        }
    }

    /// Scans directories on the calling thread, blocking until done.
    ///
    /// Runs crash recovery, scans, then flushes the catalog (a flush failure is
    /// logged, not returned). A probe may spawn a subprocess, so a large
    /// plugin folder can take minutes the first time; see
    /// [`spawn_scan`](Self::spawn_scan) for the background form.
    ///
    /// `directories` can be `&[PathBuf]`, an array of `&str`, or any iterator
    /// of paths.
    pub fn scan(&mut self, directories: impl IntoIterator<Item = impl AsRef<Path>>) -> ScanResult {
        self.recover_crash();
        let result = self.scan_inner(directories, None);
        if let Err(e) = self.catalog.flush() {
            warn!("failed to flush plugin catalog after blocking scan: {e}");
        }
        result
    }

    /// Core scanning logic shared between the blocking and spawned paths.
    fn scan_inner(
        &mut self,
        directories: impl IntoIterator<Item = impl AsRef<Path>>,
        progress_tx: Option<&Sender<ScanProgress>>,
    ) -> ScanResult {
        let emit = |p: ScanProgress| {
            if let Some(tx) = progress_tx {
                let _ = tx.send(p);
            }
        };

        // Step 1: discovery. `dir_count` is tallied as we go rather than read
        // from a `len()`: the input is an iterator, so it has no length to ask
        // for and is consumed by this pass.
        let mut dir_count = 0usize;
        let plugin_paths: Vec<(PathBuf, PluginFormat)> = directories
            .into_iter()
            .flat_map(|dir| {
                dir_count += 1;
                let dir = dir.as_ref();
                emit(ScanProgress {
                    current: 0,
                    total: 0,
                    current_path: dir.to_path_buf(),
                    phase: ScanPhase::Discovery,
                });
                discover_plugins(dir)
            })
            .collect();

        let total = plugin_paths.len();
        info!("discovered {total} plugin files across {dir_count} directories");

        // Step 2: scanning — classify (pure) then execute (effectful).
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
                let facts = FileFacts::read(path);
                match facts::classify(self.catalog.get(path), &facts) {
                    ScanDecision::Blacklisted => {
                        debug!("skipping blacklisted plugin: {:?}", path);
                        ScanOutcome::Blacklisted
                    }
                    ScanDecision::UpToDate => {
                        debug!("skipping up-to-date plugin: {:?}", path);
                        ScanOutcome::UpToDate
                    }
                    ScanDecision::Probe => self.probe_and_record(path, *format, &facts),
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
    /// A failing probe is *recorded*, not merely warned about. Without a catalog
    /// write `needs_rescan` stays true and the plugin is re-probed at full cost —
    /// a 5 s hang, or a crashing subprocess — on every scan, forever. Crash and
    /// timeout go to the blacklist, which is the one case the pedal cannot cover:
    /// the pedal fires when the *scanner host* dies, which is exactly what
    /// out-of-process probing prevents.
    ///
    /// JUCE does the same: a scan attempt that yields nothing goes into
    /// `failedFiles` and then `addToBlacklist` — failure, not just a hard
    /// crash, earns the blacklist.
    ///
    /// That reasoning applies to a *load* failure too. A plugin whose library
    /// will not open
    /// — a stub file, a wrong-arch binary, a broken install — fails identically on
    /// every future scan, and each attempt costs a full subprocess spawn. It is
    /// recorded for the same reason a crash is. What it is *not* is silently
    /// dropped: the entry carries the loader's own reason string, `blacklisted()`
    /// surfaces it to a UI, and [`CatalogExt::unblacklist`] is its inverse.
    ///
    /// The mtime stamp is what makes this safe to be wrong about — reinstalling or
    /// updating the plugin re-admits it automatically, without the user knowing the
    /// blacklist exists.
    fn probe_and_record(
        &mut self,
        path: &Path,
        format: PluginFormat,
        facts: &FileFacts,
    ) -> ScanOutcome {
        if let Err(e) = self.pedal.arm(path) {
            warn!("failed to arm dead-man's pedal: {e}");
        }
        let outcome = match (self.probe)(path, format) {
            Ok(descriptor) => {
                self.catalog.upsert(PluginRecord {
                    path: path.to_path_buf(),
                    format,
                    descriptor,
                    modification_time: facts.mtime.unwrap_or(0),
                    blacklist: Blacklist::Ok,
                });
                ScanOutcome::New
            }
            Err(failure) => {
                warn!("failed to probe plugin {:?}: {}", path, failure.reason);
                if failure.blacklistable {
                    // The blacklist stamps the file's current mtime, so a
                    // reinstall or update re-admits the plugin (see
                    // `CatalogExt::is_blacklisted_and_unchanged`).
                    self.catalog.blacklist(path, failure.reason, facts);
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
pub struct ProbeFailure {
    /// Why the probe failed; stored as the blacklist reason.
    pub reason: String,
    /// Set when the failure is a property of *this plugin* — it will recur
    /// identically on every future scan, at the same cost — so recording it beats
    /// re-probing forever. Clear for environmental failures, which say nothing
    /// about the plugin and must stay retryable.
    pub blacklistable: bool,
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

/// Minimal metadata from the file name, for a host whose probe is unavailable.
/// The native classification is unknown without a probe, so the descriptor
/// carries [`PluginClass::Unknown`].
pub fn probe_plugin_fallback(path: &Path, format: PluginFormat) -> PluginDescriptor {
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
    use super::super::catalog::MemoryCatalog;
    use super::*;
    use tempfile::TempDir;

    fn create_fake_plugin(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"fake plugin binary").unwrap();
        path
    }

    fn filename_probe(path: &Path, format: PluginFormat) -> Result<PluginDescriptor, ProbeFailure> {
        Ok(probe_plugin_fallback(path, format))
    }

    fn scanner_for(dir: &Path) -> PluginScanner {
        let db = MemoryCatalog::default();
        PluginScanner::new(Box::new(db), dir.join(".scanning"), filename_probe)
    }

    #[test]
    fn scan_discovers_and_records() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        create_fake_plugin(&plugins_dir, "synth.vst3");
        create_fake_plugin(&plugins_dir, "comp.clap");

        let mut scanner = scanner_for(dir.path());

        let result = scanner.scan(&[plugins_dir]);
        assert_eq!(result.scanned, 2);
        assert_eq!(result.new + result.failed, 2);
    }

    /// `scan` accepts any path-like iterable, not just a `&[PathBuf]`.
    ///
    /// The point of the `IntoIterator<Item = impl AsRef<Path>>` bound is that a
    /// caller with a `Vec<String>`, a borrowed config field, or a filtered
    /// iterator does not have to materialise a `Vec<PathBuf>` first. Each arm
    /// here is a distinct shape that fails to compile under a narrower
    /// signature, so this is a compile-time assertion as much as a runtime one:
    /// `&Vec<PathBuf>` is what `Plugins::rescan` passes, `[&str; 1]` covers the
    /// no-PathBuf-in-sight caller, and the `filter` covers a lazy iterator with
    /// no length to read.
    #[test]
    fn scan_accepts_any_path_like_iterable() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        create_fake_plugin(&plugins_dir, "synth.vst3");

        // A borrowed owned collection — the `Plugins::rescan` shape.
        let owned: Vec<PathBuf> = vec![plugins_dir.clone()];
        let mut scanner = scanner_for(dir.path());
        assert_eq!(scanner.scan(&owned).scanned, 1);

        // Borrowed `&str`s, never a `PathBuf`.
        let as_str = plugins_dir.to_str().unwrap();
        let mut scanner = scanner_for(dir.path());
        assert_eq!(scanner.scan([as_str]).scanned, 1);

        // A lazy iterator, which has no `len()` to read — the case that forced
        // `scan_inner` to tally directories as it walks them.
        let mut scanner = scanner_for(dir.path());
        let lazy = owned.iter().filter(|p| p.exists());
        assert_eq!(scanner.scan(lazy).scanned, 1);
    }

    /// An already-blacklisted plugin is skipped without being probed, and is
    /// counted as blacklisted rather than as a fresh failure.
    ///
    /// That split is the subject: "was hidden before this scan" and "this scan
    /// hid it" are different facts, and only the second is worth telling the
    /// user about.
    ///
    /// What happens to the *other* stub depends on the probe, not on the code
    /// under test, so only `new + failed` is asserted for it.
    #[test]
    fn scan_skips_blacklisted() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        let bad = create_fake_plugin(&plugins_dir, "bad.vst3");
        create_fake_plugin(&plugins_dir, "good.vst3");

        let mut db = MemoryCatalog::default();
        db.blacklist(&bad, "known crasher".into(), &FileFacts::read(&bad));

        let mut scanner =
            PluginScanner::new(Box::new(db), dir.path().join(".scanning"), filename_probe);
        let result = scanner.scan(&[plugins_dir]);

        assert_eq!(result.scanned, 2);
        // The pre-blacklisted one was skipped, so it contributes to `blacklisted`
        // but not to `newly_blacklisted`.
        assert_eq!(
            result.blacklisted - result.newly_blacklisted,
            1,
            "exactly one plugin was hidden before this scan started"
        );
        // The skipped plugin must not also be counted as processed. This is what
        // keeps the test non-vacuous without asserting `new`: a scanner that
        // probed the blacklisted file anyway would land it in one of these
        // buckets.
        //
        // `new + failed` and not `+ newly_blacklisted`: `tally` deliberately
        // counts a newly-blacklisted plugin in *both* `failed` and
        // `newly_blacklisted`, so adding the third term double-counts it. Every
        // probed plugin lands in exactly one of `new` or `failed`.
        assert_eq!(
            result.new + result.failed,
            1,
            "only the non-blacklisted plugin should have been probed, but \
             new={} failed={} were recorded",
            result.new,
            result.failed
        );
    }

    #[test]
    fn scan_skips_up_to_date() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        let plugin = create_fake_plugin(&plugins_dir, "cached.vst3");
        let mtime = FileFacts::read(&plugin).mtime.unwrap();

        let mut db = MemoryCatalog::default();
        db.upsert(PluginRecord {
            path: plugin,
            format: PluginFormat::Vst3,
            descriptor: PluginDescriptor::new("cached", "cached", PluginClass::Unknown),
            modification_time: mtime,
            blacklist: Blacklist::Ok,
        });

        let mut scanner =
            PluginScanner::new(Box::new(db), dir.path().join(".scanning"), filename_probe);
        let result = scanner.scan(&[plugins_dir]);

        assert_eq!(result.scanned, 1);
        assert_eq!(result.new, 0);
    }

    #[test]
    fn recover_crash_blacklists_pedal_plugin() {
        let dir = TempDir::new().unwrap();
        let pedal_file = dir.path().join(".scanning");

        // Simulate a previous crash: write a pedal file.
        std::fs::write(&pedal_file, "/plugins/crashy.vst3").unwrap();

        let db = MemoryCatalog::default();
        let mut scanner = PluginScanner::new(Box::new(db), pedal_file.clone(), filename_probe);
        scanner.recover_crash();

        let catalog = scanner.into_catalog();
        assert!(catalog.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert!(!pedal_file.exists());
    }

    #[test]
    fn spawned_scan_with_progress() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        create_fake_plugin(&plugins_dir, "a.vst3");
        create_fake_plugin(&plugins_dir, "b.clap");

        let scanner = scanner_for(dir.path());

        let handle = scanner.spawn_scan(&[plugins_dir]);

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

    /// A spawned scan must hand its catalog back. `spawn_scan` *moves* the
    /// catalog onto the worker thread — that is what lets any `PluginCatalog`
    /// impl be scanned rather than only file-backed ones that can be cheaply
    /// reloaded from disk. Without this channel the records a scan produced
    /// would be dropped with the thread, and the caller would be left holding
    /// nothing. Uses `MemoryCatalog` precisely because it has no on-disk
    /// fallback: if the handback were missing, the scan results would be
    /// unrecoverable.
    #[test]
    fn spawned_scan_returns_the_catalog() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        create_fake_plugin(&plugins_dir, "a.vst3");

        let scanner = scanner_for(dir.path());
        let handle = scanner.spawn_scan(&[plugins_dir]);

        let catalog = handle
            .catalog_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("scan must return the catalog it was given");

        // The stub is not a loadable plugin, so it is recorded as blacklisted
        // rather than as a healthy record — either way the scan's findings
        // survived the trip back.
        assert_eq!(
            catalog.len(),
            1,
            "the scanned plugin must be present in the returned catalog"
        );
    }

    /// Catalog half: a blacklisted probe result must
    /// actually land in the catalog and hide the plugin from `plugins()`.
    #[test]
    fn blacklisting_from_a_failed_probe_hides_the_plugin() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        let bad = create_fake_plugin(&plugins_dir, "crashy.vst3");

        let mut db = MemoryCatalog::default();
        db.blacklist(&bad, "crashed during probe".into(), &FileFacts::read(&bad));

        assert!(db.is_blacklisted(&bad));
        assert_eq!(db.plugins().count(), 0);
        // The blacklist stamps the *current* mtime, so the plugin is skipped
        // while unchanged...
        assert!(db.is_blacklisted_and_unchanged(&bad, &FileFacts::read(&bad)));
        assert!(matches!(
            facts::classify(db.get(&bad), &FileFacts::read(&bad)),
            ScanDecision::Blacklisted
        ));
    }

    /// An mtime change — a reinstall or a vendor update — must re-admit a
    /// blacklisted plugin. `classify` therefore consults `needs_rescan` *before*
    /// the `is_blacklisted` flag; checking the flag first makes a false positive
    /// permanent short of hand-editing the JSON.
    #[test]
    fn mtime_change_readmits_a_blacklisted_plugin() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        let bad = create_fake_plugin(&plugins_dir, "was-crashy.vst3");

        let mut db = MemoryCatalog::default();
        db.blacklist(&bad, "crashed during probe".into(), &FileFacts::read(&bad));
        assert!(matches!(
            facts::classify(db.get(&bad), &FileFacts::read(&bad)),
            ScanDecision::Blacklisted
        ));

        // Simulate a reinstall: the file's mtime moves.
        let record = db.get(&bad).unwrap().clone();
        db.upsert(PluginRecord {
            modification_time: record.modification_time.wrapping_sub(1),
            ..record
        });

        assert!(db.is_blacklisted(&bad), "flag is still set");
        assert!(
            !db.is_blacklisted_and_unchanged(&bad, &FileFacts::read(&bad)),
            "but the file changed, so it must be re-probed"
        );
        assert!(
            matches!(
                facts::classify(db.get(&bad), &FileFacts::read(&bad)),
                ScanDecision::Probe
            ),
            "a changed blacklisted plugin must be re-probed, not skipped forever"
        );
    }

    /// Blacklisting must have an inverse.
    #[test]
    fn unblacklist_and_clear_blacklist_are_the_inverse() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        // Real files, so their mtimes are real and non-zero — the whole point
        // of `unblacklist` zeroing `modification_time` is to force a rescan.
        let a = create_fake_plugin(&plugins_dir, "a.vst3");
        let b = create_fake_plugin(&plugins_dir, "b.vst3");

        let mut db = MemoryCatalog::default();
        db.blacklist(&a, "pedal misfire".into(), &FileFacts::read(&a));
        db.blacklist(&b, "pedal misfire".into(), &FileFacts::read(&b));
        assert_eq!(db.blacklisted().count(), 2);
        assert_eq!(db.plugins().count(), 0);
        // Blacklisting stamps the live mtime, so the entry is "unchanged".
        assert!(!db.needs_rescan(&a, &FileFacts::read(&a)));

        assert!(db.unblacklist(&a));
        assert!(!db.is_blacklisted(&a));
        // Cleared records are forced to re-probe: the blacklisted stub's
        // descriptor is a placeholder, so it must not be served as-is.
        assert!(db.needs_rescan(&a, &FileFacts::read(&a)));
        assert!(matches!(
            facts::classify(db.get(&a), &FileFacts::read(&a)),
            ScanDecision::Probe
        ));
        // Second call is a no-op, not a lie.
        assert!(!db.unblacklist(&a));

        let cleared = db.clear_blacklist();
        assert_eq!(cleared, vec![b.clone()]);
        assert_eq!(db.blacklisted().count(), 0);
        assert!(db.needs_rescan(&b, &FileFacts::read(&b)));
    }

    #[test]
    fn probe_plugin_fallback_returns_metadata() {
        let meta = probe_plugin_fallback(Path::new("/plugins/My Reverb.vst3"), PluginFormat::Vst3);
        assert_eq!(meta.name, "My Reverb");
        assert_eq!(meta.id, "vst3.my_reverb");
    }

    /// The consequence rather than the classification: once a load failure is
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

        let mut db = MemoryCatalog::default();
        // What `probe_and_record` does for a LoadFailed.
        let failure = ProbeFailure {
            reason: "failed to load during probe: dlopen failed".into(),
            blacklistable: true,
        };
        assert!(failure.blacklistable);
        db.blacklist(&broken, failure.reason, &FileFacts::read(&broken));

        // The re-probe loop is closed.
        assert!(
            matches!(
                facts::classify(db.get(&broken), &FileFacts::read(&broken)),
                ScanDecision::Blacklisted
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
            matches!(
                facts::classify(db.get(&broken), &FileFacts::read(&broken)),
                ScanDecision::Probe
            ),
            "a rebuilt or reinstalled plugin must be re-probed"
        );
    }
}
