//! Discovery decisions as pure functions over values.
//!
//! The scan reads the filesystem once per candidate into a [`FileFacts`]
//! (`FileFacts::read`, in `fs.rs`); everything here decides from that value and
//! a table row, touching no disk and no clock.

use crate::format::PluginFormat;
use crate::table::FileRow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// What the scan found on disk for one candidate, read once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFacts {
    /// Whether the path exists. Only `tutti_plugin::catalog::Plugins::prune_missing`
    /// reads it.
    pub exists: bool,
    /// Seconds since the Unix epoch of the file's last modification, if it has
    /// one. A row stamped without a time matches a file that reads without one.
    pub mtime: Option<u64>,
    /// The format the path's extension names.
    pub format: Option<PluginFormat>,
}

/// What a scan does with one discovered plugin.
///
/// Exhaustive: these three are the whole outcome, and callers match on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanDecision {
    /// Blacklisted, and the file has not changed since: skip it.
    Blacklisted,
    /// Recorded, and the file has not changed since: skip it.
    UpToDate,
    /// New or changed: probe it.
    Probe,
}

/// Decides what a scan does with a candidate, given its row (if any) and its
/// facts.
///
/// The blacklist is mtime-aware: a blacklisted file whose time changed on disk
/// (a reinstall, a vendor update) is probed again rather than skipped forever.
#[must_use]
pub fn classify(row: Option<&FileRow>, facts: &FileFacts) -> ScanDecision {
    match row {
        Some(r) if is_blacklisted_and_unchanged(r, facts) => ScanDecision::Blacklisted,
        Some(r) if !needs_rescan(r, facts) => ScanDecision::UpToDate,
        _ => ScanDecision::Probe,
    }
}

/// Returns `true` if the file's modification time differs from the one the row
/// stamped.
#[must_use]
pub fn needs_rescan(row: &FileRow, facts: &FileFacts) -> bool {
    facts.mtime != row.mtime
}

/// Returns `true` if the row is blacklisted and its file has not changed since
/// the blacklist was stamped.
#[must_use]
pub fn is_blacklisted_and_unchanged(row: &FileRow, facts: &FileFacts) -> bool {
    row.blacklist_reason().is_some() && !needs_rescan(row, facts)
}

/// Returns the paths among `paths` that are not in `present`.
#[must_use]
pub fn missing<'a>(
    paths: impl Iterator<Item = &'a Path>,
    present: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    paths
        .filter(|path| !present.contains(*path))
        .map(Path::to_path_buf)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{PluginClass, PluginDescriptor};

    const T: u64 = 1_700_000_000;

    fn facts(mtime: Option<u64>) -> FileFacts {
        FileFacts {
            exists: true,
            mtime,
            format: Some(PluginFormat::Clap),
        }
    }

    fn healthy(mtime: Option<u64>) -> FileRow {
        FileRow::probed(
            PluginFormat::Clap,
            &facts(mtime),
            vec![PluginDescriptor::new("id", "name", PluginClass::Unknown)],
        )
    }

    fn bad(mtime: Option<u64>) -> FileRow {
        FileRow::blacklisted("crashed".into(), &facts(mtime))
    }

    #[test]
    fn an_unknown_plugin_is_probed() {
        assert_eq!(classify(None, &facts(Some(T))), ScanDecision::Probe);
    }

    #[test]
    fn a_changed_file_is_probed_whichever_way_the_time_moved() {
        let r = healthy(Some(T));
        assert_eq!(classify(Some(&r), &facts(Some(T + 1))), ScanDecision::Probe);
        assert_eq!(classify(Some(&r), &facts(Some(T - 1))), ScanDecision::Probe);
    }

    #[test]
    fn an_unchanged_row_is_up_to_date() {
        let r = healthy(Some(T));
        assert_eq!(classify(Some(&r), &facts(Some(T))), ScanDecision::UpToDate);
    }

    #[test]
    fn an_unchanged_blacklisted_row_is_skipped_as_blacklisted() {
        let r = bad(Some(T));
        assert_eq!(
            classify(Some(&r), &facts(Some(T))),
            ScanDecision::Blacklisted
        );
    }

    #[test]
    fn a_changed_blacklisted_file_is_readmitted() {
        let r = bad(Some(T));
        assert_eq!(classify(Some(&r), &facts(Some(T + 5))), ScanDecision::Probe);
        assert!(!is_blacklisted_and_unchanged(&r, &facts(Some(T + 5))));
    }

    #[test]
    fn a_healthy_unchanged_row_is_not_blacklisted() {
        let r = healthy(Some(T));
        assert!(!is_blacklisted_and_unchanged(&r, &facts(Some(T))));
    }

    /// A file with no readable time matches only a row stamped without one.
    #[test]
    fn a_missing_time_matches_only_a_row_stamped_without_one() {
        assert!(!needs_rescan(&healthy(None), &facts(None)));
        assert!(needs_rescan(&healthy(Some(T)), &facts(None)));
        assert!(needs_rescan(&healthy(None), &facts(Some(T))));
    }

    /// Facts at T2 over a row at T1, then a blacklisting with those facts: the
    /// classification skips it. This is the scanner's path (a probe fails, the
    /// row is stamped with the facts the decision saw).
    #[test]
    fn blacklisting_a_changed_file_with_the_facts_makes_it_skipped() {
        let r = healthy(Some(T));
        let f = facts(Some(T + 9));
        assert_eq!(classify(Some(&r), &f), ScanDecision::Probe);
        let out = FileRow::blacklisted("boom".into(), &f);
        assert_eq!(out.mtime, Some(T + 9));
        assert_eq!(classify(Some(&out), &f), ScanDecision::Blacklisted);
    }

    #[test]
    fn a_blacklisted_row_takes_the_format_and_the_time_from_the_facts() {
        let out = FileRow::blacklisted("boom".into(), &facts(Some(T)));
        assert_eq!(out.format, PluginFormat::Clap);
        assert_eq!(out.mtime, Some(T));
        assert_eq!(out.blacklist_reason(), Some("boom"));
        assert!(out.plugins().is_empty());
    }

    #[test]
    fn a_blacklisted_row_for_an_unrecognised_extension_defaults_to_vst3() {
        let none = FileFacts {
            exists: false,
            mtime: None,
            format: None,
        };
        let out = FileRow::blacklisted("boom".into(), &none);
        assert_eq!(out.format, PluginFormat::Vst3);
        assert_eq!(out.mtime, None);
    }

    #[test]
    fn missing_is_exactly_the_paths_not_present() {
        let a = Path::new("/plugins/a.clap");
        let b = Path::new("/plugins/b.clap");
        let present: HashSet<PathBuf> = [a.to_path_buf()].into();
        assert_eq!(missing([a, b].into_iter(), &present), vec![b.to_path_buf()]);
        assert!(missing([a].into_iter(), &present).is_empty());
        assert_eq!(
            missing([a].into_iter(), &HashSet::new()),
            vec![a.to_path_buf()]
        );
    }
}
