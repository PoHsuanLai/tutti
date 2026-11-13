//! The database as a serde record: a version, then the rows.
//!
//! Deserializing reads a private [`Raw`] and converts it with `TryFrom`, so a
//! database that is not valid never exists: a record of another version, or an
//! old one with none, is [`InvalidDb::Version`], and a path listed twice is
//! [`InvalidDb::DuplicatePath`]. There is no migration; a rescan rebuilds the
//! table.
//!
//! A path that is not valid UTF-8 is stored as its bytes (unix), so a plugin
//! with such a name keeps its row, and its blacklist, across restarts. Where
//! that is not possible, serializing fails rather than dropping the row.

use super::os_path;
use super::table::{Delta, FileRow, PluginDb};
use serde::ser::Error as _;
use serde::{Deserialize, Serialize, Serializer};
use std::path::{Path, PathBuf};

/// The version of the record layout [`PluginDb`] serializes and deserializes.
/// 3: a file's row lists every plugin it holds; 2: a bus's speakers name a
/// channel with no speaker `Unassigned`.
pub const DB_VERSION: u64 = 3;

/// Why a record is not a database this version can read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidDb {
    /// Another version wrote it; `found` is `0` for a record with no version.
    #[error("the plugin database is version {found}, this reads version {DB_VERSION}")]
    Version {
        /// The version the record declares.
        found: u64,
    },
    /// Two rows name one path, so the record does not say which one holds.
    #[error("the plugin database lists {0:?} twice")]
    DuplicatePath(PathBuf),
}

/// A path as the record holds it: text, or the raw bytes of a name that is not
/// UTF-8.
#[derive(Serialize)]
#[serde(untagged)]
enum StoredPath<'a> {
    Text(&'a str),
    Bytes { bytes: &'a [u8] },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredPathOwned {
    Text(String),
    Bytes { bytes: Vec<u8> },
}

impl<'a> StoredPath<'a> {
    fn of(path: &'a Path) -> Option<Self> {
        match path.to_str() {
            Some(text) => Some(Self::Text(text)),
            None => os_path::raw_bytes(path).map(|bytes| Self::Bytes { bytes }),
        }
    }
}

impl StoredPathOwned {
    fn into_path(self) -> PathBuf {
        match self {
            Self::Text(text) => PathBuf::from(text),
            Self::Bytes { bytes } => os_path::from_bytes(&bytes),
        }
    }
}

#[derive(Serialize)]
struct Out<'a> {
    version: u64,
    files: Vec<(StoredPath<'a>, &'a FileRow)>,
}

/// The record as read, before it is checked.
#[derive(Deserialize)]
pub(crate) struct Raw {
    #[serde(default)]
    version: u64,
    files: Vec<(StoredPathOwned, FileRow)>,
}

impl TryFrom<Raw> for PluginDb {
    type Error = InvalidDb;

    fn try_from(raw: Raw) -> Result<Self, InvalidDb> {
        if raw.version != DB_VERSION {
            return Err(InvalidDb::Version { found: raw.version });
        }
        let mut db = PluginDb::new();
        for (path, row) in raw.files {
            let path = path.into_path();
            if db.row(&path).is_some() {
                return Err(InvalidDb::DuplicatePath(path));
            }
            db.apply(Delta::put(path, row));
        }
        Ok(db)
    }
}

impl Serialize for PluginDb {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut files = Vec::with_capacity(self.len());
        for (path, row) in self.rows() {
            let stored = StoredPath::of(path).ok_or_else(|| {
                S::Error::custom(format!(
                    "the path {path:?} cannot be stored in the database"
                ))
            })?;
            files.push((stored, row));
        }
        Out {
            version: DB_VERSION,
            files,
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::FileFacts;
    use crate::format::{PluginClass, PluginDescriptor, PluginFormat};
    use crate::table::FileState;
    use tutti_plugin_types::{
        LatencyVerdict, MaxBlock, Measured, NotChecked, PROBE_FLOOR, ProbeConfig, StateHash,
        TailVerdict, Verdict,
    };
    use tutti_types::{Db, SampleRate, Samples};

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

    fn to_json(db: &PluginDb) -> serde_json::Value {
        serde_json::to_value(db).unwrap()
    }

    fn from_json(value: serde_json::Value) -> Result<PluginDb, serde_json::Error> {
        serde_json::from_value(value)
    }

    /// A row of one plugin whose `measured` is `measured`.
    fn measured_row(name: &str, measured: Measured) -> FileRow {
        let mut row = row(name);
        let FileState::Probed(plugins) = &mut row.state else {
            unreachable!("a probed row");
        };
        plugins[0].measured = Some(measured);
        row
    }

    fn measurement(tail: TailVerdict, latency: LatencyVerdict) -> Measured {
        Measured {
            tail: Some(Samples(8191)),
            latency: Some(Samples(64)),
            config: ProbeConfig {
                rate: SampleRate::SR_44K1,
                block: MaxBlock::new(Samples(64)).unwrap(),
                floor: PROBE_FLOOR,
                state: StateHash::of(b"patch"),
            },
            verdict: Verdict { tail, latency },
        }
    }

    /// **A populated database survives a round trip**: probed rows, a
    /// blacklisted file with no time, and a measurement of every verdict a probe
    /// can write read back equal.
    #[test]
    fn a_populated_database_round_trips() {
        let mut db = db_of(&["reverb", "delay"]);
        db.blacklist(
            Path::new("/plugins/crashy.vst3"),
            "segfault".into(),
            &no_file(),
        );
        let held = Db(-60.0);
        let verdicts = [
            (TailVerdict::Honest, LatencyVerdict::Honest),
            (
                TailVerdict::TailLonger {
                    reported: Samples(8191),
                    measured: Samples(12_287),
                },
                LatencyVerdict::LatencyDiffers {
                    reported: Samples(0),
                    measured: Samples(64),
                },
            ),
            (
                TailVerdict::NeverSettles { level: held },
                LatencyVerdict::NeverSettles { level: held },
            ),
            (
                TailVerdict::NotChecked(NotChecked::UnboundedClaim),
                LatencyVerdict::NotChecked(NotChecked::Instrument),
            ),
        ];
        for (k, (tail, latency)) in verdicts.into_iter().enumerate() {
            let name = format!("plugin{k}");
            db.apply(Delta::put(
                format!("/plugins/{name}.vst3"),
                measured_row(&name, measurement(tail, latency)),
            ));
        }

        let back = from_json(to_json(&db)).unwrap();

        assert_eq!(back, db);
        assert_eq!(back.len(), 7);
        assert!(back.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert_eq!(
            back.row(Path::new("/plugins/crashy.vst3")).unwrap().mtime,
            None
        );
        assert_eq!(
            back.plugins()
                .filter(|p| p.descriptor.measured.is_some())
                .count(),
            4
        );
    }

    /// A serialized database declares the version it deserializes.
    #[test]
    fn a_serialized_database_declares_the_version_it_reads() {
        assert_eq!(to_json(&db_of(&["a"]))["version"], DB_VERSION);
    }

    /// Rows come out in path order, so the same table serializes to the same
    /// bytes whatever order it was built in.
    #[test]
    fn insertion_order_does_not_change_the_serialized_bytes() {
        let a = serde_json::to_vec(&db_of(&["x", "y", "z", "w", "u", "v"])).unwrap();
        let b = serde_json::to_vec(&db_of(&["v", "u", "w", "z", "y", "x"])).unwrap();
        assert_eq!(a, b);
    }

    /// A record of another version is refused with the named error, on either
    /// side of this version. Each body is otherwise a valid current-version
    /// body, so a reader that ignored the version would accept it.
    #[test]
    fn a_record_of_another_version_is_refused() {
        let current = to_json(&db_of(&["reverb"]));
        for found in [DB_VERSION - 1, DB_VERSION + 1, 41] {
            let mut value = current.clone();
            value["version"] = found.into();

            let raw: Raw = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(
                PluginDb::try_from(raw),
                Err(InvalidDb::Version { found }),
                "version {found}"
            );
            let message = from_json(value).unwrap_err().to_string();
            assert!(
                message.contains(&format!("version {found}")),
                "version {found}: {message}"
            );
        }
    }

    /// A record with no version is version `0`, not the current one.
    #[test]
    fn a_record_with_no_version_is_refused() {
        let mut value = to_json(&db_of(&["reverb"]));
        value.as_object_mut().unwrap().remove("version");

        let raw: Raw = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            PluginDb::try_from(raw),
            Err(InvalidDb::Version { found: 0 })
        );
        assert!(from_json(value).is_err());
    }

    /// A path listed twice is refused, naming the path: the record does not say
    /// which row holds.
    #[test]
    fn a_path_listed_twice_is_refused() {
        let (first, second) = (row("first"), row("second"));
        let value = serde_json::json!({
            "version": DB_VERSION,
            "files": [["/plugins/a.vst3", first], ["/plugins/a.vst3", second]],
        });

        let raw: Raw = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            PluginDb::try_from(raw),
            Err(InvalidDb::DuplicatePath("/plugins/a.vst3".into()))
        );
        assert!(
            from_json(value)
                .unwrap_err()
                .to_string()
                .contains("/plugins/a.vst3")
        );
    }

    /// The current version with a body of the wrong shape does not deserialize.
    #[test]
    fn the_current_version_with_a_bad_body_is_refused() {
        for body in [
            serde_json::json!({ "version": DB_VERSION, "files": 7 }),
            serde_json::json!({ "version": DB_VERSION }),
            serde_json::json!({ "version": DB_VERSION, "files": [["/p/a.vst3", 5]] }),
            serde_json::json!([]),
        ] {
            assert!(from_json(body.clone()).is_err(), "{body}");
        }
    }

    #[cfg(unix)]
    mod unix_tests {
        use super::*;

        /// A name that is not UTF-8 keeps its row, blacklist included, through a
        /// round trip: a crashing plugin with such a name must not be probed
        /// again after a restart.
        #[test]
        fn a_non_utf8_path_round_trips() {
            use std::os::unix::ffi::OsStrExt;
            let mut db = db_of(&["kept"]);
            let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/plugins/caf\xe9.vst3"));
            db.blacklist(&odd, "boom".into(), &no_file());

            let back = from_json(to_json(&db)).unwrap();

            assert_eq!(back, db);
            assert!(back.is_blacklisted(&odd));
        }
    }
}
