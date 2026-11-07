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

    /// Record the plugin that is about to be probed. Creates the pedal's
    /// directory, so the first run is protected too.
    pub(super) fn arm(&self, plugin: impl AsRef<Path>) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::File::create(&self.path)?;
        f.write_all(&path_bytes(plugin.as_ref()))?;
        f.sync_all()?;
        Ok(())
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the pedal on clean completion.
    pub(super) fn disarm(&self) {
        let _ = std::fs::remove_file(&self.path);
    }

    /// If a pedal file survives from a crashed previous scan, blacklists the
    /// recorded file in `db`, hands `db` to `save`, and removes the pedal.
    /// Returns the file that was blacklisted.
    ///
    /// The pedal stays until `save` succeeds, so a host that dies again before
    /// the blacklist is on disk still finds it next start.
    pub(super) fn recover(
        &self,
        db: &mut PluginDb,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Option<PathBuf> {
        if !self.path.exists() {
            return None;
        }
        let crashed = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| parse_pedal(&bytes));
        if let Some(p) = &crashed {
            warn!(
                "dead-man's pedal found: blacklisting {:?} (crashed during previous scan)",
                p
            );
            db.blacklist(
                p,
                "crashed during previous scan".into(),
                &FileFacts::read(p),
            );
            if let Err(e) = save(db) {
                warn!("could not save the blacklist for {p:?}; keeping the pedal: {e}");
                return crashed;
            }
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

/// Reads the plugin a pedal file names.
///
/// Returns `None` for contents that name no plugin (empty or only whitespace).
pub(super) fn parse_pedal(bytes: &[u8]) -> Option<PathBuf> {
    let bytes = bytes.trim_ascii();
    (!bytes.is_empty()).then(|| path_from_bytes(bytes))
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

        pedal.disarm();
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

        assert_eq!(crashed, Some(PathBuf::from("/plugins/crashy.vst3")));
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

        assert!(crashed.is_some());
        assert!(db.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert!(pedal.path.exists());
    }

    #[test]
    fn recover_returns_none_when_no_pedal() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        let mut db = PluginDb::new();

        assert_eq!(pedal.recover(&mut db, |_| Ok(())), None);
    }

    // Mutation records: the edit that breaks each in-memory test.

    /// Mutation: drop the `trim` (the trailing newline becomes part of the path).
    #[test]
    fn parse_pedal_names_the_plugin_without_surrounding_whitespace() {
        assert_eq!(
            parse_pedal("/plugins/crashy.vst3".as_bytes()),
            Some(PathBuf::from("/plugins/crashy.vst3"))
        );
        assert_eq!(
            parse_pedal("  /plugins/crashy.vst3\n".as_bytes()),
            Some(PathBuf::from("/plugins/crashy.vst3"))
        );
    }

    /// Mutation: return `Some` for the empty path (a blank pedal would then
    /// blacklist the path `""`).
    #[test]
    fn parse_pedal_names_nothing_for_blank_text() {
        assert_eq!(parse_pedal("".as_bytes()), None);
        assert_eq!(parse_pedal(" \n\t ".as_bytes()), None);
    }

    /// A path with interior spaces is one path. Mutation: split on whitespace.
    #[test]
    fn parse_pedal_keeps_interior_spaces() {
        assert_eq!(
            parse_pedal("/Library/My Plugins/A B.vst3".as_bytes()),
            Some(PathBuf::from("/Library/My Plugins/A B.vst3"))
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
        assert_eq!(pedal.recover(&mut db, |_| Ok(())), None);
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

        assert_eq!(crashed, Some(plugin.clone()));
        assert!(db.is_blacklisted(&plugin));
    }
}
