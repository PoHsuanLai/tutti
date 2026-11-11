//! Where a normalized output's audio waits for its gain (doc 019 section 7.5).
//!
//! A gain needs the whole signal measured, so a normalizing output keeps what
//! the graph rendered for it and is read back twice (measured, then written)
//! without rendering again. The audio is held in memory while the spills
//! together stay inside a budget, and past it a spill moves to a temporary
//! file of interleaved little-endian `f32` beside its target, which is
//! removed when the spill is dropped.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use tutti_types::{ChannelLayout, InterleavedMut, Samples};

use crate::render::Replay;

/// The most the spills of one render hold in memory before they move to
/// files: 512 MiB (doc 019, question 3).
pub(crate) const SPILL_MEMORY_BYTES: usize = 512 * 1024 * 1024;

/// The memory the spills of one render may hold between them.
#[derive(Debug)]
pub(crate) struct SpillBudget {
    held: usize,
    limit: usize,
}

impl SpillBudget {
    pub(crate) fn new(limit_bytes: usize) -> Self {
        Self {
            held: 0,
            limit: limit_bytes,
        }
    }
}

enum Store {
    Memory(Vec<f32>),
    File {
        path: PathBuf,
        writer: Option<BufWriter<File>>,
    },
}

/// The interleaved audio of one output, at the render's rate and the output's
/// width, kept until it has been read back.
pub(crate) struct Spill {
    layout: ChannelLayout,
    /// Where a spill to a file goes: a name beside the target.
    beside: PathBuf,
    store: Store,
    frames: usize,
}

impl Spill {
    /// An empty spill for `layout`, which moves to a file beside `target`
    /// when the budget runs out.
    pub(crate) fn new(layout: ChannelLayout, target: &Path) -> Self {
        Self {
            layout,
            beside: target.to_path_buf(),
            store: Store::Memory(Vec::new()),
            frames: 0,
        }
    }

    /// Frames kept so far.
    pub(crate) fn frames(&self) -> Samples {
        Samples(self.frames)
    }

    /// Whether the audio is in a file.
    #[cfg(test)]
    pub(crate) fn is_file(&self) -> bool {
        matches!(self.store, Store::File { .. })
    }

    /// Keeps `block`, whole frames of this spill's layout.
    ///
    /// # Errors
    ///
    /// The file's I/O error, when the spill is (or has to move) there.
    pub(crate) fn push(&mut self, block: &[f32], budget: &mut SpillBudget) -> io::Result<()> {
        debug_assert_eq!(block.len() % usize::from(self.layout.count()), 0);
        let bytes = std::mem::size_of_val(block);
        if let Store::Memory(held) = &mut self.store {
            if budget.held + bytes <= budget.limit && held.try_reserve(block.len()).is_ok() {
                held.extend_from_slice(block);
                budget.held += bytes;
                self.frames += block.len() / usize::from(self.layout.count());
                return Ok(());
            }
            self.move_to_file(budget)?;
        }
        let Store::File { writer, .. } = &mut self.store else {
            unreachable!("moved to a file above");
        };
        let writer = writer.as_mut().expect("a spill is read only once finished");
        write_f32(writer, block)?;
        self.frames += block.len() / usize::from(self.layout.count());
        Ok(())
    }

    /// Writes what is in memory to a new file beside the target and keeps the
    /// rest there; gives the memory back to the budget.
    fn move_to_file(&mut self, budget: &mut SpillBudget) -> io::Result<()> {
        let Store::Memory(held) = &mut self.store else {
            return Ok(());
        };
        let path = unique_beside(&self.beside);
        let mut writer = BufWriter::new(File::create_new(&path)?);
        // The file exists from here: a failure drops `Store::File`, which
        // removes it.
        let held = std::mem::take(held);
        budget.held = budget.held.saturating_sub(std::mem::size_of_val(&held[..]));
        self.store = Store::File { path, writer: None };
        write_f32(&mut writer, &held)?;
        if let Store::File { writer: w, .. } = &mut self.store {
            *w = Some(writer);
        }
        Ok(())
    }

    /// Ends the writing: from here the spill is read back. The data written
    /// is flushed.
    ///
    /// # Errors
    ///
    /// The file's I/O error.
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if let Store::File {
            writer: Some(writer),
            ..
        } = &mut self.store
        {
            writer.flush()?;
        }
        Ok(())
    }

    /// The audio from the start, once [`finish`](Self::finish)ed. May be
    /// called again for another pass.
    ///
    /// # Errors
    ///
    /// The file's I/O error (it could not be opened).
    pub(crate) fn replay(&self) -> io::Result<SpillReplay<'_>> {
        Ok(match &self.store {
            Store::Memory(held) => SpillReplay::Memory { held, at: 0 },
            Store::File { path, .. } => SpillReplay::File {
                reader: BufReader::new(File::open(path)?),
                bytes: Vec::new(),
            },
        })
    }
}

impl Drop for Spill {
    fn drop(&mut self) {
        if let Store::File { path, writer } = &mut self.store {
            // Closed before it is removed (Windows will not remove an open file).
            drop(writer.take());
            if let Err(e) = std::fs::remove_file(&*path)
                && e.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(
                    "export: could not remove the spill file {}: {e}",
                    path.display()
                );
            }
        }
    }
}

/// A name beside `target` no other spill (or process) uses.
fn unique_beside(target: &Path) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = target
        .file_name()
        .map_or_else(|| "export".to_owned(), |n| n.to_string_lossy().into_owned());
    target.with_file_name(format!(
        ".{name}.{}.{}.spill",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn write_f32(writer: &mut impl Write, samples: &[f32]) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(samples.len().min(1 << 16) * 4);
    for chunk in samples.chunks(1 << 16) {
        bytes.clear();
        bytes.extend(chunk.iter().flat_map(|s| s.to_le_bytes()));
        writer.write_all(&bytes)?;
    }
    Ok(())
}

/// A spill read from its start, a block at a time.
pub(crate) enum SpillReplay<'a> {
    Memory {
        held: &'a [f32],
        at: usize,
    },
    File {
        reader: BufReader<File>,
        bytes: Vec<u8>,
    },
}

impl Replay for SpillReplay<'_> {
    fn fill(&mut self, mut out: InterleavedMut<'_>) -> crate::Result<usize> {
        let width = usize::from(out.layout().count());
        match self {
            Self::Memory { held, at } => {
                let n = (held.len() - *at).min(out.len() * width);
                let n = n - n % width;
                out.samples_mut()[..n].copy_from_slice(&held[*at..*at + n]);
                *at += n;
                Ok(n / width)
            }
            Self::File { reader, bytes } => {
                let want = out.len() * width;
                bytes.resize(want * 4, 0);
                let mut got = 0;
                while got < bytes.len() {
                    match reader.read(&mut bytes[got..])? {
                        0 => break,
                        n => got += n,
                    }
                }
                let samples = got / 4;
                let samples = samples - samples % width;
                for (dst, b) in out.samples_mut()[..samples]
                    .iter_mut()
                    .zip(bytes.as_chunks::<4>().0)
                {
                    *dst = f32::from_le_bytes(*b);
                }
                Ok(samples / width)
            }
        }
    }
}

#[cfg(test)]
mod tests;
