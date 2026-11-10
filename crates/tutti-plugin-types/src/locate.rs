//! Where to find the host's helper executables, `plugin-server` and
//! `plugin-probe`.
//!
//! The search is a value, [`Locator`], rather than a bare function, because a
//! test must answer "which binary do we spawn?" differently from production
//! without writing process-global state. Reading only the environment made the
//! answer global: the one way to point a test at a stand-in was
//! `std::env::set_var`, which every other thread in the binary sees, so under a
//! parallel runner one test's stand-in was another's production lookup. A test
//! builds [`Locator::at`] and hands it in instead.

use std::path::{Path, PathBuf};

/// One of the host's helper executables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Helper {
    /// The out-of-process host (`plugin-server`, `TUTTI_PLUGIN_SERVER`).
    Server,
    /// The descriptor probe (`plugin-probe`, `TUTTI_PLUGIN_PROBE`).
    Probe,
}

impl Helper {
    /// The environment variable that names the binary outright.
    #[must_use]
    pub const fn env_var(self) -> &'static str {
        match self {
            Self::Server => "TUTTI_PLUGIN_SERVER",
            Self::Probe => "TUTTI_PLUGIN_PROBE",
        }
    }

    /// The executable's file name, without the platform's suffix.
    #[must_use]
    pub const fn file_stem(self) -> &'static str {
        match self {
            Self::Server => "plugin-server",
            Self::Probe => "plugin-probe",
        }
    }
}

/// Which binary to spawn for a [`Helper`].
///
/// [`Default`] is production: the environment variable, then beside the
/// running executable and its parent directory (an `examples/` binary), then
/// `PATH`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Locator {
    /// Search the environment, the executable's directory, and `PATH`.
    #[default]
    Search,
    /// Spawn exactly this binary, searching nowhere.
    ///
    /// A missing file is a miss rather than a fallback to the search: a caller
    /// that named a path meant that path, and silently spawning a different
    /// binary is how a test ends up exercising production code it thought it
    /// had replaced.
    Explicit(PathBuf),
}

impl Locator {
    /// Spawn `path` and nothing else.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self::Explicit(path.into())
    }

    /// The binary to spawn for `helper`, or `None` when there is none.
    #[must_use]
    pub fn resolve(&self, helper: Helper) -> Option<PathBuf> {
        match self {
            Self::Explicit(path) => path.is_file().then(|| path.clone()),
            Self::Search => search(helper),
        }
    }
}

/// The production search for `helper`.
fn search(helper: Helper) -> Option<PathBuf> {
    let name = format!("{}{}", helper.file_stem(), std::env::consts::EXE_SUFFIX);
    let env_var = helper.env_var();
    if let Some(p) = std::env::var_os(env_var) {
        let path = PathBuf::from(&p);
        if path.is_file() {
            return Some(path);
        }
        // Not a file: fall back to the search, as if it were unset.
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        if let Some(found) = beside(exe_dir, &name) {
            return Some(found);
        }
        if let Some(found) = exe_dir.parent().and_then(|dir| beside(dir, &name)) {
            return Some(found);
        }
    }

    std::env::var_os("PATH")
        .and_then(|path| std::env::split_paths(&path).find_map(|dir| beside(&dir, &name)))
}

/// `dir/name`, if it is a file. An empty `PATH` entry names no directory: it is
/// skipped, not read as the working directory.
fn beside(dir: &Path, name: &str) -> Option<PathBuf> {
    if dir.as_os_str().is_empty() {
        return None;
    }
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An explicit path is spawned verbatim, with no search behind it: if
    /// `Explicit` fell through to the search, a test's stand-in would be
    /// replaced by whatever `PATH` holds, and the test would exercise the real
    /// binary while claiming otherwise.
    ///
    /// Mutation (run): `Explicit` resolving through `search` fails the second
    /// half (the search finds nothing at that name, or finds something else).
    #[test]
    fn an_explicit_locator_is_that_path_or_nothing() {
        let this_exe = std::env::current_exe().expect("test binary has a path");
        for helper in [Helper::Server, Helper::Probe] {
            assert_eq!(
                Locator::at(&this_exe).resolve(helper),
                Some(this_exe.clone())
            );
            let missing = std::env::temp_dir().join("tutti-no-such-helper-XYZZY");
            assert_eq!(Locator::at(&missing).resolve(helper), None);
        }
        assert_eq!(Locator::default(), Locator::Search);
    }

    /// A directory with the helper's name is not the helper. (The search used
    /// `exists()`, so a directory resolved and the failure surfaced later as a
    /// confusing spawn error.)
    ///
    /// Mutation (run): `beside` checking `exists()` fails it.
    #[test]
    fn a_directory_is_not_a_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let name = format!("plugin-probe{}", std::env::consts::EXE_SUFFIX);
        std::fs::create_dir(dir.path().join(&name)).expect("mkdir");
        assert_eq!(beside(dir.path(), &name), None);
        assert_eq!(
            Locator::at(dir.path().join(&name)).resolve(Helper::Probe),
            None
        );
    }

    /// An empty `PATH` entry is skipped, not read as the working directory:
    /// `"".join(name)` is the relative `name`, which would spawn whatever file
    /// of that name sits in the host's current directory.
    ///
    /// Mutation (run): deleting the emptiness check makes `beside` return the
    /// relative path when the file is in the working directory, and fails.
    ///
    /// It changes the working directory, which is this process's alone: each
    /// test runs in a process of its own under nextest (CLAUDE.md).
    #[test]
    fn an_empty_path_entry_is_not_the_working_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let name = "tutti-helper-in-cwd";
        std::fs::write(dir.path().join(name), b"").expect("write");
        std::env::set_current_dir(dir.path()).expect("cd");
        assert_eq!(beside(Path::new(""), name), None);
    }
}
