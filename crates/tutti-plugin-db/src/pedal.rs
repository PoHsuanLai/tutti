//! Dead-man's pedal: a sentinel file whose presence signals the scanner
//! crashed while probing a specific plugin. Next startup blacklists that
//! plugin so the scan can make progress.

use super::facts::FileFacts;
use super::table::PluginDb;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug, Clone)]
pub(super) struct Pedal {
    path: PathBuf,
}

impl Pedal {
    /// Pedal file lives at `path` for the duration of a single probe.
    pub(super) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// The plugins the file names, in order: a crashed probe, and any entry a
    /// recovery could not save and kept.
    fn entries(&self) -> Vec<PathBuf> {
        std::fs::read(&self.path)
            .map(|bytes| parse_pedal(&bytes))
            .unwrap_or_default()
    }

    /// Writes `entries` (NUL-separated, which no path holds), or removes the
    /// file when there are none.
    fn write(&self, entries: &[PathBuf]) -> std::io::Result<()> {
        if entries.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::File::create(&self.path)?;
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                f.write_all(&[0])?;
            }
            f.write_all(&path_bytes(entry))?;
        }
        f.sync_all()
    }

    /// Record the plugin that is about to be probed, keeping any entry a
    /// recovery could not save: overwriting it would forget a crash. Creates
    /// the pedal's directory, so the first run is protected too.
    pub(super) fn arm(&self, plugin: impl AsRef<Path>) -> std::io::Result<()> {
        let plugin = plugin.as_ref();
        let mut entries = self.entries();
        entries.retain(|p| p != plugin);
        entries.push(plugin.to_path_buf());
        self.write(&entries)
    }

    /// Remove the probe's entry on clean completion; a retained entry stays.
    pub(super) fn disarm(&self, plugin: &Path) {
        let mut entries = self.entries();
        entries.retain(|p| p != plugin);
        let _ = self.write(&entries);
    }

    /// If a pedal file survives from a crashed previous scan, blacklists the
    /// recorded files in `db`, hands `db` to `save`, and removes the pedal.
    /// Returns the files that were blacklisted.
    ///
    /// The pedal stays until `save` succeeds, so a host that dies again before
    /// the blacklist is on disk still finds it next start, and a scan that
    /// follows a failed save adds its own entry beside the kept ones.
    pub(super) fn recover(
        &self,
        db: &mut PluginDb,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Vec<PathBuf> {
        let crashed = self.entries();
        for p in &crashed {
            warn!(
                "dead-man's pedal found: blacklisting {:?} (crashed during previous scan)",
                p
            );
            db.blacklist(
                p,
                "crashed during previous scan".into(),
                &FileFacts::read(p),
            );
        }
        if !crashed.is_empty()
            && let Err(e) = save(db)
        {
            warn!("could not save the blacklist for {crashed:?}; keeping the pedal: {e}");
            return crashed;
        }
        let _ = std::fs::remove_file(&self.path);
        crashed
    }
}

/// A path as the pedal stores it: the raw bytes on unix, where a name need not
/// be UTF-8 and a lossy copy would blacklist a different file; the UTF-8 text
/// elsewhere (a Windows path that is not valid Unicode is stored lossily).
#[cfg(unix)]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    std::borrow::Cow::Owned(path.to_string_lossy().into_owned().into_bytes())
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Reads the plugins a pedal file names: NUL-separated paths, each trimmed of
/// surrounding ASCII whitespace; blank entries name nothing.
pub(super) fn parse_pedal(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|b| *b == 0)
        .map(<[u8]>::trim_ascii)
        .filter(|entry| !entry.is_empty())
        .map(path_from_bytes)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn arm_writes_and_disarm_removes() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        let plugin = Path::new("/plugins/test.vst3");

        pedal.arm(plugin).unwrap();
        assert!(pedal.path.exists());
        let contents = std::fs::read_to_string(&pedal.path).unwrap();
        assert_eq!(contents, "/plugins/test.vst3");

        pedal.disarm(plugin);
        assert!(!pedal.path.exists());
    }

    /// Mutation: skip the `save` call.
    #[test]
    fn recover_blacklists_crashed_plugin_and_saves_before_clearing_the_pedal() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        std::fs::write(&pedal.path, "/plugins/crashy.vst3").unwrap();

        let mut db = PluginDb::new();
        let mut saved = None;
        let crashed = pedal.recover(&mut db, |db| {
            assert!(pedal.path.exists(), "the pedal outlives the save");
            saved = Some(db.clone());
            Ok(())
        });

        assert_eq!(crashed, vec![PathBuf::from("/plugins/crashy.vst3")]);
        assert!(db.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert_eq!(saved, Some(db));
        assert!(!pedal.path.exists());
    }

    /// Mutation: remove the pedal whatever `save` returned.
    #[test]
    fn recover_keeps_the_pedal_when_the_save_fails() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        std::fs::write(&pedal.path, "/plugins/crashy.vst3").unwrap();

        let mut db = PluginDb::new();
        let crashed = pedal.recover(&mut db, |_| Err(std::io::Error::other("disk full")));

        assert_eq!(crashed.len(), 1);
        assert!(db.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert!(pedal.path.exists());
    }

    #[test]
    fn recover_returns_none_when_no_pedal() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        let mut db = PluginDb::new();

        assert!(pedal.recover(&mut db, |_| Ok(())).is_empty());
    }

    // Mutation records: the edit that breaks each in-memory test.

    /// Mutation: drop the `trim` (the trailing newline becomes part of the path).
    #[test]
    fn parse_pedal_names_the_plugin_without_surrounding_whitespace() {
        assert_eq!(
            parse_pedal("/plugins/crashy.vst3".as_bytes()),
            vec![PathBuf::from("/plugins/crashy.vst3")]
        );
        assert_eq!(
            parse_pedal("  /plugins/crashy.vst3\n".as_bytes()),
            vec![PathBuf::from("/plugins/crashy.vst3")]
        );
    }

    /// Mutation: return `Some` for the empty path (a blank pedal would then
    /// blacklist the path `""`).
    #[test]
    fn parse_pedal_names_nothing_for_blank_text() {
        assert!(parse_pedal("".as_bytes()).is_empty());
        assert!(parse_pedal(" \n\t ".as_bytes()).is_empty());
    }

    /// A path with interior spaces is one path. Mutation: split on whitespace.
    #[test]
    fn parse_pedal_keeps_interior_spaces() {
        assert_eq!(
            parse_pedal("/Library/My Plugins/A B.vst3".as_bytes()),
            vec![PathBuf::from("/Library/My Plugins/A B.vst3")]
        );
    }

    /// An unreadable or blank pedal is removed without blacklisting anything.
    /// Mutation: leave the file in place on the `None` path (every later start
    /// would find it again).
    #[test]
    fn recover_removes_a_blank_pedal_and_blacklists_nothing() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        std::fs::write(&pedal.path, "  \n").unwrap();

        let mut db = PluginDb::new();
        assert!(pedal.recover(&mut db, |_| Ok(())).is_empty());
        assert!(db.is_empty());
        assert!(!pedal.path.exists());
    }

    /// On the first run the pedal's directory does not exist yet; arming must
    /// still protect the probe. Mutation: drop the `create_dir_all` in `arm`.
    #[test]
    fn arm_creates_the_pedals_directory() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join("fresh").join("state").join(".scanning"));

        pedal.arm("/plugins/test.vst3").unwrap();

        assert!(pedal.path.exists());
    }

    /// A name that is not UTF-8 survives the pedal, so the file that crashed is
    /// the file that is blacklisted. Mutation: store `to_string_lossy()` in
    /// `path_bytes`.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_round_trips_through_the_pedal() {
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        let plugin = PathBuf::from(std::ffi::OsStr::from_bytes(b"/plugins/caf\xe9.vst3"));
        assert!(plugin.to_str().is_none());

        pedal.arm(&plugin).unwrap();
        let mut db = PluginDb::new();
        let crashed = pedal.recover(&mut db, |_| Ok(()));

        assert_eq!(crashed, vec![plugin.clone()]);
        assert!(db.is_blacklisted(&plugin));
    }

    /// Arming beside an entry keeps it, and disarming removes only its own.
    /// Mutation: write only the new entry in `arm`; or clear the file in
    /// `disarm`.
    #[test]
    fn entries_sit_side_by_side_and_disarm_removes_only_its_own() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        let (a, b) = (Path::new("/p/a.vst3"), Path::new("/p/b.vst3"));

        pedal.arm(a).unwrap();
        pedal.arm(b).unwrap();
        assert_eq!(pedal.entries(), [a, b]);
        pedal.disarm(b);
        assert_eq!(pedal.entries(), [a]);
        pedal.disarm(a);
        assert!(!pedal.path.exists());
    }
}
