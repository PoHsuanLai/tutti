//! The sampler's two-trait I/O vocabulary: [`AudioIn`] (pull frames from a
//! source) and [`AudioOut`] (push frames to a destination). Everything the
//! sampler reads or writes speaks one of these two shapes.
//!
//! ```text
//!   AudioIn  ──poll_into──▶  [your buffer]  ──write──▶  AudioOut
//!   (mic, file, disk, …)                                (WAV file, …)
//! ```
//!
//! Recording is nothing more than pumping an [`AudioIn`] into an [`AudioOut`]:
//! poll a block from the source, write that block to the sink, repeat. Mic,
//! decoded file, disk stream, neural generator — all are just an `AudioIn`; a
//! WAV file, a network socket, another ring — all are just an `AudioOut`.
//!
//! # Deliberately minimal
//!
//! Two methods total. No rate/length/seek/format vocabulary lives here — those
//! are properties of a *particular* source or sink, not of "can be read from"
//! or "can be written to". A caller that needs a source's sample rate holds the
//! concrete type; the trait is only the frame-transfer contract.
//!
//! # Not the RT hot path
//!
//! Neither trait is invoked per-sample on the audio thread. They move frames in
//! *blocks* on a cold/background path (a capture pump, an offline render). The
//! per-sample graph read stays behind the monomorphized `ClipSource` enum and
//! must remain alloc-free / lock-free; these block interfaces do not touch it.

/// A pull source of stereo audio frames. The one method fills a caller-owned
/// buffer and reports how many frames it actually produced.
///
/// # Why a returned count
///
/// A *live* source (a microphone, a socket) may have fewer frames ready than
/// the buffer asks for — it returns what it has. A *finite* source (a decoded
/// file) returns a short count at end-of-stream, then `0`. The caller owns the
/// buffer, so polling never allocates; the count tells the caller how much of
/// `out` was written this call.
pub trait AudioIn {
    /// Fill the front of `out` with the next available stereo frames and return
    /// the number written (`0..=out.len()`). Frames past the returned count are
    /// left untouched. `0` means "nothing available right now" for a live
    /// source, or end-of-stream for a finite one.
    fn poll_into(&mut self, out: &mut [(f32, f32)]) -> usize;
}

/// A push destination for stereo audio frames: write blocks incrementally, then
/// close once.
///
/// # Why `finalize` consumes `self`
///
/// A destination may need a final commit that can fail and must happen exactly
/// once (a WAV sink back-patches its header; a socket flushes and closes).
/// Taking `self` by value makes "you cannot write after finalizing" a
/// compile-time guarantee and gives the commit a place to surface I/O errors.
pub trait AudioOut {
    /// Append `frames` to the destination. Called repeatedly as data arrives;
    /// implementations write incrementally and never buffer the whole stream.
    fn write(&mut self, frames: &[(f32, f32)]);

    /// Close the destination, flushing and committing. For a file sink this is
    /// where the header is back-patched, so a failure here can mean an
    /// unreadable file — surface it rather than swallowing it.
    fn finalize(self) -> std::io::Result<()>;
}
