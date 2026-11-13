//! `Catalog::scan` against a directory of real files: a plugin file added
//! between two scans is found by the second, and only it is new (feature
//! `clap`, the reference plugin; `cargo build -p tutti-plugin-probe` first, or
//! the scan degrades to filename metadata, which still lists the file).

#![cfg(feature = "clap")]

use tutti_plugin::catalog::{Catalog, CatalogConfig};

/// **A scan picks up a file that appeared since the last one**, and counts only
/// that file as new: the catalog starts empty over an empty directory, a
/// plugin dropped in is listed by the next `scan()`, and a second file dropped
/// in after that is added beside the first, which is not probed again.
#[test]
fn scan_picks_up_a_new_file() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let scanned_dir = dir.path().join("plugins");
    std::fs::create_dir(&scanned_dir).expect("the scanned directory");
    let mut catalog = Catalog::empty(CatalogConfig::new(
        dir.path().join("db.json"),
        vec![scanned_dir.clone()],
    ));
    let files = |catalog: &Catalog| {
        let mut files: Vec<_> = catalog
            .iter()
            .map(|(plugin, _)| plugin.path().to_path_buf())
            .collect();
        files.sort();
        files.dedup();
        files
    };

    let empty = catalog.scan().expect("no scan is running");
    assert_eq!((empty.scanned, empty.new), (0, 0));
    assert!(files(&catalog).is_empty());

    let first = scanned_dir.join("First.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &first).expect("drop a plugin in");
    let found = catalog.scan().expect("no scan is running");
    assert_eq!((found.scanned, found.new), (1, 1));
    let canonical = |path: &std::path::Path| tutti_plugin_db::canonical_path(path);
    assert_eq!(files(&catalog), [canonical(&first)]);

    let second = scanned_dir.join("Second.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &second).expect("drop another in");
    let found = catalog.scan().expect("no scan is running");
    assert_eq!(
        (found.scanned, found.new),
        (2, 1),
        "two files are in the directory and only the new one is probed"
    );
    let mut both = [canonical(&first), canonical(&second)];
    both.sort();
    assert_eq!(files(&catalog), both);
}

/// A catalog over a directory holding one copy of the reference plugin, and the
/// copy's path.
fn catalog_with_one_plugin(dir: &tempfile::TempDir) -> (Catalog, std::path::PathBuf) {
    let scanned_dir = dir.path().join("plugins");
    std::fs::create_dir(&scanned_dir).expect("the scanned directory");
    let file = scanned_dir.join("Only.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &file).expect("drop a plugin in");
    let catalog = Catalog::empty(CatalogConfig::new(
        dir.path().join("db.json"),
        vec![scanned_dir],
    ));
    (catalog, file)
}

/// **A scan measures only when the catalog was told to** (doc 023 section 6): a
/// plain scan leaves `measured` empty, a catalog `with_measuring` records what
/// the probe found, the configuration it was found at and a verdict.
#[test]
fn a_scan_measures_only_when_asked() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (mut plain, _) = catalog_with_one_plugin(&dir);
    plain.scan().expect("no scan is running");
    assert!(
        plain.iter().all(|(_, d)| d.measured.is_none()),
        "a plain scan measures nothing"
    );

    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (catalog, _) = catalog_with_one_plugin(&dir);
    let mut measuring = catalog.with_measuring();
    measuring.scan().expect("no scan is running");
    let (_, descriptor) = measuring.iter().next().expect("the plugin is listed");
    let measured = descriptor.measured.expect("the scan measured it");
    assert_eq!(
        measured.config.rate,
        tutti_plugin_types::SampleRate(48_000.0)
    );
}

/// **`measure` measures a file a scan already probed**, which a scan never
/// revisits while the file is unchanged, and replaces its row.
#[test]
fn measure_measures_a_plugin_a_scan_left_unmeasured() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (mut catalog, _) = catalog_with_one_plugin(&dir);
    catalog.scan().expect("no scan is running");
    let (plugin, descriptor) = catalog.iter().next().expect("the plugin is listed");
    assert_eq!(descriptor.measured, None);

    catalog.measure(&plugin).expect("the probe measures it");
    let (_, descriptor) = catalog.iter().next().expect("still listed");
    assert!(descriptor.measured.is_some());
}
