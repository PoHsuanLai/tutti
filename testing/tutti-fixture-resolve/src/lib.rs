//! Finding the reference plugin cdylibs and the `plugin-server` binary the
//! host test suites load.
//!
//! A test, bench or example binary lives at `<profile>/deps/<exe>` (examples at
//! `<profile>/examples/<exe>`), under `<target>` or `<target>/<triple>`, and
//! cargo drops the dev-dependency cdylibs and the workspace's binaries into the
//! same profile directory. So the running executable names the profile
//! directory ([`profile_dir`]) and nothing else is configured: no build script,
//! no environment variable, and the answer is right under `CARGO_TARGET_DIR` and
//! `--target`, because a test binary is built for the target it runs on.
//!
//! This crate is a dev-dependency only; it is never linked into a shipped
//! binary.
//!
//! ## The two rules, and what they cost when broken
//!
//! **Absence is a hard failure, never a skip.** The plugins are built by the
//! same `cargo test` invocation that runs the tests, so a missing one is a build
//! failure rather than a property of the machine. A skip would leave the suite
//! one typo away from reporting success having executed nothing, as when nine
//! VST3 tests skipped while printing `test result: ok. 9 passed`.
//!
//! **The newest candidate wins, not the first.** Cargo writes a cdylib to
//! `<profile>/deps/<name>` and hardlinks it up to `<profile>/<name>` without
//! always refreshing the uplifted copy, so the more obvious path can hold an
//! *older build of the same plugin*. A stale probe makes a suite run green
//! against behaviour the tests no longer set, and can silently reverse a
//! mutation-verification result.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// The profile directory a binary at `exe` belongs to: its parent, or its
/// grandparent when the parent is `deps` or `examples`.
///
/// Pure over the path, so the layouts are testable without cargo.
pub fn profile_dir_of(exe: &Path) -> PathBuf {
    let dir = exe.parent().expect("an executable path has a parent");
    let in_artifact_dir = dir
        .file_name()
        .is_some_and(|n| n == "deps" || n == "examples");
    if in_artifact_dir {
        dir.parent().unwrap_or(dir).to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

/// The profile directory of the running test, bench or example binary.
///
/// # Panics
///
/// If the OS cannot name the executable.
pub fn profile_dir() -> PathBuf {
    profile_dir_of(&std::env::current_exe().expect("the running executable has a path"))
}

/// The cdylib filename for the target being built, e.g.
/// `libtutti_clap_test_plugin.so`.
///
/// `lib_stem` is the crate name with `-` replaced by `_`, as cargo spells it.
pub fn lib_filename(lib_stem: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{lib_stem}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{lib_stem}.dylib")
    } else {
        format!("lib{lib_stem}.so")
    }
}

/// Where cargo may have put the artifact `name` under `profile`:
/// `<profile>/deps/<name>` and `<profile>/<name>`.
pub fn candidates_in(profile: &Path, name: &str) -> [PathBuf; 2] {
    [profile.join("deps").join(name), profile.join(name)]
}

/// The newest existing regular file among `candidates`, or `None`.
///
/// **Newest, not first**: see the module docs. Directories are rejected, since
/// only a regular file can be `dlopen`ed.
pub fn newest_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .filter(|c| c.is_file())
        .filter_map(|c| {
            let mtime = std::fs::metadata(c).and_then(|m| m.modified()).ok()?;
            Some((mtime, c))
        })
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, c)| c.clone())
}

fn searched(candidates: &[PathBuf]) -> String {
    candidates
        .iter()
        .map(|c| format!("\n  - {}", c.display()))
        .collect()
}

/// The reference plugin cdylib for `lib_stem` (a crate name with `_`), the
/// newest copy under the running binary's profile directory.
///
/// `what` names the plugin crate for the panic message.
///
/// # Panics
///
/// If it does not exist, naming every path tried. The plugin is a
/// dev-dependency built by the same `cargo test`, so this is a build failure,
/// not a missing install.
pub fn cdylib(lib_stem: &str, what: &str) -> PathBuf {
    let tried = candidates_in(&profile_dir(), &lib_filename(lib_stem));
    newest_existing(&tried).unwrap_or_else(|| {
        panic!(
            "reference plugin `{what}` not found.\nSearched:{}\n\
             It is a dev-dependency built by this same `cargo test`, so this is \
             a build failure, not a missing install.",
            searched(&tried)
        )
    })
}

/// The `plugin-server` binary under the running binary's profile directory.
///
/// # Panics
///
/// If it has not been built. It is not a dev-dependency of the crates whose
/// tests spawn it (`tutti-plugin-server` depends on `tutti-plugin`, so the edge
/// would be a cycle), so running tests does not build it.
pub fn plugin_server() -> PathBuf {
    let name = if cfg!(windows) {
        "plugin-server.exe"
    } else {
        "plugin-server"
    };
    let tried = candidates_in(&profile_dir(), name);
    newest_existing(&tried).unwrap_or_else(|| {
        panic!(
            "`plugin-server` not found.\nSearched:{}\n\
             It is not a dev-dependency (that would be a cycle through \
             `tutti-plugin`), so running tests does not build it. Build it \
             first, then re-run:\n\n  cargo build -p tutti-plugin-server\n",
            searched(&tried)
        )
    })
}

/// Publish `real` under its own name with `extension` (`.clap`, say) beside
/// it, and return that path.
///
/// The extension is load-bearing for a host that dispatches on it: cargo's
/// `libtutti_clap_test_plugin.so` reads as VST2 there. The link path is shared
/// by every test process of every suite, so it is published by atomic rename from a per-process
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
    ///
    /// Mutation: return the first existing candidate instead of the newest ->
    /// the older, first-listed file wins -> fails.
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

        let picked = newest_existing(&[old.clone(), new.clone()]).expect("one exists");
        assert_eq!(
            picked, new,
            "the older candidate was listed first and must not win"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory at a candidate path is not a plugin: `metadata` succeeds on
    /// a directory, `dlopen` does not.
    ///
    /// Mutation: filter on `exists()` instead of `is_file()` -> the directory
    /// is returned -> fails.
    #[test]
    fn a_directory_is_not_a_candidate() {
        let dir = std::env::temp_dir().join(format!("tfr-dir-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(newest_existing(std::slice::from_ref(&dir)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A test or bench binary is in `deps/`, an example in `examples/`, and a
    /// plain binary beside its profile; a target triple only adds a level above.
    ///
    /// Mutation: drop the `examples` arm of `in_artifact_dir` -> the example
    /// case returns `.../debug/examples` -> fails; drop the `deps` arm -> the
    /// test case fails.
    #[test]
    fn the_profile_dir_is_derived_from_the_executable_path() {
        let p = |s: &str| profile_dir_of(Path::new(s));
        assert_eq!(p("/t/debug/deps/a-1f"), Path::new("/t/debug"));
        assert_eq!(p("/t/debug/examples/ex"), Path::new("/t/debug"));
        assert_eq!(p("/t/release/plugin-server"), Path::new("/t/release"));
        assert_eq!(
            p("/t/x86_64-pc-windows-msvc/debug/deps/a-1f.exe"),
            Path::new("/t/x86_64-pc-windows-msvc/debug")
        );
        assert_eq!(
            p("/t/aarch64-apple-darwin/release/examples/ex"),
            Path::new("/t/aarch64-apple-darwin/release")
        );
    }

    #[test]
    fn candidates_are_deps_then_the_uplifted_copy() {
        assert_eq!(
            candidates_in(Path::new("/t/debug"), "x"),
            [
                PathBuf::from("/t/debug/deps/x"),
                PathBuf::from("/t/debug/x")
            ]
        );
    }

    #[test]
    fn lib_filename_follows_the_target_os() {
        // Assert the shape rather than one platform's answer, so this holds on
        // every runner.
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
