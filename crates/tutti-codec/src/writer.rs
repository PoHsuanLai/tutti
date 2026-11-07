//! The contract every format's writer keeps.

use crate::error::Result;
use tutti_types::Interleaved;

/// A `FormatWriter` writes blocks of interleaved samples into its sink as one
/// file of its format.
///
/// The writer is made by its own `create`, which writes the header. It takes the
/// blocks it is given, at the rate and width its spec states: it does not pull,
/// resample or dither, and nothing here stops a caller from handing it blocks
/// that were not (the pipeline in front of it is the caller's, and
/// `tutti-export` has one path through its own). [`write`](Self::write) is
/// fallible, so a full disk in the middle of a take is an error at the block that
/// hit it; [`finish`](Self::finish) takes the writer by value, so a file is
/// closed exactly once, and it closes what was written even after a failed
/// `write`, handing the sink back. A writer dropped before `finish` leaves the
/// sink as it is (WAV also back-patches its sizes, best effort, so the take
/// stays readable) and stops any thread it started before `drop` returns.
///
/// The sink bounds belong to each implementation, not to the trait: a format
/// that needs more of its sink (Ogg Vorbis encodes on a thread of its own,
/// which takes the sink with it) asks for more on its own `impl`.
///
/// [`Encoder`](crate::Encoder) is the one type that holds any of them.
pub trait FormatWriter: Sized {
    /// The sink the file is written to, handed back by [`finish`](Self::finish).
    type Sink;

    /// Writes one block. An empty block writes nothing.
    ///
    /// # Errors
    ///
    /// The sink's failure, or the codec's refusal of the block. After an error
    /// the file is not a complete take.
    fn write(&mut self, block: Interleaved<'_>) -> Result<()>;

    /// Closes the file: back-patches its sizes and flushes the sink, which is
    /// returned positioned at the end of the file.
    ///
    /// # Errors
    ///
    /// The sink's failure, or the codec's.
    fn finish(self) -> Result<Self::Sink>;
}
