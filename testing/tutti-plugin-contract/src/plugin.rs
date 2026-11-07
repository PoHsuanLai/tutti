//! Where the shared plugin is, per format. It is one cdylib; a format whose
//! host dispatches on the extension gets a link to it under that extension.

use std::path::PathBuf;

/// The library, as cargo built it.
#[must_use]
pub fn library() -> PathBuf {
    tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin")
}

/// The library published as `.clap`.
#[must_use]
pub fn clap() -> PathBuf {
    tutti_fixtures::publish_with_extension(&library(), "clap")
}
