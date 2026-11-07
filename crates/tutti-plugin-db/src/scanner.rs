//! The plugin scanner, with crash recovery.
//!
//! A scan works on a snapshot of the database: a worker discovers plugin files,
//! probes the new or changed ones and sends a [`Delta`] per file. The owner of
//! the [`PluginDb`] applies them as they arrive ([`Scan::poll`]), so the database
//! stays readable for the whole scan. An internal dead-man's pedal
//! auto-blacklists a plugin that takes the host down while it is probed.

use super::facts::{self, FileFacts, ScanDecision};
use super::format::{PluginClass, PluginDescriptor, PluginFormat};
use super::fs::discover;
use super::pedal::Pedal;
use super::table::{Delta, FileRow, PluginDb};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

/// What a scanner calls to read one plugin file's metadata.
type ProbeFn = dyn Fn(&Path, PluginFormat) -> Result<PluginDescriptor, ProbeFailure> + Send + Sync;

/// Scans plugin directories into a [`PluginDb`], with crash recovery.
///
/// Each new or changed file is probed through the scanner's probe (the host's
/// out-of-process one); unchanged ones (same modification time) are skipped. A
/// plugin that fails to load, hangs or crashes its probe is blacklisted with the
/// reason, and a sentinel file (the "dead-man's pedal") blacklists the plugin a
/// crashed scan was probing on the next run. `tutti_plugin::catalog::Plugins`
/// drives a scanner for most hosts.
///
/// A scanner holds no database. Every scan takes the database it reads from and
/// writes to, so one scanner serves any number of them.
#[derive(Clone)]
pub struct Scanner {
    pedal: Pedal,
    probe: Arc<ProbeFn>,
}

impl std::fmt::Debug for Scanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scanner")
            .field("pedal", &self.pedal)
            .finish_non_exhaustive()
    }
}

impl Scanner {
    /// Creates a scanner.
    ///
    /// `pedal_path` is where the crash sentinel file lives: a crashed scan
    /// leaves it behind, and the next run blacklists the plugin it names.
    ///
    /// `probe` reads one plugin file's metadata. It runs out of process where
    /// it loads the plugin, so a crash or a hang cannot take the scan down.
    pub fn new(
        pedal_path: impl Into<PathBuf>,
        probe: impl Fn(&Path, PluginFormat) -> Result<PluginDescriptor, ProbeFailure>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            pedal: Pedal::new(pedal_path),
            probe: Arc::new(probe),
        }
    }

    /// Blacklists the plugin a previous, crashed scan was probing, in `db`, and
    /// hands `db` to `save` before the sentinel is removed. Returns the file
    /// that was blacklisted.
    ///
    /// Safe to call multiple times; a no-op when no sentinel is present. Every
    /// scan calls it first.
    pub fn recover_crash(
        &self,
        db: &mut PluginDb,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Option<PathBuf> {
        self.pedal.recover(db, save)
    }

    /// Scans directories on a background thread, returning immediately.
    ///
    /// Runs crash recovery on the calling thread first (see
    /// [`recover_crash`](Self::recover_crash)), then hands the worker a snapshot
    /// of `db`. The worker's findings come back as deltas: call
    /// [`Scan::poll`] with the database to apply them. Until it does, `db` is
    /// unchanged and fully usable.
    ///
    /// Spawns a thread and hands back channels, in the shape of
    /// [`std::process::Command::spawn`]: this is not an `async fn` and returns
    /// no future. See [`scan`](Self::scan) for the blocking form. Dropping the
    /// [`Scan`] stops the worker after the file it is probing.
    ///
    /// # Panics
    ///
    /// Panics if the OS refuses to spawn the scanner thread.
    pub fn spawn(
        &self,
        db: &mut PluginDb,
        directories: impl IntoIterator<Item = impl AsRef<Path>>,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Scan {
        let job = self.job(db, directories, save);
        let (progress_tx, progress_rx) = crossbeam_channel::unbounded();
        let (tx, rx) = crossbeam_channel::unbounded();

        std::thread::Builder::new()
            .name("plugin-scanner".into())
            .spawn(move || {
                let result = job.run(Some(&progress_tx), |delta| {
                    tx.send(Message::Delta(delta)).is_ok()
                });
                let _ = tx.send(Message::Done(result));
            })
            .expect("failed to spawn plugin scanner thread");

        Scan { progress_rx, rx }
    }

    /// Scans directories on the calling thread into `db`, blocking until done.
    ///
    /// Runs crash recovery first. A probe may spawn a subprocess, so a large
    /// plugin folder can take minutes the first time; see
    /// [`spawn`](Self::spawn) for the background form.
    ///
    /// `directories` can be `&[PathBuf]`, an array of `&str`, or any iterator
    /// of paths.
    pub fn scan(
        &self,
        db: &mut PluginDb,
        directories: impl IntoIterator<Item = impl AsRef<Path>>,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> ScanResult {
        let job = self.job(db, directories, save);
        job.run(None, |delta| {
            db.apply(delta);
            true
        })
    }

    fn job(
        &self,
        db: &mut PluginDb,
        directories: impl IntoIterator<Item = impl AsRef<Path>>,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Job {
        self.recover_crash(db, save);
        Job {
            snapshot: db.clone(),
            directories: directories
                .into_iter()
                .map(|d| d.as_ref().to_path_buf())
                .collect(),
            pedal: self.pedal.clone(),
            probe: Arc::clone(&self.probe),
        }
    }
}

/// A scan in flight on its worker thread.
///
/// The worker owns a snapshot of the database; this is the other end. It holds
/// no database, so the owner reads its own throughout and applies what arrives
/// when it chooses.
#[derive(Debug)]
#[must_use]
pub struct Scan {
    /// Per-plugin progress, emitted as the scan advances.
    pub progress_rx: Receiver<ScanProgress>,
    rx: Receiver<Message>,
}

/// What the worker sends the owner.
#[derive(Debug)]
enum Message {
    Delta(Delta),
    Done(ScanResult),
}

/// Where a scan stands after [`Scan::poll`].
#[derive(Debug)]
pub enum ScanPoll {
    /// The worker is still going; every delta sent so far has been applied.
    Running,
    /// The worker finished and every delta has been applied. Drop the [`Scan`];
    /// polling it again reports [`Died`](Self::Died).
    Done(ScanResult),
    /// The worker thread ended without finishing, so a probe panicked. Deltas
    /// sent before that have been applied.
    Died,
}

impl Scan {
    /// Applies every delta the worker has sent to `db` and reports where the
    /// scan stands. Never blocks.
    ///
    /// This and [`wait`](Self::wait) are the only places a scan changes a
    /// database, and each delta goes through [`PluginDb::apply`].
    pub fn poll(&self, db: &mut PluginDb) -> ScanPoll {
        loop {
            match self.rx.try_recv() {
                Ok(Message::Delta(delta)) => db.apply(delta),
                Ok(Message::Done(result)) => return ScanPoll::Done(result),
                Err(TryRecvError::Empty) => return ScanPoll::Running,
                Err(TryRecvError::Disconnected) => return ScanPoll::Died,
            }
        }
    }

    /// Blocks until the scan ends, applying deltas to `db` as they arrive.
    /// Returns `None` if the worker died without finishing.
    pub fn wait(self, db: &mut PluginDb) -> Option<ScanResult> {
        while let Ok(message) = self.rx.recv() {
            match message {
                Message::Delta(delta) => db.apply(delta),
                Message::Done(result) => return Some(result),
            }
        }
        None
    }
}

/// One scan's inputs, moved onto its worker.
struct Job {
    snapshot: PluginDb,
    directories: Vec<PathBuf>,
    pedal: Pedal,
    probe: Arc<ProbeFn>,
}

impl Job {
    /// Walks, classifies (pure), then probes (effectful), passing each delta to
    /// `sink`. A sink that returns `false` stops the scan.
    fn run(
        &self,
        progress_tx: Option<&Sender<ScanProgress>>,
        mut sink: impl FnMut(Delta) -> bool,
    ) -> ScanResult {
        let emit = |p: ScanProgress| {
            if let Some(tx) = progress_tx {
                let _ = tx.send(p);
            }
        };

        for dir in &self.directories {
            emit(ScanProgress {
                current: 0,
                total: 0,
                current_path: dir.clone(),
                phase: ScanPhase::Discovery,
            });
        }
        let plugin_paths = discover(&self.directories);

        let total = plugin_paths.len();
        info!(
            "discovered {total} plugin files across {} directories",
            self.directories.len()
        );

        let mut result = ScanResult::with_total(total);
        for (i, (path, format)) in plugin_paths.iter().enumerate() {
            emit(ScanProgress {
                current: i + 1,
                total,
                current_path: path.clone(),
                phase: ScanPhase::Scanning,
            });
            let facts = FileFacts::read(path);
            let (outcome, delta) = match facts::classify(self.snapshot.row(path), &facts) {
                ScanDecision::Blacklisted => {
                    debug!("skipping blacklisted plugin: {:?}", path);
                    (ScanOutcome::Blacklisted, None)
                }
                ScanDecision::UpToDate => {
                    debug!("skipping up-to-date plugin: {:?}", path);
                    (ScanOutcome::UpToDate, None)
                }
                ScanDecision::Probe => self.probe_one(path, *format, &facts),
            };
            result = result.tally(outcome);
            if let Some(delta) = delta
                && !sink(delta)
            {
                return result;
            }
        }

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

    /// Probes one file with pedal protection and returns the row to record.
    ///
    /// A failing probe is recorded, not merely warned about. Without a row the
    /// file stays out of date and is re-probed at full cost (a 5 s hang, or a
    /// crashing subprocess) on every scan, forever. A crash, a timeout and a
    /// library that will not load are properties of the plugin: each recurs
    /// identically, so each earns the blacklist, with the loader's own reason for
    /// a UI to show. The pedal covers the other case, the scanning host itself
    /// dying, which out-of-process probing cannot prevent.
    ///
    /// The row carries the file's time, so reinstalling or updating the plugin
    /// re-admits it without the user knowing the blacklist existed.
    fn probe_one(
        &self,
        path: &Path,
        format: PluginFormat,
        facts: &FileFacts,
    ) -> (ScanOutcome, Option<Delta>) {
        if let Err(e) = self.pedal.arm(path) {
            warn!("failed to arm dead-man's pedal: {e}");
        }
        let probed = (self.probe)(path, format);
        self.pedal.disarm();
        match probed {
            Ok(descriptor) => (
                ScanOutcome::New,
                Some(Delta::put(
                    path,
                    FileRow::probed(format, facts, vec![descriptor]),
                )),
            ),
            Err(failure) if failure.blacklistable => {
                warn!("failed to probe plugin {:?}: {}", path, failure.reason);
                (
                    ScanOutcome::NewlyBlacklisted,
                    Some(Delta::put(
                        path,
                        FileRow::blacklisted(failure.reason, facts),
                    )),
                )
            }
            // Environmental (no plugin-server, an IO error, a protocol
            // mismatch): says nothing about the plugin, so record nothing and
            // retry once the environment is fixed.
            Err(failure) => {
                warn!("failed to probe plugin {:?}: {}", path, failure.reason);
                (ScanOutcome::Failed, None)
            }
        }
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
    /// and point at [`PluginDb::unblacklist`] / [`PluginDb::clear_blacklist`].
    pub newly_blacklisted: usize,
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
    use super::*;
    use crate::table::FileState;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;

    fn create_fake_plugin(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"fake plugin binary").unwrap();
        path
    }

    fn set_mtime(path: &Path, secs: u64) {
        std::fs::File::open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn filename_probe(path: &Path, format: PluginFormat) -> Result<PluginDescriptor, ProbeFailure> {
        Ok(probe_plugin_fallback(path, format))
    }

    fn no_save(_: &PluginDb) -> std::io::Result<()> {
        Ok(())
    }

    fn scanner_for(dir: &Path) -> Scanner {
        Scanner::new(dir.join(".scanning"), filename_probe)
    }

    /// A scanner whose probe counts its calls and answers `answer`.
    fn counting_scanner(
        dir: &Path,
        answer: fn(&Path) -> Result<PluginDescriptor, ProbeFailure>,
    ) -> (Scanner, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let scanner = Scanner::new(dir.join(".scanning"), move |path, _| {
            seen.fetch_add(1, Ordering::SeqCst);
            answer(path)
        });
        (scanner, calls)
    }

    fn named(path: &Path) -> Result<PluginDescriptor, ProbeFailure> {
        Ok(probe_plugin_fallback(path, PluginFormat::Clap))
    }

    fn crashes(_: &Path) -> Result<PluginDescriptor, ProbeFailure> {
        Err(ProbeFailure {
            reason: "crashed during probe".into(),
            blacklistable: true,
        })
    }

    fn plugins_dir(dir: &TempDir) -> PathBuf {
        let plugins = dir.path().join("plugins");
        std::fs::create_dir(&plugins).unwrap();
        plugins
    }

    #[test]
    fn scan_discovers_and_records() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "synth.vst3");
        create_fake_plugin(&plugins_dir, "comp.clap");

        let mut db = PluginDb::new();
        let result = scanner_for(dir.path()).scan(&mut db, &[plugins_dir], no_save);
        assert_eq!(result.scanned, 2);
        assert_eq!(result.new + result.failed, 2);
        assert_eq!(db.len(), 2);
    }

    /// `scan` accepts any path-like iterable, not just a `&[PathBuf]`.
    ///
    /// The point of the `IntoIterator<Item = impl AsRef<Path>>` bound is that a
    /// caller with a `Vec<String>`, a borrowed config field, or a filtered
    /// iterator does not have to materialise a `Vec<PathBuf>` first. Each arm
    /// here is a distinct shape that fails to compile under a narrower
    /// signature: `&Vec<PathBuf>` is what `Plugins::rescan` passes, `[&str; 1]`
    /// covers the no-PathBuf-in-sight caller, and the `filter` covers a lazy
    /// iterator with no length to read.
    #[test]
    fn scan_accepts_any_path_like_iterable() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "synth.vst3");
        let scanner = scanner_for(dir.path());

        let owned: Vec<PathBuf> = vec![plugins_dir.clone()];
        let mut db = PluginDb::new();
        assert_eq!(scanner.scan(&mut db, &owned, no_save).scanned, 1);

        let as_str = plugins_dir.to_str().unwrap();
        let mut db = PluginDb::new();
        assert_eq!(scanner.scan(&mut db, [as_str], no_save).scanned, 1);

        let mut db = PluginDb::new();
        let lazy = owned.iter().filter(|p| p.exists());
        assert_eq!(scanner.scan(&mut db, lazy, no_save).scanned, 1);
    }

    /// An already-blacklisted plugin is skipped without being probed, and is
    /// counted as blacklisted rather than as a fresh failure: "was hidden before
    /// this scan" and "this scan hid it" are different facts, and only the second
    /// is worth telling the user about.
    #[test]
    fn scan_skips_blacklisted() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let bad = create_fake_plugin(&plugins_dir, "bad.vst3");
        create_fake_plugin(&plugins_dir, "good.vst3");

        let mut db = PluginDb::new();
        db.blacklist(&bad, "known crasher".into(), &FileFacts::read(&bad));

        let result = scanner_for(dir.path()).scan(&mut db, &[plugins_dir], no_save);

        assert_eq!(result.scanned, 2);
        assert_eq!(
            result.blacklisted - result.newly_blacklisted,
            1,
            "exactly one plugin was hidden before this scan started"
        );
        // `new + failed`, not `+ newly_blacklisted`: `tally` counts a newly
        // blacklisted plugin in both `failed` and `newly_blacklisted`. A scanner
        // that probed the blacklisted file anyway would land it in one of these.
        assert_eq!(result.new + result.failed, 1);
    }

    #[test]
    fn scan_skips_up_to_date() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let plugin = create_fake_plugin(&plugins_dir, "cached.vst3");

        let mut db = PluginDb::new();
        db.apply(Delta::put(
            &plugin,
            FileRow::probed(
                PluginFormat::Vst3,
                &FileFacts::read(&plugin),
                vec![PluginDescriptor::new(
                    "cached",
                    "cached",
                    PluginClass::Unknown,
                )],
            ),
        ));

        let result = scanner_for(dir.path()).scan(&mut db, &[plugins_dir], no_save);

        assert_eq!(result.scanned, 1);
        assert_eq!(result.new, 0);
    }

    #[test]
    fn recover_crash_blacklists_pedal_plugin() {
        let dir = TempDir::new().unwrap();
        let pedal_file = dir.path().join(".scanning");
        std::fs::write(&pedal_file, "/plugins/crashy.vst3").unwrap();

        let mut db = PluginDb::new();
        let scanner = Scanner::new(pedal_file.clone(), filename_probe);
        scanner.recover_crash(&mut db, no_save);

        assert!(db.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert!(!pedal_file.exists());
    }

    #[test]
    fn spawned_scan_with_progress() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.vst3");
        create_fake_plugin(&plugins_dir, "b.clap");

        let mut db = PluginDb::new();
        let scan = scanner_for(dir.path()).spawn(&mut db, &[plugins_dir], no_save);

        let mut saw_discovery = false;
        let mut saw_scanning = false;
        let mut saw_complete = false;
        while let Ok(p) = scan.progress_rx.recv_timeout(Duration::from_secs(30)) {
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

        let result = scan.wait(&mut db).expect("the worker finishes");
        assert_eq!(result.new + result.failed, 2);
    }

    /// A spawned scan's findings reach the database through `wait`: the worker
    /// holds only a snapshot, so a scan whose deltas were never applied would
    /// leave the caller with nothing. Mutation: have `wait` drop the delta
    /// instead of applying it.
    #[test]
    fn a_spawned_scan_applies_its_findings_to_the_db() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.vst3");

        let mut db = PluginDb::new();
        let scan = scanner_for(dir.path()).spawn(&mut db, &[plugins_dir], no_save);
        assert!(db.is_empty(), "nothing is applied before the owner polls");
        scan.wait(&mut db).expect("the worker finishes");

        assert_eq!(db.len(), 1);
    }

    /// A blacklisted probe result must land in the database and hide the plugin
    /// from `plugins()`.
    #[test]
    fn blacklisting_from_a_failed_probe_hides_the_plugin() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let bad = create_fake_plugin(&plugins_dir, "crashy.vst3");

        let mut db = PluginDb::new();
        db.blacklist(&bad, "crashed during probe".into(), &FileFacts::read(&bad));

        assert!(db.is_blacklisted(&bad));
        assert_eq!(db.plugins().count(), 0);
        // The blacklist stamps the current mtime, so the plugin is skipped
        // while unchanged.
        assert!(db.is_blacklisted_and_unchanged(&bad, &FileFacts::read(&bad)));
        assert!(matches!(
            facts::classify(db.row(&bad), &FileFacts::read(&bad)),
            ScanDecision::Blacklisted
        ));
    }

    /// An mtime change, from a reinstall or a vendor update, must re-admit a
    /// blacklisted plugin. `classify` therefore consults `needs_rescan` before
    /// the blacklist; checking the flag first makes a false positive permanent
    /// short of hand-editing the JSON.
    #[test]
    fn mtime_change_readmits_a_blacklisted_plugin() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let bad = create_fake_plugin(&plugins_dir, "was-crashy.vst3");

        let mut db = PluginDb::new();
        db.blacklist(&bad, "crashed during probe".into(), &FileFacts::read(&bad));
        assert!(matches!(
            facts::classify(db.row(&bad), &FileFacts::read(&bad)),
            ScanDecision::Blacklisted
        ));

        // Simulate a reinstall: the file's mtime moves.
        let mut row = db.row(&bad).unwrap().clone();
        row.mtime = row.mtime.map(|t| t - 1);
        db.apply(Delta::put(&bad, row));

        assert!(db.is_blacklisted(&bad), "flag is still set");
        assert!(
            !db.is_blacklisted_and_unchanged(&bad, &FileFacts::read(&bad)),
            "but the file changed, so it must be re-probed"
        );
        assert!(
            matches!(
                facts::classify(db.row(&bad), &FileFacts::read(&bad)),
                ScanDecision::Probe
            ),
            "a changed blacklisted plugin must be re-probed, not skipped forever"
        );
    }

    /// Blacklisting must have an inverse.
    #[test]
    fn unblacklist_and_clear_blacklist_are_the_inverse() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let a = create_fake_plugin(&plugins_dir, "a.vst3");
        let b = create_fake_plugin(&plugins_dir, "b.vst3");

        let mut db = PluginDb::new();
        db.blacklist(&a, "pedal misfire".into(), &FileFacts::read(&a));
        db.blacklist(&b, "pedal misfire".into(), &FileFacts::read(&b));
        assert_eq!(db.blacklisted().count(), 2);
        assert_eq!(db.plugins().count(), 0);
        // Blacklisting stamps the live mtime, so the entry is "unchanged".
        assert!(!db.needs_rescan(&a, &FileFacts::read(&a)));

        assert!(db.unblacklist(&a));
        assert!(!db.is_blacklisted(&a));
        // A lifted file is probed again: a blacklisted row holds no plugins to
        // serve.
        assert!(db.needs_rescan(&a, &FileFacts::read(&a)));
        assert!(matches!(
            facts::classify(db.row(&a), &FileFacts::read(&a)),
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
    /// recorded, the scanner must stop re-probing it. `blacklistable: false`
    /// once meant no write at all, so every scan paid a subprocess spawn to
    /// rediscover the same broken library.
    #[test]
    fn a_recorded_load_failure_is_not_reprobed() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let broken = create_fake_plugin(&plugins_dir, "Broken.vst3");

        let mut db = PluginDb::new();
        // What `probe_one` does for a LoadFailed.
        let failure = ProbeFailure {
            reason: "failed to load during probe: dlopen failed".into(),
            blacklistable: true,
        };
        db.blacklist(&broken, failure.reason, &FileFacts::read(&broken));

        assert!(
            matches!(
                facts::classify(db.row(&broken), &FileFacts::read(&broken)),
                ScanDecision::Blacklisted
            ),
            "a recorded load failure must be skipped, not re-probed at full \
             subprocess cost on every scan"
        );

        // Hidden from `plugins()`, but not silently: the reason is retrievable.
        assert_eq!(db.plugins().count(), 0);
        let entry = db.blacklisted().next().expect("must be listed as hidden");
        assert!(entry.reason.contains("dlopen"));

        // And it is not permanent: reinstalling the plugin re-admits it.
        let mut row = db.row(&broken).unwrap().clone();
        row.mtime = row.mtime.map(|t| t - 1);
        db.apply(Delta::put(&broken, row));
        assert!(
            matches!(
                facts::classify(db.row(&broken), &FileFacts::read(&broken)),
                ScanDecision::Probe
            ),
            "a rebuilt or reinstalled plugin must be re-probed"
        );
    }

    // New tests. Mutation records: the edit that breaks each.

    /// A second scan of an unchanged file does not probe it again.
    /// Mutation: make `needs_rescan` return `true`, or send the `UpToDate` arm
    /// to `probe_one`.
    #[test]
    fn rescanning_an_unchanged_file_does_not_reprobe_it() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.clap");
        let (scanner, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();

        scanner.scan(&mut db, [&plugins_dir], no_save);
        let second = scanner.scan(&mut db, [&plugins_dir], no_save);

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.scanned, 1);
        assert_eq!(second.new, 0);
    }

    /// A changed file is probed again and its row replaced, not added to.
    /// Mutation: skip the `Delta` for a file that already has a row.
    #[test]
    fn a_changed_file_is_reprobed_and_its_row_replaced() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let plugin = create_fake_plugin(&plugins_dir, "a.clap");
        set_mtime(&plugin, 1_000_000_000);
        let (scanner, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();
        scanner.scan(&mut db, [&plugins_dir], no_save);
        assert_eq!(db.row(&plugin).unwrap().mtime, Some(1_000_000_000));

        set_mtime(&plugin, 1_000_000_007);
        let second = scanner.scan(&mut db, [&plugins_dir], no_save);

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(second.new, 1);
        assert_eq!(db.len(), 1);
        assert_eq!(db.row(&plugin).unwrap().mtime, Some(1_000_000_007));
    }

    /// A file deleted between scans keeps its row until `prune_missing`: a scan
    /// that pruned would lose every row under an unmounted volume.
    /// Mutation: drop the row when discovery does not find its file.
    #[test]
    fn a_file_deleted_between_scans_keeps_its_row_until_pruned() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let kept = create_fake_plugin(&plugins_dir, "kept.clap");
        let gone = create_fake_plugin(&plugins_dir, "gone.clap");
        let (scanner, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();
        scanner.scan(&mut db, [&plugins_dir], no_save);
        std::fs::remove_file(&gone).unwrap();

        let second = scanner.scan(&mut db, [&plugins_dir], no_save);

        assert_eq!(second.scanned, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "only the two first probes");
        assert!(db.row(&gone).is_some(), "the stale row stays");

        let present = db
            .rows()
            .filter(|(path, _)| FileFacts::read(path).exists)
            .map(|(path, _)| path.to_path_buf())
            .collect();
        assert_eq!(db.prune_missing(&present), vec![gone.clone()]);
        assert!(db.row(&gone).is_none());
        assert!(db.row(&kept).is_some());
    }

    /// A probe that earns a blacklist leaves a row that later scans skip, and a
    /// change to the file lifts it. Mutation: record nothing for a blacklistable
    /// failure (the file is then probed on every scan).
    #[test]
    fn a_blacklisted_file_is_skipped_until_it_changes() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let plugin = create_fake_plugin(&plugins_dir, "bad.clap");
        set_mtime(&plugin, 1_000_000_000);
        let (scanner, calls) = counting_scanner(dir.path(), crashes);
        let mut db = PluginDb::new();

        let first = scanner.scan(&mut db, [&plugins_dir], no_save);
        assert_eq!(first.newly_blacklisted, 1);
        assert!(db.is_blacklisted(&plugin));
        assert_eq!(db.plugins().count(), 0);

        let second = scanner.scan(&mut db, [&plugins_dir], no_save);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the second scan skips it");
        assert_eq!(second.blacklisted - second.newly_blacklisted, 1);

        set_mtime(&plugin, 1_000_000_009);
        scanner.scan(&mut db, [&plugins_dir], no_save);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a changed file is probed");
    }

    /// A failure that says nothing about the plugin (a missing server) records
    /// nothing, so the file is retried. Mutation: ignore `blacklistable` and
    /// blacklist every failure.
    #[test]
    fn an_environmental_failure_records_nothing() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let plugin = create_fake_plugin(&plugins_dir, "a.clap");
        let (scanner, calls) = counting_scanner(dir.path(), |_| {
            Err(ProbeFailure {
                reason: "no plugin-server".into(),
                blacklistable: false,
            })
        });
        let mut db = PluginDb::new();

        scanner.scan(&mut db, [&plugins_dir], no_save);
        scanner.scan(&mut db, [&plugins_dir], no_save);

        assert!(db.row(&plugin).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// The host dying mid-scan blacklists the file it was probing on the next
    /// start. A probe that panics ends the worker with the pedal still armed,
    /// which is all a dead host leaves behind; a fresh scanner over a fresh
    /// database then recovers it through the table, saves, and skips the file.
    /// Mutation: disarm the pedal before the probe in `probe_one`.
    #[test]
    fn a_host_dying_mid_scan_blacklists_the_file_on_the_next_start() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let fine = create_fake_plugin(&plugins_dir, "fine.clap");
        let fatal = create_fake_plugin(&plugins_dir, "fatal.clap");
        let pedal_file = dir.path().join(".scanning");

        let dying = Scanner::new(pedal_file.clone(), |path, format| {
            if path.ends_with("fatal.clap") {
                panic!("the host went down inside the plugin");
            }
            Ok(probe_plugin_fallback(path, format))
        });
        let mut before = PluginDb::new();
        let scan = dying.spawn(&mut before, [&plugins_dir], no_save);
        assert!(scan.wait(&mut before).is_none(), "the worker died");
        assert_eq!(
            std::fs::read_to_string(&pedal_file).unwrap(),
            fatal.to_str().unwrap(),
            "the pedal names the file in flight"
        );

        // Next start: a database from disk (the dead run never saved), a new
        // scanner.
        let (next, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();
        let mut saved = None;
        let result = next.scan(&mut db, [&plugins_dir], |db| {
            saved = Some(db.clone());
            Ok(())
        });

        let row = saved
            .expect("recovery saved before the scan")
            .row(&fatal)
            .cloned()
            .expect("the saved table has the file");
        assert!(matches!(row.state, FileState::Blacklisted { .. }));
        assert!(db.is_blacklisted(&fatal));
        assert!(!pedal_file.exists());
        assert_eq!(result.blacklisted - result.newly_blacklisted, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "only `fine` is probed");
        assert!(db.row(&fine).is_some());
    }

    /// The database is usable while a scan runs, and every row a reader sees is
    /// whole. The probe of the second file blocks on a gate the test holds, so
    /// the scan is parked mid-flight with no sleep: the first file's delta is
    /// already there to apply, the second file still shows its old row, and the
    /// owner reads and edits throughout. Whatever the order of the two files,
    /// each shows its old row or its new one, never a mixture.
    /// Mutation: have the worker hold its deltas until the end (the first file's
    /// new row is not there at the park).
    #[test]
    fn the_db_is_usable_and_rows_are_whole_while_a_scan_is_parked_mid_probe() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let a = create_fake_plugin(&plugins_dir, "a.clap");
        let b = create_fake_plugin(&plugins_dir, "b.clap");
        let (old, new) = (1_000_000_000, 1_000_000_099);
        for p in [&a, &b] {
            set_mtime(p, new);
        }
        let mut db = PluginDb::new();
        for p in [&a, &b] {
            let facts = FileFacts {
                mtime: Some(old),
                ..FileFacts::read(p)
            };
            let stale = vec![PluginDescriptor::new("old", "old", PluginClass::Unknown)];
            db.apply(Delta::put(
                p,
                FileRow::probed(PluginFormat::Clap, &facts, stale),
            ));
        }

        let (parked_tx, parked_rx) = crossbeam_channel::bounded::<PathBuf>(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        let probes = AtomicUsize::new(0);
        let scanner = Scanner::new(dir.path().join(".scanning"), move |path, _| {
            if probes.fetch_add(1, Ordering::SeqCst) == 1 {
                parked_tx.send(path.to_path_buf()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            }
            Ok(PluginDescriptor::new("new", "new", PluginClass::Unknown))
        });
        let scan = scanner.spawn(&mut db, [&plugins_dir], no_save);

        let parked = parked_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the second probe parks");
        let (first, second) = if parked == a { (&b, &a) } else { (&a, &b) };

        // Mid-scan: the owner reads and writes, and applies what has arrived.
        assert_eq!(db.plugins().count(), 2, "readable while the scan is parked");
        db.blacklist(
            Path::new("/elsewhere.vst3"),
            "x".into(),
            &FileFacts::read(first),
        );
        db.unblacklist(Path::new("/elsewhere.vst3"));
        let whole = |db: &PluginDb, path: &Path| {
            let row = db.row(path).unwrap();
            let name = row.plugins()[0].name.as_str();
            matches!((row.mtime, name), (Some(t), "old") if t == old)
                || matches!((row.mtime, name), (Some(t), "new") if t == new)
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut seen_first_new = false;
        while !seen_first_new {
            assert!(
                std::time::Instant::now() < deadline,
                "the first file's delta never arrived while the scan was parked"
            );
            assert!(matches!(scan.poll(&mut db), ScanPoll::Running));
            assert!(whole(&db, first) && whole(&db, second));
            seen_first_new = db.row(first).unwrap().mtime == Some(new);
            std::thread::yield_now();
        }
        assert_eq!(db.row(second).unwrap().mtime, Some(old), "still parked");

        release_tx.send(()).unwrap();
        let result = loop {
            match scan.poll(&mut db) {
                ScanPoll::Done(result) => break result,
                ScanPoll::Running => assert!(whole(&db, first) && whole(&db, second)),
                ScanPoll::Died => panic!("the worker died"),
            }
            std::thread::yield_now();
        };
        assert_eq!(result.new, 2);
        assert_eq!(db.row(second).unwrap().mtime, Some(new));
    }

    /// An edit made while a scan runs (a blacklist the user set) is still there
    /// after the scan's deltas land: the scan replaces the rows it probed and
    /// touches no other. Mutation: have `Scan::wait` clear `db` when it sees
    /// `Done`.
    #[test]
    fn an_edit_made_during_a_scan_survives_it() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.clap");
        let mut db = PluginDb::new();
        let scan = scanner_for(dir.path()).spawn(&mut db, [&plugins_dir], no_save);

        let elsewhere = Path::new("/elsewhere/user-hid-this.vst3");
        db.blacklist(
            elsewhere,
            "hidden by the user".into(),
            &FileFacts::read(elsewhere),
        );
        scan.wait(&mut db).expect("the worker finishes");

        assert!(db.is_blacklisted(elsewhere));
        assert_eq!(db.len(), 2);
    }

    /// Dropping the `Scan` stops the worker after the file in flight, and it
    /// disarms the pedal. Mutation: ignore the sink's `false` in `Job::run`.
    #[test]
    fn dropping_the_scan_stops_the_worker() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        for name in ["a.clap", "b.clap", "c.clap"] {
            create_fake_plugin(&plugins_dir, name);
        }
        let (parked_tx, parked_rx) = crossbeam_channel::bounded::<()>(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let scanner = Scanner::new(dir.path().join(".scanning"), move |path, format| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                parked_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            }
            Ok(probe_plugin_fallback(path, format))
        });
        let mut db = PluginDb::new();
        let scan = scanner.spawn(&mut db, [&plugins_dir], no_save);
        parked_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let progress = scan.progress_rx.clone();

        drop(scan);
        release_tx.send(()).unwrap();
        // The progress sender lives in the worker; its end closes the channel.
        while progress.recv_timeout(Duration::from_secs(30)).is_ok() {}

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
