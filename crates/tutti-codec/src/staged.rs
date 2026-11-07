//! A file being written beside its destination.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A temporary beside `target`: removed when dropped unless
/// [`commit`](Self::commit)ted. A leftover that cannot be removed is logged,
/// not an error: the take's outcome is already decided.
pub(crate) struct Staged {
    temp: PathBuf,
    target: PathBuf,
    kept: bool,
}

impl Staged {
    pub(crate) fn beside(target: &Path) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let name = target
            .file_name()
            .map_or_else(|| "export".to_owned(), |n| n.to_string_lossy().into_owned());
        let temp = target.with_file_name(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Self {
            temp,
            target: target.to_path_buf(),
            kept: false,
        }
    }

    pub(crate) fn temp(&self) -> &Path {
        &self.temp
    }

    /// Renames the temporary over the target.
    pub(crate) fn commit(mut self) -> std::io::Result<()> {
        std::fs::rename(&self.temp, &self.target)?;
        self.kept = true;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        match std::fs::remove_file(&self.temp) {
            Ok(()) => {}
            // Never created: the encoder failed before it opened the file.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                "codec: could not remove the unfinished file {}: {e}",
                self.temp.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests;
