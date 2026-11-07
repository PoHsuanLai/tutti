//! The contract every format's writer keeps.

use crate::error::Result;
use tutti_types::Interleaved;

/// A `FormatWriter` writes blocks of interleaved samples into its sink as one
/// file of its format.
///
/// The writer is made by its own `create`, which writes the header, and the
/// pipeline in front of it owns pulling, resampling and dithering: a writer
/// takes the blocks it is given, at the rate and width its spec states.
/// [`write`](Self::write) is fallible, so a full disk in the middle of a take
/// is an error at the block that hit it; [`finish`](Self::finish) takes the
/// writer by value, so a file is closed exactly once. A writer dropped before
/// `finish` leaves the sink as it is and stops any thread it started.
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
    /// the file is not a complete take; `finish` still closes what was written.
    fn write(&mut self, block: Interleaved<'_>) -> Result<()>;

    /// Closes the file: back-patches its sizes and flushes the sink, which is
    /// returned positioned at the end of the file.
    ///
    /// # Errors
    ///
    /// The sink's failure, or the codec's.
    fn finish(self) -> Result<Self::Sink>;

    /// Lowers the number of frames the file may hold to `frames`; it never
    /// raises a limit the format sets. Only WAV, whose sizes are 32-bit, has
    /// one, so the default does nothing.
    fn limit_frames(&mut self, _frames: u64) {}
}
