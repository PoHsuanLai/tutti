//! The engine's two-trait I/O vocabulary: [`AudioIn`] (pull frames from a
//! source) and [`AudioOut`] (push frames to a destination). Every audio source
//! and sink in tutti — mic, decoded file, disk stream, plugin boundary, WAV
//! encoder — speaks one of these two shapes.
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
//! Homed in `tutti-types`, the root leaf, so every subsystem (sampler, export,
//! analysis, the plugin hosts) can implement these without depending on any
//! particular engine crate.
//!
//! # The frame: `[S; CH]`, generic in element and channel count
//!
//! A frame is an array of `CH` samples of element type `S` — interleaved. This
//! is the foundational shape, so it carries **both** axes of variation any real
//! I/O needs:
//!
//! - **`S`** — the sample element. `f32` (default) for the whole edge world;
//!   `f64` for a 64-bit plugin bus or high-precision render.
//! - **`CH`** — channels per frame. `2` (default) is stereo; `1` is mono; a
//!   surround plugin bus is `6` or `8`. Const-generic, so the width is part of
//!   the type and the compiler monomorphizes each one.
//!
//! Both default to `<f32, 2>`, so a plain stereo source is just `AudioIn` and a
//! plain stereo sink is just `AudioOut` — the generality costs nothing at the
//! common edge. `[f32; 2]` is `Copy`, lives in a lock-free ring unchanged, and
//! carries no channel-order policy (that's the concrete backend's business).
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
//! per-sample graph read stays behind the monomorphized clip-source enum and
//! must remain alloc-free / lock-free; these block interfaces do not touch it.

/// What a 0-frame [`poll_into`](AudioIn::poll_into) means for a given source.
///
/// A zero count is ambiguous on its own — "the producer has not caught up" and
/// "there will never be more" are the same return value — and **only the source
/// knows which it is**. A microphone is live; a decoded file is finite. That is
/// a property of the type, so the type states it once rather than every caller
/// restating it per call and being able to get it wrong.
///
/// The consequence of guessing is silent: treating a mic as finite ends a
/// recording at the first empty ring, milliseconds in, producing a near-empty
/// file with no error anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnEmpty {
    /// Live source (microphone, socket): the producer has not caught up. A
    /// consumer that loops should back off and poll again.
    Starved,
    /// Finite source (decoded file, rendered net): there is no more. A consumer
    /// that loops should stop.
    EndOfStream,
}

/// A pull source of audio frames. The one method fills a caller-owned buffer of
/// `[S; CH]` frames and reports how many it actually produced.
///
/// Generic over the sample element `S` (default `f32`) and channel count `CH`
/// (default `2` = stereo). A mono source is `AudioIn<f32, 1>`; a 64-bit
/// six-channel plugin output is `AudioIn<f64, 6>`. See the [module docs](self)
/// for the frame model.
///
/// # Why a returned count
///
/// A *live* source (a microphone, a socket) may have fewer frames ready than
/// the buffer asks for — it returns what it has. A *finite* source (a decoded
/// file) returns a short count at end-of-stream, then `0`. The caller owns the
/// buffer, so polling never allocates; the count tells the caller how much of
/// `out` was written this call.
///
/// # Why the count alone is not enough
///
/// `0` is the same value in both cases, so a consumer that *loops* — a capture
/// pump, a refill — cannot tell "not yet" from "never again" without knowing
/// what kind of source it holds. [`ON_EMPTY`](Self::ON_EMPTY) is that knowledge,
/// carried on the type. It is an associated const rather than a method because
/// it is fixed per source and callable from a generic context without a value;
/// `AudioIn` is never used as `dyn` anywhere, so this costs no object safety.
pub trait AudioIn<S = f32, const CH: usize = 2> {
    /// What a 0-frame poll means for *this* source. See [`OnEmpty`].
    const ON_EMPTY: OnEmpty;

    /// Fill the front of `out` with the next available frames and return the
    /// number written (`0..=out.len()`). Frames past the returned count are
    /// left untouched. `0` means "nothing available right now" for a live
    /// source, or end-of-stream for a finite one — which of the two is
    /// [`ON_EMPTY`](Self::ON_EMPTY).
    fn poll_into(&mut self, out: &mut [[S; CH]]) -> usize;
}

/// A push destination for audio frames: write blocks of `[S; CH]` incrementally,
/// then close once.
///
/// Generic over the sample element `S` (default `f32`) and channel count `CH`
/// (default `2` = stereo), matching [`AudioIn`]. A WAV file sink is
/// `AudioOut<f32, 2>`; a 64-bit multichannel encoder is `AudioOut<f64, N>`.
///
/// # Why `finalize` consumes `self`
///
/// A destination may need a final commit that can fail and must happen exactly
/// once (a WAV sink back-patches its header; a socket flushes and closes).
/// Taking `self` by value makes "you cannot write after finalizing" a
/// compile-time guarantee and gives the commit a place to surface I/O errors.
pub trait AudioOut<S = f32, const CH: usize = 2> {
    /// Append `frames` to the destination. Called repeatedly as data arrives;
    /// implementations write incrementally and never buffer the whole stream.
    fn write(&mut self, frames: &[[S; CH]]);

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
/// let mut buf = [[0.0f32; 2]; 1024];      // caller owns the buffer — no alloc per pump
/// while running.load(Ordering::Relaxed) {
///     if pump(&mut mic, &mut wav, &mut buf) == 0 {
///         std::thread::yield_now();       // nothing ready — a live source may starve briefly
///     }
/// }
/// wav.finalize()?;                        // caller finalizes once, after the loop
/// ```
///
/// Generic over the frame `[S; CH]` and, not `dyn`, over the concrete source
/// and sink: the caller picks both at the call site, so `poll_into` and `write`
/// inline and the pump allocates nothing (the buffer is caller-owned). The
/// source, sink, and buffer must agree on `S` and `CH` — a stereo mic can't
/// pump into a 6-channel sink, and the type system enforces it. Returning `0`
/// means the source had nothing this pass — the caller decides whether that's
/// back-off (live source) or end-of-stream (finite source).
pub fn pump<S, const CH: usize, I, O>(src: &mut I, dst: &mut O, buf: &mut [[S; CH]]) -> usize
where
    I: AudioIn<S, CH> + ?Sized,
    O: AudioOut<S, CH> + ?Sized,
{
    let n = src.poll_into(buf);
    dst.write(&buf[..n]);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finite in-memory [`AudioIn`], generic over element `S` and channel
    /// count `CH`: hands out its frames in bounded chunks, returning a
    /// short-then-zero count at end-of-stream — the shape a decoded file has,
    /// and a stand-in for a live source in a test. Generic so the same fixture
    /// exercises the default `<f32, 2>` and a non-default width.
    struct SliceSource<S, const CH: usize> {
        frames: Vec<[S; CH]>,
        pos: usize,
        /// Cap per poll, to exercise the "source produces fewer than asked" path.
        chunk: usize,
    }

    impl<S: Copy, const CH: usize> AudioIn<S, CH> for SliceSource<S, CH> {
        const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

        fn poll_into(&mut self, out: &mut [[S; CH]]) -> usize {
            let remaining = self.frames.len() - self.pos;
            let n = remaining.min(out.len()).min(self.chunk);
            out[..n].copy_from_slice(&self.frames[self.pos..self.pos + n]);
            self.pos += n;
            n
        }
    }

    /// A sink that just tallies every frame it's handed — no I/O, so the pump
    /// contract (write exactly the polled count, never the untouched tail) can
    /// be asserted without touching disk. Generic to match [`SliceSource`].
    struct CountingSink<S, const CH: usize> {
        written: Vec<[S; CH]>,
    }

    impl<S, const CH: usize> Default for CountingSink<S, CH> {
        fn default() -> Self {
            Self {
                written: Vec::new(),
            }
        }
    }

    impl<S: Copy, const CH: usize> AudioOut<S, CH> for CountingSink<S, CH> {
        fn write(&mut self, frames: &[[S; CH]]) {
            self.written.extend_from_slice(frames);
        }
        fn finalize(self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Pumping a finite source to exhaustion moves every frame exactly once, in
    /// order, and never writes past the polled count even when the buffer is
    /// larger than what the source produces that pass. Uses the default stereo
    /// `f32` frame.
    #[test]
    fn pump_drains_a_finite_source_exactly() {
        let frames: Vec<[f32; 2]> = (0..1000).map(|i| [i as f32, -(i as f32)]).collect();
        let mut src = SliceSource {
            frames: frames.clone(),
            pos: 0,
            chunk: 37, // deliberately coprime with the buffer so chunks straddle
        };
        let mut dst = CountingSink::default();
        let mut buf = vec![[0.0f32; 2]; 64];

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

    /// The same pump over a non-default frame — `f64`, six channels — the shape
    /// a 64-bit surround plugin bus wants. Proves the const-generic width and
    /// the element type actually thread through `pump`, not just the stereo-f32
    /// default that would compile even if the generics were vestigial.
    #[test]
    fn pump_carries_a_64bit_six_channel_frame() {
        let frames: Vec<[f64; 6]> = (0..500)
            .map(|i| std::array::from_fn(|ch| (i * 6 + ch) as f64))
            .collect();
        let mut src = SliceSource {
            frames: frames.clone(),
            pos: 0,
            chunk: 41,
        };
        let mut dst: CountingSink<f64, 6> = CountingSink::default();
        let mut buf = vec![[0.0f64; 6]; 64];

        while pump(&mut src, &mut dst, &mut buf) != 0 {}

        assert_eq!(
            dst.written, frames,
            "wide frames must survive the pump intact"
        );
    }

    /// A live source stands in for a mic: it withholds frames (returning `0`)
    /// without being exhausted, then yields them later.
    ///
    /// This is the shape that makes [`OnEmpty`] load-bearing — a consumer that
    /// treats its `0` as end-of-stream stops with frames still to come.
    struct StarvingSource {
        frames: Vec<[f32; 2]>,
        pos: usize,
        /// Poll counter; odd polls return nothing, mimicking a ring the
        /// producer has not refilled yet.
        polls: usize,
    }

    impl AudioIn for StarvingSource {
        const ON_EMPTY: OnEmpty = OnEmpty::Starved;

        fn poll_into(&mut self, out: &mut [[f32; 2]]) -> usize {
            self.polls += 1;
            if self.polls % 2 == 1 {
                return 0; // "nothing ready yet" — but not finished
            }
            let n = (self.frames.len() - self.pos).min(out.len()).min(4);
            out[..n].copy_from_slice(&self.frames[self.pos..self.pos + n]);
            self.pos += n;
            n
        }
    }

    /// A generic drain loop that branches on `I::ON_EMPTY` — the exact shape a
    /// consumer needs, and the reason the const exists rather than a method.
    ///
    /// Reading the const off a type parameter is only possible because
    /// `AudioIn` is never a trait object; this function is the compile-time
    /// proof of that, and it would not build if the const were on a `dyn`-safe
    /// path.
    fn drain<I: AudioIn>(src: &mut I, dst: &mut CountingSink<f32, 2>, max_polls: usize) {
        let mut buf = [[0.0f32; 2]; 16];
        for _ in 0..max_polls {
            if pump(src, dst, &mut buf) == 0 {
                match I::ON_EMPTY {
                    // A live source has more coming — keep polling.
                    OnEmpty::Starved => continue,
                    // A finite one does not.
                    OnEmpty::EndOfStream => break,
                }
            }
        }
    }

    /// The same generic loop reaches every frame of a starving source but stops
    /// promptly on a finite one — driven entirely by `ON_EMPTY`.
    ///
    /// Both halves matter. Without the `Starved` arm the live source would be
    /// cut off at its first empty poll (4 of 40 frames, the bug this const
    /// prevents); without `EndOfStream` the finite source would spin to the
    /// poll ceiling instead of finishing.
    #[test]
    fn a_generic_consumer_branches_on_the_sources_own_verdict() {
        let frames: Vec<[f32; 2]> = (0..40).map(|i| [i as f32, -(i as f32)]).collect();

        let mut live = StarvingSource {
            frames: frames.clone(),
            pos: 0,
            polls: 0,
        };
        let mut live_sink: CountingSink<f32, 2> = CountingSink::default();
        drain(&mut live, &mut live_sink, 100);
        assert_eq!(
            live_sink.written, frames,
            "a starving source must be drained past its empty polls, not truncated at the first"
        );

        let mut finite = SliceSource {
            frames: frames.clone(),
            pos: 0,
            chunk: 7,
        };
        let mut finite_sink: CountingSink<f32, 2> = CountingSink::default();
        drain(&mut finite, &mut finite_sink, 100);
        assert_eq!(
            finite_sink.written, frames,
            "and a finite source must still deliver everything before stopping"
        );
        assert_eq!(
            finite.pos,
            frames.len(),
            "the finite source is exhausted, so the loop ended by verdict not by ceiling"
        );
    }
}
