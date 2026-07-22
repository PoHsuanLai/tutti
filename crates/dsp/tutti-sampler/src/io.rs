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

/// Move one block of frames from an [`AudioIn`] to an [`AudioOut`]: poll up to
/// `buf.len()` frames from `src`, write exactly what it produced to `dst`,
/// return that count.
///
/// This is the whole of "recording", minus the loop and the stop condition —
/// both of which are the *caller's* policy, not this function's. A recorder
/// runs this on a background thread until its stop flag is set:
///
/// ```ignore
/// let mut buf = vec![(0.0, 0.0); 1024];   // caller owns the buffer — no alloc per pump
/// while running.load(Ordering::Relaxed) {
///     if pump(&mut mic, &mut wav, &mut buf) == 0 {
///         std::thread::yield_now();       // nothing ready — a live source may starve briefly
///     }
/// }
/// wav.finalize()?;                        // caller finalizes once, after the loop
/// ```
///
/// Generic, not `dyn`: the caller picks the concrete source and sink at the
/// call site, so both `poll_into` and `write` inline and the pump allocates
/// nothing (the buffer is caller-owned). Returning `0` means the source had
/// nothing this pass — the caller decides whether that's back-off (live source)
/// or end-of-stream (finite source).
pub fn pump<I: AudioIn + ?Sized, O: AudioOut + ?Sized>(
    src: &mut I,
    dst: &mut O,
    buf: &mut [(f32, f32)],
) -> usize {
    let n = src.poll_into(buf);
    dst.write(&buf[..n]);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finite in-memory [`AudioIn`]: hands out its frames in bounded chunks,
    /// returning a short-then-zero count at end-of-stream — the shape a decoded
    /// file has, and a stand-in for a live source in a test.
    struct SliceSource {
        frames: Vec<(f32, f32)>,
        pos: usize,
        /// Cap per poll, to exercise the "source produces fewer than asked" path.
        chunk: usize,
    }

    impl AudioIn for SliceSource {
        fn poll_into(&mut self, out: &mut [(f32, f32)]) -> usize {
            let remaining = self.frames.len() - self.pos;
            let n = remaining.min(out.len()).min(self.chunk);
            out[..n].copy_from_slice(&self.frames[self.pos..self.pos + n]);
            self.pos += n;
            n
        }
    }

    /// A sink that just tallies every frame it's handed — no I/O, so the pump
    /// contract (write exactly the polled count, never the untouched tail) can
    /// be asserted without touching disk.
    #[derive(Default)]
    struct CountingSink {
        written: Vec<(f32, f32)>,
    }

    impl AudioOut for CountingSink {
        fn write(&mut self, frames: &[(f32, f32)]) {
            self.written.extend_from_slice(frames);
        }
        fn finalize(self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Pumping a finite source to exhaustion moves every frame exactly once, in
    /// order, and never writes past the polled count even when the buffer is
    /// larger than what the source produces that pass.
    #[test]
    fn pump_drains_a_finite_source_exactly() {
        let frames: Vec<(f32, f32)> = (0..1000).map(|i| (i as f32, -(i as f32))).collect();
        let mut src = SliceSource {
            frames: frames.clone(),
            pos: 0,
            chunk: 37, // deliberately coprime with the buffer so chunks straddle
        };
        let mut dst = CountingSink::default();
        let mut buf = vec![(0.0, 0.0); 64];

        let mut total = 0;
        loop {
            let n = pump(&mut src, &mut dst, &mut buf);
            if n == 0 {
                break;
            }
            assert!(n <= buf.len(), "pump wrote more than the buffer holds");
            total += n;
        }

        assert_eq!(total, frames.len());
        assert_eq!(
            dst.written, frames,
            "frames must arrive intact and in order"
        );
    }
}
