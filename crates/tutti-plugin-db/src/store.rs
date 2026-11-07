//! The database as a JSON file: [`save`] and [`load`].
//!
//! The file carries a version. A file of another version, or an old file with
//! none, is not read: it is moved aside and the database starts empty, and a
//! rescan rebuilds it. There is no migration. Quarantined files are never
//! overwritten.
//!
//! A path that is not valid UTF-8 is stored as its bytes (unix), so a plugin
//! with such a name keeps its row, and its blacklist, across restarts. Where
//! that is not possible, [`save`] leaves the row out, logs it, saves the rest,
//! and returns an error if the row was a blacklisted one.

use super::fs::replace_file;
use super::table::{Delta, FileRow, PluginDb};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// The version of the file layout [`save`] writes and [`load`] reads.
const VERSION: u64 = 1;

#[derive(Serialize)]
struct Out<'a> {
    version: u64,
    files: Vec<(StoredPath<'a>, &'a FileRow)>,
}

#[derive(Deserialize)]
struct In {
    files: Vec<(StoredPathOwned, FileRow)>,
}

/// A path as the file holds it: text, or the raw bytes of a name that is not
/// UTF-8.
#[derive(Serialize)]
#[serde(untagged)]
enum StoredPath<'a> {
    Text(&'a str),
    #[cfg(unix)]
    Bytes {
        bytes: &'a [u8],
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredPathOwned {
    Text(String),
    Bytes { bytes: Vec<u8> },
}

impl<'a> StoredPath<'a> {
    fn of(path: &'a Path) -> Option<Self> {
        if let Some(text) = path.to_str() {
            return Some(Self::Text(text));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Some(Self::Bytes {
                bytes: path.as_os_str().as_bytes(),
            })
        }
        #[cfg(not(unix))]
        None
    }
}

impl StoredPathOwned {
    fn into_path(self) -> PathBuf {
        match self {
            Self::Text(text) => PathBuf::from(text),
            #[cfg(unix)]
            Self::Bytes { bytes } => {
                use std::os::unix::ffi::OsStrExt;
                PathBuf::from(std::ffi::OsStr::from_bytes(&bytes))
            }
            #[cfg(not(unix))]
            Self::Bytes { bytes } => PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()),
        }
    }
}

/// Why a file is not a database this version can read.
#[derive(Debug)]
enum Unreadable {
    /// Not JSON, or not the shape of the current version.
    Corrupt(serde_json::Error),
    /// Another version wrote it. `0` is an old file that has no version.
    Version(u64),
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Corrupt(e) => write!(f, "corrupt ({e})"),
            Self::Version(found) => write!(f, "is version {found}, this reads version {VERSION}"),
        }
    }
}

impl Unreadable {
    /// Where the file is moved so that its records can be recovered by hand.
    fn quarantine(&self, path: &Path) -> PathBuf {
        match self {
            Self::Corrupt(_) => path.with_extension("corrupt"),
            Self::Version(found) => path.with_extension(format!("v{found}")),
        }
    }
}

fn parse(text: &str) -> Result<PluginDb, Unreadable> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(Unreadable::Corrupt)?;
    match value.get("version").map(serde_json::Value::as_u64) {
        Some(Some(VERSION)) => {}
        Some(Some(found)) => return Err(Unreadable::Version(found)),
        Some(None) => return Err(Unreadable::Corrupt(custom("`version` is not an integer"))),
        // An old file is a bare list of records and has no version.
        None if value.is_array() => return Err(Unreadable::Version(0)),
        None => return Err(Unreadable::Corrupt(custom("no `version`"))),
    }
    let file: In = serde_json::from_value(value).map_err(Unreadable::Corrupt)?;
    let mut db = PluginDb::new();
    for (path, row) in file.files {
        db.apply(Delta::put(path.into_path(), row));
    }
    Ok(db)
}

fn custom(message: &str) -> serde_json::Error {
    <serde_json::Error as serde::de::Error>::custom(message)
}

/// The first of `path`, `path.1`, `path.2`, ... that does not exist, so a
/// quarantined file never replaces an earlier one.
fn unused_name(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    (1u32..)
        .map(|n| {
            let mut name = path.clone().into_os_string();
            name.push(format!(".{n}"));
            PathBuf::from(name)
        })
        .find(|candidate| !candidate.exists())
        .unwrap_or(path)
}

/// Loads the database from `path`, or starts empty if the file is missing,
/// corrupt, or of another version.
///
/// An unreadable file is not overwritten: it is renamed (`.corrupt`, or `.v<N>`
/// for version `N`, `.v0` for an old file with none; `.1`, `.2`, ... is added
/// when that name is taken) so its records can be recovered by hand.
///
/// A temp file a crashed [`save`] left behind (`<name>.tmp-<pid>-<n>`) is not
/// removed: the writer of a live one may be another process.
pub fn load(path: impl AsRef<Path>) -> PluginDb {
    let path = path.as_ref();
    let Ok(text) = std::fs::read_to_string(path) else {
        debug!("no plugin database at {:?}, starting fresh", path);
        return PluginDb::new();
    };
    match parse(&text) {
        Ok(db) => {
            debug!("loaded {} plugin files from {:?}", db.len(), path);
            db
        }
        Err(unreadable) => {
            let quarantine = unused_name(unreadable.quarantine(path));
            match std::fs::rename(path, &quarantine) {
                Ok(()) => warn!(
                    "plugin database {unreadable}; preserved at {:?}, starting fresh",
                    quarantine
                ),
                Err(e) => warn!(
                    "plugin database {unreadable} and could not be preserved ({e}); \
                     starting fresh"
                ),
            }
            PluginDb::new()
        }
    }
}

/// Writes the database to `path` atomically: a sibling temp file, fsynced, then
/// renamed over the real path. A reader, or a crash, sees the old file or the
/// new one, never a partial write.
///
/// # Errors
///
/// Returns the I/O error if the parent directory, the temp file or the rename
/// fails; the previous file is then intact.
pub fn save(db: &PluginDb, path: impl AsRef<Path>) -> std::io::Result<()> {
    let path = path.as_ref();
    let mut files = Vec::new();
    let mut lost_blacklist = false;
    for (path, row) in db.rows() {
        match StoredPath::of(path) {
            Some(stored) => files.push((stored, row)),
            None => {
                warn!("plugin database: not saving {path:?}, the path cannot be stored");
                lost_blacklist |= row.blacklist_reason().is_some();
            }
        }
    }
    let out = Out {
        version: VERSION,
        files,
    };
    let json = serde_json::to_string_pretty(&out).map_err(std::io::Error::other)?;
    replace_file(path, |f| f.write_all(json.as_bytes()))?;
    debug!("saved {} plugin files to {:?}", db.len(), path);
    if lost_blacklist {
        // The rest is saved; the error keeps a recovery's pedal armed, since the
        // pedal is the only record of that blacklist.
        return Err(std::io::Error::other(
            "a blacklisted file's path cannot be stored in the database file",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::FileFacts;
    use crate::format::{PluginClass, PluginDescriptor, PluginFormat};
    use tempfile::TempDir;

    fn row(name: &str) -> FileRow {
        let facts = FileFacts {
            exists: true,
            mtime: Some(1_700_000_000),
            format: Some(PluginFormat::Vst3),
        };
        FileRow::probed(
            PluginFormat::Vst3,
            &facts,
            vec![PluginDescriptor::new(name, name, PluginClass::Unknown)],
        )
    }

    fn db_of<S: AsRef<str>>(names: &[S]) -> PluginDb {
        let mut db = PluginDb::new();
        for name in names {
            let name = name.as_ref();
            db.apply(Delta::put(format!("/plugins/{name}.vst3"), row(name)));
        }
        db
    }

    fn no_file() -> FileFacts {
        FileFacts {
            exists: false,
            mtime: None,
            format: None,
        }
    }

    /// The text of a current-version file holding one row.
    fn current_text() -> String {
        let db = db_of(&["reverb"]);
        serde_json::to_string(&Out {
            version: VERSION,
            files: db
                .rows()
                .map(|(p, r)| (StoredPath::of(p).unwrap(), r))
                .collect(),
        })
        .unwrap()
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plugins.json");
        let mut db = db_of(&["reverb", "delay"]);
        db.blacklist(
            Path::new("/plugins/crashy.vst3"),
            "segfault".into(),
            &no_file(),
        );
        save(&db, &path).unwrap();

        assert_eq!(load(&path), db);
    }

    /// The blacklist, including a file that has no time, survives a restart.
    /// Mutation: skip blacklisted rows in `save`.
    #[test]
    fn a_blacklist_persists_through_save_and_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db.json");
        let crashy = PathBuf::from("/plugins/crashy.vst3");
        let mut db = PluginDb::new();
        db.blacklist(&crashy, "segfault".into(), &no_file());
        save(&db, &path).unwrap();

        let loaded = load(&path);
        assert!(loaded.is_blacklisted(&crashy));
        assert_eq!(loaded.row(&crashy).unwrap().mtime, None);
    }

    #[test]
    fn a_missing_file_loads_empty() {
        let dir = TempDir::new().unwrap();
        assert!(load(dir.path().join("none.json")).is_empty());
    }

    #[test]
    fn load_corrupt_json_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db.json");
        std::fs::write(&path, "not valid json{{{").unwrap();

        assert!(load(&path).is_empty());
    }

    /// A corrupt database must be preserved, not destroyed by the next save.
    /// Losing it loses every blacklist entry.
    #[test]
    fn load_corrupt_json_quarantines_the_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db.json");
        std::fs::write(&path, "not valid json{{{").unwrap();

        let _db = load(&path);

        let quarantine = path.with_extension("corrupt");
        assert!(quarantine.exists(), "corrupt DB should be moved aside");
        assert_eq!(
            std::fs::read_to_string(&quarantine).unwrap(),
            "not valid json{{{"
        );
        assert!(!path.exists(), "corrupt DB should not be left in place");
    }

    /// The current version with a body of the wrong shape is corrupt, not
    /// another version. Mutation: map every parse failure to `Version`.
    #[test]
    fn the_current_version_with_a_bad_body_is_quarantined_as_corrupt() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db.json");
        std::fs::write(&path, format!(r#"{{"version":{VERSION},"files":7}}"#)).unwrap();

        assert!(load(&path).is_empty());
        assert!(path.with_extension("corrupt").exists());
    }

    /// A file of another version is refused and moved aside, whichever side of
    /// this version it is on. Each body is otherwise a valid current-version
    /// body, so a loader that ignored the version would read it.
    /// Mutation: drop the `Some(Some(VERSION))` check (accept any integer
    /// version).
    #[test]
    fn a_file_of_another_version_is_refused_rebuilt_and_kept() {
        let current = current_text();
        let marker = format!("\"version\":{VERSION}");
        assert!(current.contains(&marker));
        let with_version = |v: u64| current.replace(&marker, &format!("\"version\":{v}"));
        let cases = [
            (with_version(VERSION - 1), "v0"),
            (with_version(VERSION + 1), "v2"),
            (with_version(41), "v41"),
        ];
        for (text, extension) in cases {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("db.json");
            std::fs::write(&path, &text).unwrap();

            let db = load(&path);

            assert!(db.is_empty(), "{extension}: was misread: {text}");
            assert!(!path.exists(), "{extension}: left in place");
            assert_eq!(
                std::fs::read_to_string(path.with_extension(extension)).unwrap(),
                text,
                "{extension}: not preserved"
            );
        }
    }

    /// A body with no version is not read as the current one. Mutation: treat a
    /// missing version as current.
    #[test]
    fn a_body_with_no_version_is_refused() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db.json");
        let text = current_text().replace(&format!("\"version\":{VERSION},"), "");
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("version").is_none());
        std::fs::write(&path, &text).unwrap();

        assert!(load(&path).is_empty());
        assert!(path.with_extension("corrupt").exists());
    }

    /// An old file in the layout before versions (a bare list of records) is
    /// refused and rebuilt. Mutation: accept a top-level array.
    #[test]
    fn the_unversioned_record_list_layout_is_refused() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db.json");
        let old = r#"[{"path":"/p/a.vst3","format":"Vst3","descriptor":{"id":"i","name":"n"},"modification_time":5}]"#;
        std::fs::write(&path, old).unwrap();

        assert!(load(&path).is_empty());
        assert_eq!(
            std::fs::read_to_string(path.with_extension("v0")).unwrap(),
            old
        );
    }

    /// A file that lists one path twice loads the later row, as `apply` would.
    /// Mutation: apply the rows in reverse order in `parse`.
    #[test]
    fn a_later_row_for_one_path_wins() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db.json");
        let (first, second) = (row("first"), row("second"));
        let text = serde_json::to_string(&Out {
            version: VERSION,
            files: vec![
                (StoredPath::Text("/plugins/a.vst3"), &first),
                (StoredPath::Text("/plugins/a.vst3"), &second),
            ],
        })
        .unwrap();
        std::fs::write(&path, text).unwrap();

        let db = load(&path);

        assert_eq!(db.len(), 1);
        assert_eq!(db.row(Path::new("/plugins/a.vst3")), Some(&second));
    }

    /// A save must write the version `load` accepts. Mutation: write another
    /// number in `save`.
    #[test]
    fn a_saved_file_declares_the_version_load_reads() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db.json");
        save(&db_of(&["a"]), &path).unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["version"], VERSION);
    }

    /// `save` replaces the file wholesale and leaves no temp debris.
    #[test]
    fn save_replaces_the_file_and_leaves_no_temp_files() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plugins.json");
        let many: Vec<String> = (0..50).map(|i| format!("p{i}")).collect();
        save(&db_of(&many), &path).unwrap();
        assert_eq!(load(&path).len(), 50);

        // Fewer rows: the rename must fully replace, not leave the tail of the
        // longer previous content.
        save(&db_of(&["only"]), &path).unwrap();
        assert_eq!(load(&path).len(), 1);

        let stray: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp-"))
            .collect();
        assert!(stray.is_empty(), "save left temp files behind: {stray:?}");
    }

    /// A write that fails midway leaves the old file whole and no temp file.
    /// Mutation: have `replace_file` write to `path` directly.
    #[test]
    fn a_failed_write_leaves_the_old_file_intact() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plugins.json");
        save(&db_of(&["first"]), &path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = replace_file(&path, |f| {
            f.write_all(b"half of a new fi")?;
            Err(std::io::Error::other("disk full"))
        })
        .unwrap_err();

        assert_eq!(err.to_string(), "disk full");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(load(&path), db_of(&["first"]));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, ["plugins.json"], "the temp file is removed");
    }

    /// A failure at the rename (the target is a directory) is an error, and
    /// leaves no temp file. Mutation: skip the cleanup in `replace_file`.
    #[test]
    fn a_failed_rename_is_an_error_and_leaves_no_temp_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plugins.json");
        std::fs::create_dir(&path).unwrap();

        assert!(save(&db_of(&["a"]), &path).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// Rows come out in path order, so a saved file does not churn between
    /// saves. Mutation: back the table with a `HashMap`.
    #[test]
    fn saving_twice_writes_the_same_bytes() {
        let dir = TempDir::new().unwrap();
        let (a, b) = (dir.path().join("a.json"), dir.path().join("b.json"));
        let db = db_of(&["x", "y", "z", "w", "u", "v"]);
        save(&db, &a).unwrap();
        save(&db_of(&["v", "u", "w", "z", "y", "x"]), &b).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    }

    /// A second unreadable file does not replace the first, for each kind.
    /// Mutation: return the name without checking `exists` in `unused_name`.
    #[test]
    fn quarantined_files_are_never_overwritten() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db.json");
        let old = r#"[{"path":"/p/a.vst3"}]"#;
        for (text, extension) in [
            ("junk one", "corrupt"),
            ("junk two", "corrupt.1"),
            ("junk three", "corrupt.2"),
            (old, "v0"),
            ("[1]", "v0.1"),
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(load(&path).is_empty());
            assert_eq!(
                std::fs::read_to_string(path.with_extension(extension)).unwrap(),
                text,
                "{extension}"
            );
        }
    }

    /// The temp file is a sibling of the target, so the rename stays on one
    /// filesystem. Mutation: build the temp name under `std::env::temp_dir()`.
    #[test]
    fn the_temp_file_is_a_sibling_of_the_target() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plugins.json");

        replace_file(&path, |f| {
            let temps: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(temps.len(), 1, "{temps:?}");
            assert!(temps[0].starts_with("plugins.tmp-"), "{temps:?}");
            f.write_all(b"x")
        })
        .unwrap();
    }

    /// A name that is not UTF-8 keeps its row, blacklist included, through a
    /// save and a load: a crashing plugin with such a name must not be probed
    /// again after a restart. Mutation: skip rows whose path has no `to_str`
    /// in `StoredPath::of` (return `None`).
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_round_trips_through_the_file() {
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db.json");
        let mut db = db_of(&["kept"]);
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/plugins/caf\xe9.vst3"));
        db.blacklist(&odd, "boom".into(), &no_file());

        save(&db, &path).unwrap();

        assert_eq!(load(&path), db);
    }
}
