//! Locate the `plugin-server` and `plugin-probe` binaries.
//!
//! Production search order: `TUTTI_PLUGIN_SERVER` env → next to current exe →
//! parent dir (for `examples/` subdir) → `PATH`.
//!
//! That search is wrapped in a `ServerLocator` value rather than performed by
//! a bare function, because "which binary do we spawn?" is a decision two call
//! sites make ([`launch`](fn@super::launch) and
//! [`probe_metadata_with`](super::probe_metadata_with)) and a test needs to answer
//! differently. Reading the environment inside the search made the answer
//! **process-global**: the only way to point a test at a stand-in server was
//! `std::env::set_var`, which every other thread in the binary sees. Under a
//! parallel runner that is a data race on the answer — one test's stand-in path
//! is another test's production lookup — and it is not fixable by a
//! save/restore guard, because the window between set and restore is exactly
//! when the other threads run.
//!
//! So the env var is read in exactly one place, [`ServerLocator::from_env`],
//! which the production entry points call for their default. A test constructs
//! [`ServerLocator::at`] instead and hands it in, touching no global state.

use crate::error::{BridgeError, Result};
use std::path::{Path, PathBuf};

/// Which `plugin-server` binary to spawn.
///
/// [`Default`] is the production behaviour — the full environment-and-`PATH`
/// search — so `..Default::default()` and `ServerLocator::default()` keep every
/// existing caller's meaning.
///
/// Only tests construct the non-default variant today, hence the `dead_code`
/// allowance. It stays in the shipped enum rather than being `#[cfg(test)]`-gated
/// so that both of [`resolve`](Self::resolve)'s arms are compiled and
/// type-checked in every build — a test-only arm is one that stops compiling
/// without anyone noticing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum ServerLocator {
    /// Search the environment, the executable's directory, and `PATH`.
    #[default]
    Search,
    /// Spawn exactly this binary, searching nowhere.
    ///
    /// A missing file is an error rather than a fallback to the search: a
    /// caller that named a path meant that path, and silently spawning a
    /// different server is how a test ends up exercising production code it
    /// thought it had replaced.
    Explicit(PathBuf),
}

impl ServerLocator {
    /// The production default: read `TUTTI_PLUGIN_SERVER`, else search.
    ///
    /// Named rather than implicit so the one site that reads process-global
    /// state is greppable.
    pub fn from_env() -> Self {
        Self::Search
    }

    /// Spawn `path` and nothing else. See the type docs for why this is
    /// test-only for now.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self::Explicit(path.into())
    }

    /// Resolve to a binary, or explain why none was found.
    pub(super) fn resolve(&self) -> Result<PathBuf> {
        match self {
            Self::Explicit(path) => {
                if path.is_file() {
                    Ok(path.clone())
                } else {
                    Err(BridgeError::ServerNotFound)
                }
            }
            Self::Search => {
                search("TUTTI_PLUGIN_SERVER", "plugin-server").ok_or(BridgeError::ServerNotFound)
            }
        }
    }
}

/// Which `plugin-probe` binary to spawn: the same choice `ServerLocator` makes
/// for the server, with its own binary and environment variable
/// (`TUTTI_PLUGIN_PROBE`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum ProbeLocator {
    /// Search the environment, the executable's directory, and `PATH`.
    #[default]
    Search,
    /// Spawn exactly this binary, searching nowhere; a missing file is an
    /// error, as for `ServerLocator::Explicit`.
    Explicit(PathBuf),
}

impl ProbeLocator {
    /// Spawn `path` and nothing else.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self::Explicit(path.into())
    }

    /// Resolve to a binary, or explain why none was found.
    pub(super) fn resolve(&self) -> Result<PathBuf> {
        match self {
            Self::Explicit(path) if path.is_file() => Ok(path.clone()),
            Self::Explicit(_) => Err(BridgeError::ProbeNotFound),
            Self::Search => {
                search("TUTTI_PLUGIN_PROBE", "plugin-probe").ok_or(BridgeError::ProbeNotFound)
            }
        }
    }
}

/// The environment-and-`PATH` search for the binary `name`, which `env_var` may
/// name outright.
fn search(env_var: &str, name: &str) -> Option<PathBuf> {
    let name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    if let Ok(p) = std::env::var(env_var) {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Some(path);
        }
        tracing::warn!("{env_var}={p} does not exist, falling back to search");
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

    if let Ok(path_var) = std::env::var("PATH") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for dir in path_var.split(sep) {
            if let Some(found) = beside(Path::new(dir), &name) {
                return Some(found);
            }
        }
    }

    None
}

/// `dir/name`, if it is there.
fn beside(dir: &Path, name: &str) -> Option<PathBuf> {
    let candidate = dir.join(name);
    candidate.exists().then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An explicit path is spawned verbatim, with no search behind it.
    ///
    /// This is the property a test relies on when it substitutes a stand-in
    /// server: if `Explicit` fell through to the search, the stand-in would be
    /// silently replaced by whatever `PATH` happens to hold, and the test would
    /// be exercising the real server while claiming otherwise.
    #[test]
    fn an_explicit_locator_resolves_to_exactly_that_path() {
        let this_exe = std::env::current_exe().expect("test binary has a path");
        assert_eq!(
            ServerLocator::at(&this_exe).resolve().unwrap(),
            this_exe,
            "an explicit locator must resolve to its own path"
        );
    }

    /// An explicit path that does not exist is an error, not a fallback.
    #[test]
    fn an_explicit_locator_pointing_nowhere_does_not_fall_back_to_the_search() {
        let missing = std::env::temp_dir().join("tutti-no-such-plugin-server-XYZZY");
        assert!(
            matches!(
                ServerLocator::at(&missing).resolve(),
                Err(BridgeError::ServerNotFound)
            ),
            "a named-but-absent server must fail rather than silently spawn a \
             different binary found on PATH"
        );
    }

    /// The production default stays the search — the whole point of keeping the
    /// env var readable is that shipping behaviour is unchanged.
    #[test]
    fn the_default_locator_is_the_search() {
        assert_eq!(ServerLocator::default(), ServerLocator::Search);
        assert_eq!(ServerLocator::from_env(), ServerLocator::Search);
    }

    /// An explicit probe path is spawned verbatim, and a missing one is
    /// `ProbeNotFound` rather than a fall back to the search: the probe's own
    /// counterpart of the server locator's pair above.
    ///
    /// Mutation (run): `Explicit` resolving through `search` fails both halves.
    #[test]
    fn an_explicit_probe_locator_is_that_path_or_not_found() {
        let this_exe = std::env::current_exe().expect("test binary has a path");
        assert_eq!(ProbeLocator::at(&this_exe).resolve().unwrap(), this_exe);
        let missing = std::env::temp_dir().join("tutti-no-such-plugin-probe-XYZZY");
        assert!(matches!(
            ProbeLocator::at(&missing).resolve(),
            Err(BridgeError::ProbeNotFound)
        ));
        assert_eq!(ProbeLocator::default(), ProbeLocator::Search);
    }
}
