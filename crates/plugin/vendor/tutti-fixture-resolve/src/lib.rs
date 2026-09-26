//! Finding the reference plugin cdylibs the host test suites load.
//!
//! Four crates build a reference plugin as a dev-dependency and then have to
//! locate the artifact — `tutti-plugin` and `tutti-vst2-host` (both resolving
//! `tutti-vst2-test-plugin`), `tutti-clap-host` and `tutti-plugin-server` (both
//! resolving `tutti-clap-test-plugin`). Each had its own copy of this logic:
//! four `build.rs` files with a byte-identical `resolve_target_dir`, and four
//! spellings of the same mtime-max resolver. They had begun to drift — one
//! build script had lost its `rerun-if-env-changed` for the very variable
//! `resolve_target_dir` reads, one resolver accepted a *directory* where the
//! others required a file, and two carried contradictory comments about whether
//! candidate order mattered.
//!
//! Both halves live here because a build script cannot `include!` across a
//! package boundary; it needs a real crate, which is what this is.
//!
//! ## The two rules, and what they cost when broken
//!
//! **Absence is a hard failure, never a skip.** These plugins are built by the
//! same `cargo test` invocation that runs the tests, so a missing one is a build
//! failure rather than a property of the machine. A skip would leave the suite
//! one typo away from reporting success having executed nothing — which has
//! happened twice here, most memorably nine VST3 tests skipping while printing
//! `test result: ok. 9 passed`.
//!
//! **The newest candidate wins, not the first.** Cargo writes a cdylib to
//! `<profile>/deps/<name>` and hardlinks it up to `<profile>/<name>` without
//! always refreshing the uplifted copy, so the more obvious path can hold an
//! *older build of the same plugin*. That is the nastier failure: a stale probe
//! makes a suite run green against behaviour the tests no longer set — it once
//! silently reversed a mutation-verification result, which is the one thing
//! mutation testing exists to be trusted for.

use std::path::{Path, PathBuf};

/// The cdylib filename for the target being built, e.g.
/// `libtutti_clap_test_plugin.so`.
///
/// `lib_stem` is the crate name with `-` replaced by `_`, as cargo spells it.
/// Reads `CARGO_CFG_TARGET_OS` when set (build-script context, and correct
/// under cross-compilation) and falls back to the host `cfg!` otherwise.
pub fn lib_filename(lib_stem: &str) -> String {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_else(|_| {
        if cfg!(target_os = "windows") {
            "windows".into()
        } else if cfg!(target_os = "macos") {
            "macos".into()
        } else {
            "linux".into()
        }
    });
    match os.as_str() {
        "windows" => format!("{lib_stem}.dll"),
        "macos" => format!("lib{lib_stem}.dylib"),
        _ => format!("lib{lib_stem}.so"),
    }
}

/// Resolve `<target-dir>` — the directory holding `debug/`, `release/`.
///
/// Honors `CARGO_TARGET_DIR` when set (this workspace points it at an external
/// disk). Otherwise derives it from `OUT_DIR`, which cargo shapes as
/// `<target>/<profile>/build/<pkg>-<hash>/out` — the 5th ancestor.
///
/// Build-script context only: both variables come from cargo.
pub fn target_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    out_dir
        .ancestors()
        .nth(4)
        .map(Path::to_path_buf)
        .unwrap_or(out_dir)
}

/// The places cargo may drop a dev-dependency's cdylib, newest-first-agnostic.
///
/// Cargo does not promise *where* under the profile directory the artifact
/// lands: the default layout puts it at `<profile>/<name>`, but under an
/// explicit `CARGO_TARGET_DIR` (or `--target <triple>`) it has been observed
/// only in `<profile>/deps/<name>`. Both are returned so a layout shift degrades
/// into a slower lookup rather than a silently skipped suite.
///
/// Order is not significant — [`newest_existing`] compares mtimes.
pub fn candidates(lib_stem: &str) -> Vec<PathBuf> {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let profile_dir = target_dir().join(profile);
    let name = lib_filename(lib_stem);
    vec![
        profile_dir.join("deps").join(&name),
        profile_dir.join(&name),
    ]
}

/// Serialize candidate paths for a `cargo:rustc-env` line.
///
/// `;` rather than `:` so the value survives a Windows drive letter.
pub fn join_candidates(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(";")
}

/// Emit everything a dependent's tests need to find `lib_stem`'s cdylib:
/// the candidate list under `var`, plus the `rerun-if` directives that keep it
/// correct.
///
/// The whole build-script side, so a caller's `main` is one line. Missing
/// `rerun-if-env-changed=CARGO_TARGET_DIR` is precisely the drift found in one
/// of the four copies this replaced — the script would not re-run when the
/// variable its own path resolution reads had changed.
pub fn emit_candidates(var: &str, lib_stem: &str) {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");
    println!(
        "cargo:rustc-env={var}={}",
        join_candidates(&candidates(lib_stem))
    );
}

/// The newest existing file among `candidates`, or `None` if none exist.
///
/// **Newest, not first** — see the module docs for why a stale artifact is worse
/// than a missing one. Directories are rejected: only a regular file can be
/// `dlopen`ed, and one copy of this logic used to accept a directory because it
/// checked only that `metadata` succeeded.
pub fn newest_existing<'a, I>(candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    candidates
        .into_iter()
        .filter(|c| !c.is_empty())
        .filter(|c| Path::new(c).is_file())
        .filter_map(|c| {
            let mtime = std::fs::metadata(c).and_then(|m| m.modified()).ok()?;
            Some((mtime, c))
        })
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, c)| c)
}

/// The newest existing candidate in a `;`-separated list, or a panic naming
/// every path tried.
///
/// The test-side entry point, pairing with [`emit_candidates`]. It panics
/// rather than returning `Option` deliberately — see the module docs.
///
/// `what` names the plugin for the panic message (e.g.
/// `"tutti-clap-test-plugin"`).
pub fn resolve_or_panic(candidate_list: &str, what: &str) -> String {
    match newest_existing(candidate_list.split(';')) {
        Some(path) => path.to_string(),
        None => {
            let searched = candidate_list
                .split(';')
                .filter(|s| !s.is_empty())
                .map(|s| format!("\n  - {s}"))
                .collect::<String>();
            panic!(
                "reference plugin `{what}` not found.\nSearched:{searched}\n\
                 It is a dev-dependency built by this same `cargo test`, so this \
                 is a build failure, not a missing install. If the artifact \
                 landed somewhere not listed, build.rs needs a new candidate."
            )
        }
    }
}

/// Publish `real` under its own name with `extension` (`.clap`, say) beside
/// it, and return that path.
///
/// The extension is load-bearing for a host that dispatches on it: cargo's
/// `libtutti_clap_test_plugin.so` reads as VST2 there. The link path is shared
/// by every test process of every suite (the candidate list is baked in at
/// build time), so it is published by atomic rename from a per-process
/// staging name: a `remove_file` + `symlink` pair once left it briefly absent.
///
/// **On Unix a link that already points at `real` is left alone.** On a macOS
/// runner a process loading the link while other processes renamed fresh
/// links over it read it as absent, and the load failed with "Plugin not
/// found" (twice in a row, once more suites came to publish the link; APFS
/// does not promise a concurrent lookup never misses a path being renamed
/// over). Every test process used to republish; now only the first does, or
/// one that finds the link naming something else. Nothing goes stale by
/// skipping: a symlink is resolved at each open, so one naming `real` is as
/// fresh as `real`. A Windows copy can go stale, so it is still republished.
///
/// # Panics
///
/// If the link cannot be staged, or is absent after a lost rename.
pub fn publish_with_extension(real: &Path, extension: &str) -> PathBuf {
    let link = real.with_extension(extension);
    #[cfg(unix)]
    if std::fs::read_link(&link).is_ok_and(|to| to == real) {
        return link;
    }

    // Stage under a name no other process can pick, then swap it in.
    let staging = link.with_extension(format!("{extension}.tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&staging);
    #[cfg(unix)]
    std::os::unix::fs::symlink(real, &staging).expect("stage the reference plugin symlink");
    #[cfg(windows)]
    std::fs::copy(real, &staging).expect("stage the reference plugin copy");

    if let Err(e) = std::fs::rename(&staging, &link) {
        // Losing the swap is not a failure: whoever won published a link to
        // the same artifact. Only a missing result is fatal.
        let _ = std::fs::remove_file(&staging);
        assert!(
            link.exists(),
            "publish the reference plugin at {}: {e}",
            link.display()
        );
    }
    link
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The rule the module docs call load-bearing: newest wins, so a stale
    /// uplifted copy cannot shadow a fresh one in `deps/`.
    #[test]
    fn newest_wins_over_first() {
        let dir = std::env::temp_dir().join(format!("tfr-newest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let old = dir.join("old.so");
        let new = dir.join("new.so");
        std::fs::File::create(&old)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        // Coarse filesystem timestamps: without a gap the two mtimes can be
        // equal and `max_by_key` would return either, so the test would pass
        // for the wrong reason.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::File::create(&new)
            .unwrap()
            .write_all(b"x")
            .unwrap();

        let list = format!("{};{}", old.display(), new.display());
        let picked = newest_existing(list.split(';')).expect("one exists");
        assert_eq!(
            picked,
            new.display().to_string(),
            "the older candidate was listed first and must not win"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory at a candidate path is not a plugin. One of the four
    /// resolvers this replaced accepted one, because `metadata` succeeds on a
    /// directory.
    #[test]
    fn a_directory_is_not_a_candidate() {
        let dir = std::env::temp_dir().join(format!("tfr-dir-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(newest_existing(dir.display().to_string().split(';')), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_segments_are_ignored() {
        assert_eq!(newest_existing(";;".split(';')), None);
    }

    #[test]
    fn lib_filename_follows_the_target_os() {
        // Reads the ambient target; assert the shape rather than one platform's
        // answer, so this holds on every runner.
        let name = lib_filename("some_plugin");
        assert!(
            name.ends_with(".so") || name.ends_with(".dylib") || name.ends_with(".dll"),
            "unexpected cdylib name: {name}"
        );
        assert!(name.contains("some_plugin"));
    }

    /// **A link already naming the artifact is not republished.** The
    /// link's own inode is the witness: a republish renames a fresh symlink
    /// over it, which a second call must not do.
    ///
    /// Mutation: drop the `read_link` early return -> the second call renames
    /// a new link in -> the inode changes -> fails.
    #[cfg(unix)]
    #[test]
    fn a_published_link_is_left_alone() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("tfr-link-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let real = dir.join("libplugin.so");
        std::fs::File::create(&real)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        let link = publish_with_extension(&real, "clap");
        assert_eq!(link, dir.join("libplugin.clap"));
        assert_eq!(std::fs::read_link(&link).unwrap(), real);
        let first = std::fs::symlink_metadata(&link).unwrap().ino();
        assert_eq!(publish_with_extension(&real, "clap"), link);
        let second = std::fs::symlink_metadata(&link).unwrap().ino();
        assert_eq!(first, second, "the second publish replaced the link");

        // A link naming something else is replaced.
        let other = dir.join("other.so");
        std::fs::File::create(&other).unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&other, &link).unwrap();
        publish_with_extension(&real, "clap");
        assert_eq!(std::fs::read_link(&link).unwrap(), real);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
