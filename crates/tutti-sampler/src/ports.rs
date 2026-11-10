//! The playback tier choice ([`Source`]) and the READ port onto the streaming
//! engine ([`Status`]).
//!
//! The [`DiskStreamer`](crate::DiskStreamer) handle owns the streamer thread. A
//! stream is opened with [`DiskStreamer::open`](crate::DiskStreamer::open),
//! whose reply is the stream's control-side [`Stream`](crate::Stream) and its
//! [`StreamReader`](crate::StreamReader); a stream is steered through its
//! `Stream`, and there is no other command surface.

use std::path::PathBuf;
use std::sync::Arc;

use crate::streamer::SessionRate;
use tutti_codec::Wave;
use tutti_types::SampleRate;

/// The caller's stated choice of playback tier for a voice: whole-file in memory
/// (`Memory`) or incremental disk streaming (`Disk`).
///
/// Plain data. The sampler never decides the tier on its own; it plays whichever
/// variant it is handed, and the caller owns the decision — a host's tiering
/// policy, say. [`probe`](fn@crate::probe) reports what a file *allows* —
/// a `streamable: false` file has no `Disk` option at all.
///
/// An enum rather than a trait over the two tiers, for the reason the runtime
/// side documents on `VoiceSource`: the tiers differ only in the essential
/// per-sample read, and a trait makes every cold control operation
/// tier-conditional in its *meaning* while looking uniform at the call site.
#[derive(Clone)]
pub enum Source {
    /// Whole file already decoded into memory, played by a `MemorySource`.
    ///
    /// The `Wave` is shared by `Arc`, so several voices over one sample cost one
    /// copy of the audio.
    Memory(Arc<Wave>),
    /// File streamed incrementally from disk by the streamer thread.
    ///
    /// The path is opened on the streamer thread, not here, so constructing this
    /// variant does no I/O and cannot fail.
    Disk(PathBuf),
}

// `Wave` doesn't implement `Debug`, so hand-roll it (len/rate summary for
// `Memory`, path for `Disk`) rather than deriving.
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Memory(wave) => f
                .debug_struct("Memory")
                .field("len", &wave.len())
                .field("sample_rate", &wave.sample_rate())
                .finish(),
            Source::Disk(path) => f.debug_tuple("Disk").field(path).finish(),
        }
    }
}

/// READ port onto the streaming engine: a cloneable snapshot of the session
/// rate.
#[derive(Clone)]
pub struct Status {
    sample_rate: SessionRate,
}

impl std::fmt::Debug for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Status")
            .field("sample_rate", &self.sample_rate.get())
            .finish_non_exhaustive()
    }
}

impl Status {
    pub(crate) fn new(sample_rate: SessionRate) -> Self {
        Self { sample_rate }
    }

    /// The session rate: the one the streamer was built with, or the one
    /// [`DiskStreamer::set_sample_rate`](crate::DiskStreamer::set_sample_rate)
    /// last moved it to. Read live, so a `Status` taken before a device
    /// restart reports the rate after it.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate.get()
    }
}
