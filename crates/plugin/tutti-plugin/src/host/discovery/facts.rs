//! Discovery decisions as pure functions over values.
//!
//! The scan reads the filesystem once per candidate into a [`FileFacts`]
//! (`FileFacts::read`, in `fs.rs`); everything here decides from that value and
//! a catalog record, touching no disk and no clock.

use super::record::{Blacklist, PluginDescriptor, PluginFormat, PluginRecord};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// What the scan found on disk for one candidate, read once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFacts {
    /// Whether the path exists.
    pub exists: bool,
    /// Seconds since the Unix epoch of the file's last modification, if it has
    /// one. An absent time compares as `0`, the value an unstamped record holds.
    pub mtime: Option<u64>,
    /// The format the path's extension names.
    pub format: Option<PluginFormat>,
}

impl FileFacts {
    /// The modification time as a record stores it.
    fn stamp(&self) -> u64 {
        self.mtime.unwrap_or(0)
    }
}

/// What a scan does with one discovered plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanDecision {
    /// Blacklisted, and the file has not changed since: skip it.
    Blacklisted,
    /// Recorded, and the file has not changed since: skip it.
    UpToDate,
    /// New or changed: probe it.
    Probe,
}

/// Decides what a scan does with a candidate, given its record (if any) and its
/// facts.
///
/// The blacklist is mtime-aware: a blacklisted plugin whose file changed on disk
/// (a reinstall, a vendor update) is probed again rather than skipped forever.
#[must_use]
pub fn classify(record: Option<&PluginRecord>, facts: &FileFacts) -> ScanDecision {
    match record {
        Some(r) if is_blacklisted_and_unchanged(r, facts) => ScanDecision::Blacklisted,
        Some(r) if !needs_rescan(r, facts) => ScanDecision::UpToDate,
        _ => ScanDecision::Probe,
    }
}

/// Returns `true` if the file's modification time differs from the one the record
/// stamped.
#[must_use]
pub fn needs_rescan(record: &PluginRecord, facts: &FileFacts) -> bool {
    facts.stamp() != record.modification_time
}

/// Returns `true` if the record is blacklisted and its file has not changed since
/// the blacklist was stamped.
#[must_use]
pub fn is_blacklisted_and_unchanged(record: &PluginRecord, facts: &FileFacts) -> bool {
    record.blacklist.is_blacklisted() && !needs_rescan(record, facts)
}

/// Builds the record a blacklisting stores: the existing one (or a stub for
/// `path`) marked with `reason` and stamped with the file's current time.
///
/// The stamp is what lets a reinstall re-admit the plugin. A stub carries a
/// placeholder descriptor and the format its extension names, or VST3 when it
/// names none.
#[must_use]
pub fn blacklist_record(
    existing: Option<&PluginRecord>,
    path: &Path,
    reason: String,
    facts: &FileFacts,
) -> PluginRecord {
    let blacklist = Blacklist::Blacklisted { reason };
    let modification_time = facts.stamp();
    match existing {
        Some(record) => PluginRecord {
            blacklist,
            modification_time,
            ..record.clone()
        },
        None => PluginRecord {
            path: path.to_path_buf(),
            format: facts.format.unwrap_or(PluginFormat::Vst3),
            descriptor: PluginDescriptor::default(),
            modification_time,
            blacklist,
        },
    }
}

/// Returns the paths of `records` that are not in `present`.
#[must_use]
pub fn missing<'a>(
    records: impl Iterator<Item = &'a PluginRecord>,
    present: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    records
        .filter(|r| !present.contains(&r.path))
        .map(|r| r.path.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 1_700_000_000;

    fn facts(mtime: Option<u64>) -> FileFacts {
        FileFacts {
            exists: true,
            mtime,
            format: Some(PluginFormat::Clap),
        }
    }

    fn record(mtime: u64, blacklist: Blacklist) -> PluginRecord {
        PluginRecord {
            path: PathBuf::from("/plugins/a.clap"),
            format: PluginFormat::Clap,
            descriptor: PluginDescriptor::default(),
            modification_time: mtime,
            blacklist,
        }
    }

    fn bad() -> Blacklist {
        Blacklist::Blacklisted {
            reason: "crashed".into(),
        }
    }

    // Mutation records: each rule below names the edit that makes its test fail.

    /// Mutation: return `UpToDate` or `Blacklisted` for `None`.
    #[test]
    fn an_unknown_plugin_is_probed() {
        assert_eq!(classify(None, &facts(Some(T))), ScanDecision::Probe);
    }

    /// Mutation: compare with `<` or `>` instead of `!=` in `needs_rescan`
    /// (the older and the newer file cases each fail one of the two asserts).
    #[test]
    fn a_changed_file_is_probed_whichever_way_the_time_moved() {
        let r = record(T, Blacklist::Ok);
        assert_eq!(classify(Some(&r), &facts(Some(T + 1))), ScanDecision::Probe);
        assert_eq!(classify(Some(&r), &facts(Some(T - 1))), ScanDecision::Probe);
    }

    /// Mutation: make `classify` return `Probe` for an unchanged record.
    #[test]
    fn an_unchanged_record_is_up_to_date() {
        let r = record(T, Blacklist::Ok);
        assert_eq!(classify(Some(&r), &facts(Some(T))), ScanDecision::UpToDate);
    }

    /// Mutation: test `needs_rescan` before the blacklist flag in `classify`
    /// (an unchanged blacklisted record then reports `UpToDate`).
    #[test]
    fn an_unchanged_blacklisted_record_is_skipped_as_blacklisted() {
        let r = record(T, bad());
        assert_eq!(
            classify(Some(&r), &facts(Some(T))),
            ScanDecision::Blacklisted
        );
    }

    /// Mutation: drop the `!needs_rescan` term from `is_blacklisted_and_unchanged`
    /// (a changed blacklisted file is then skipped forever).
    #[test]
    fn a_changed_blacklisted_file_is_readmitted() {
        let r = record(T, bad());
        assert_eq!(classify(Some(&r), &facts(Some(T + 5))), ScanDecision::Probe);
        assert!(!is_blacklisted_and_unchanged(&r, &facts(Some(T + 5))));
    }

    /// Mutation: drop the `is_blacklisted` term from `is_blacklisted_and_unchanged`.
    #[test]
    fn a_healthy_unchanged_record_is_not_blacklisted() {
        let r = record(T, Blacklist::Ok);
        assert!(!is_blacklisted_and_unchanged(&r, &facts(Some(T))));
    }

    /// An unreadable time compares as 0, the stamp of a record cleared for
    /// rescan. Mutation: map `None` to `u64::MAX` in `stamp`; or compare the
    /// `Option` directly against `Some(record.modification_time)`.
    #[test]
    fn a_missing_time_compares_as_zero() {
        assert!(!needs_rescan(&record(0, Blacklist::Ok), &facts(None)));
        assert!(needs_rescan(&record(T, Blacklist::Ok), &facts(None)));
    }

    /// Mutation: keep `record.modification_time` instead of the facts' time in
    /// the `Some` arm (the plugin is then re-probed on every scan after a
    /// blacklisting of a changed file).
    #[test]
    fn blacklisting_an_existing_record_restamps_it_and_keeps_its_fields() {
        let r = record(T, Blacklist::Ok);
        let out = blacklist_record(Some(&r), &r.path, "boom".into(), &facts(Some(T + 9)));
        assert_eq!(out.modification_time, T + 9);
        assert_eq!(out.blacklist.reason(), Some("boom"));
        assert_eq!(out.path, r.path);
        assert_eq!(out.format, PluginFormat::Clap);
        assert!(is_blacklisted_and_unchanged(&out, &facts(Some(T + 9))));
    }

    /// Mutation: stamp `0` instead of the facts' time; or take the format from
    /// anything but the facts.
    #[test]
    fn a_stub_takes_the_path_the_format_and_the_time_from_the_facts() {
        let path = Path::new("/plugins/new.clap");
        let out = blacklist_record(None, path, "boom".into(), &facts(Some(T)));
        assert_eq!(out.path, path);
        assert_eq!(out.format, PluginFormat::Clap);
        assert_eq!(out.modification_time, T);
        assert!(out.blacklist.is_blacklisted());
    }

    /// Mutation: default to `Clap` (or drop the default) instead of `Vst3`.
    #[test]
    fn a_stub_for_an_unrecognised_extension_defaults_to_vst3() {
        let none = FileFacts {
            exists: false,
            mtime: None,
            format: None,
        };
        let out = blacklist_record(None, Path::new("/x/y"), "boom".into(), &none);
        assert_eq!(out.format, PluginFormat::Vst3);
        assert_eq!(out.modification_time, 0);
    }

    /// Mutation: invert the `contains` test, or return every path.
    #[test]
    fn missing_is_exactly_the_records_not_present() {
        let a = record(0, Blacklist::Ok);
        let b = PluginRecord {
            path: PathBuf::from("/plugins/b.clap"),
            ..record(0, Blacklist::Ok)
        };
        let present: HashSet<PathBuf> = [a.path.clone()].into();
        assert_eq!(
            missing([&a, &b].into_iter(), &present),
            vec![b.path.clone()]
        );
        assert!(missing([&a].into_iter(), &present).is_empty());
        assert_eq!(missing([&a].into_iter(), &HashSet::new()), vec![a.path]);
    }
}
