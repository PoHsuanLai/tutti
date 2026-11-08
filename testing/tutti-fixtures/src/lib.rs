//! Where the test suites find things: the repo's `assets/`, the reference plugin
//! cdylibs and the `plugin-server` binary; [`check_range_contract`], the one
//! check a range-checked type keeps its contract; and [`sf2`], a SoundFont built
//! in memory.
//!
//! A test finds a repo file through [`repo_root`] and [`asset`], never through
//! a relative `..` path, so moving a crate never changes what a test reads.
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

mod range_contract;
pub mod sf2;

pub use range_contract::{RangeContract, check_range_contract};

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The first ancestor of `start` (itself included) that holds a `Cargo.lock`.
///
/// Pure over the filesystem it is given, so the walk is testable on a temp tree.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
        .map(Path::to_path_buf)
}

/// The repository root: the first ancestor of this crate's manifest directory
/// that holds a `Cargo.lock`.
///
/// # Panics
///
/// If there is none, naming the start.
pub fn repo_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let own = Path::new(env!("CARGO_MANIFEST_DIR"));
        find_root(own).unwrap_or_else(|| {
            panic!(
                "no Cargo.lock above {}; the repo root cannot be found",
                own.display()
            )
        })
    })
}

/// A file under the repo's `assets/`, e.g. `asset("audio/stereo.flac")`.
///
/// # Panics
///
/// If the file is absent, naming the full path (and, for a soundfont, how to
/// fetch it). A missing asset fails the test; it never skips it.
pub fn asset(rel: &str) -> PathBuf {
    // One component at a time, so the path and the panic are native on every
    // platform (`rel` always uses `/`).
    let path = rel
        .split('/')
        .fold(repo_root().join("assets"), |dir, part| dir.join(part));
    assert!(
        path.is_file(),
        "test asset missing at {}.{}",
        path.display(),
        if rel.starts_with("soundfonts/") {
            " Fetch it with `assets/soundfonts/download-timgm6mb.sh` (see \
             `assets/soundfonts/README.md`)."
        } else {
            ""
        }
    );
    path
}

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
    #[cfg(unix)]
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
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&new, b"x").unwrap();
        // Set the mtimes outright: equal mtimes would let `max_by_key` return
        // either file, and the test would pass for the wrong reason.
        let now = std::time::SystemTime::now();
        let age = |file: &Path, by: u64| {
            std::fs::File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(now - std::time::Duration::from_secs(by))
                .unwrap();
        };
        age(&old, 100);
        age(&new, 10);

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

    /// The root is the nearest ancestor with a `Cargo.lock`, not the farthest,
    /// and the start itself counts.
    ///
    /// Mutation: walk `ancestors().skip(1)` -> the start-is-root case fails;
    /// pick the last match instead of the first -> the nested case returns the
    /// outer lock -> fails.
    #[test]
    fn the_root_is_the_nearest_ancestor_with_a_lockfile() {
        let base = std::env::temp_dir().join(format!("tfx-root-{}", std::process::id()));
        let inner = base.join("outer/inner/deep");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(base.join("outer/Cargo.lock"), "").unwrap();
        assert_eq!(find_root(&inner), Some(base.join("outer")));
        std::fs::write(base.join("outer/inner/Cargo.lock"), "").unwrap();
        assert_eq!(find_root(&inner), Some(base.join("outer/inner")));
        assert_eq!(
            find_root(&base.join("outer/inner")),
            Some(base.join("outer/inner")),
            "the start itself counts"
        );
        std::fs::remove_file(base.join("outer/Cargo.lock")).unwrap();
        std::fs::remove_file(base.join("outer/inner/Cargo.lock")).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The root found is the real repo root, with the assets under it.
    ///
    /// Mutation: look for `Cargo.toml` instead of `Cargo.lock` in `find_root`
    /// -> the root is this crate's own directory, which has no `assets/` ->
    /// fails.
    #[test]
    fn the_repo_root_holds_the_assets() {
        assert!(repo_root().join("Cargo.lock").is_file());
        assert!(asset("audio/stereo.flac").is_file());
    }

    /// A missing asset fails, naming the full path in the platform's own
    /// separators (the path is built component by component).
    ///
    /// Mutation: return the path without the `is_file` assert -> no panic ->
    /// fails; join `rel` as one component -> on Windows the message has mixed
    /// separators and no longer equals the native path -> fails there.
    #[test]
    fn a_missing_asset_panics_with_its_path() {
        let native = repo_root()
            .join("assets")
            .join("audio")
            .join("no_such_file.wav");
        let panic = std::panic::catch_unwind(|| asset("audio/no_such_file.wav"))
            .expect_err("a missing asset must panic");
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .expect("a string panic");
        assert!(
            message.contains(&native.display().to_string()),
            "{message:?} does not name {}",
            native.display()
        );
    }

    /// Mutation: test `rel.starts_with("sound/")` in `asset` -> the hint is
    /// dropped -> fails.
    #[test]
    #[should_panic(expected = "download-timgm6mb.sh")]
    fn a_missing_soundfont_names_how_to_fetch_it() {
        asset("soundfonts/no_such_font.sf2");
    }

    /// Mutation: swap the two entries of `candidates_in` -> fails.
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

    /// Mutation: spell the Linux name `lib{lib_stem}.bin` -> fails.
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
