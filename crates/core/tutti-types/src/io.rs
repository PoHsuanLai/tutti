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
//! # The buffer: a flat interleaved `&[S]`, width from [`ChannelLayout`]
//!
//! Both methods take a **flat interleaved slice** of the sample element `S`
//! (`f32` by default; `f64` for a 64-bit plugin bus or a high-precision
//! render). The channel count is **not** in the type — it is a runtime
//! [`ChannelLayout`] the source or sink reports from [`AudioIn::layout`] /
//! [`AudioOut::layout`].
//!
//! That is deliberate. A const width can only carry a width that is a property
//! of the *code* — a stereo device, a stereo ring. It cannot carry one that is a
//! property of the *data*, which is what a decoded file, a streamed region, or
//! the device a user just plugged in actually has. A runtime width is what lets
//! a data-determined source be expressed here at all.
//!
//! # Counts are in FRAMES. Always. Everywhere.
//!
//! **This is the entire risk surface of the flat-slice form, so it is stated
//! once, loudly, and never relaxed:**
//!
//! - [`poll_into`](AudioIn::poll_into) **returns a frame count**, never a sample
//!   count. The samples it wrote are `returned * layout().count()`.
//! - [`write`](AudioOut::write) is handed `frames.len() / layout().count()`
//!   frames. A trailing partial frame is not a frame.
//! - [`pump`] returns frames. Its `buf` is *sized* in samples only because a
//!   flat slice has no other unit — a caller writes `frames * ch` and gets
//!   frames back.
//!
//! Exposing samples anywhere on this boundary is not a cosmetic slip. A
//! consumer compares a returned count against a loop range, a region length, or
//! a file position — every one of which is denominated in frames. Return
//! samples and a 6-channel looped clip wraps at one sixth of its length and
//! presents as "the loop points are wrong", sending the next reader off to
//! debug the loop config rather than this boundary.
//!
//! # Width agreement is a runtime check
//!
//! With a runtime width, "a stereo source cannot feed a 6-channel sink" is not a
//! type error. Two runtime checks carry it instead, and both are load-bearing:
//! [`pump`]'s `debug_assert_eq!` and `tutti_io::Recorder::start`'s returned
//! error. See [`pump`]'s docs for why there are two.
//!
//! # Deliberately minimal
//!
//! Three items total: one method per direction, plus the width. No
//! rate/length/seek/format vocabulary lives here — those are properties of a
//! *particular* source or sink, not of "can be read from" or "can be written
//! to". A caller that needs a source's sample rate holds the concrete type; the
//! trait is only the frame-transfer contract.
//!
//! # Not the RT hot path
//!
//! Neither trait is invoked per-sample on the audio thread. They move frames in
//! *blocks* on a cold/background path (a capture pump, an offline render). The
//! per-sample graph read stays behind the monomorphized clip-source enum and
//! must remain alloc-free / lock-free; these block interfaces do not touch it.

use crate::ChannelLayout;

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
///
/// Orthogonal to width: this says nothing about how many channels a frame has,
/// which is why it survived the move to a runtime [`ChannelLayout`] unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnEmpty {
    /// Live source (microphone, socket): the producer has not caught up. A
    /// consumer that loops should back off and poll again.
    Starved,
    /// Finite source (decoded file, rendered net): there is no more. A consumer
    /// that loops should stop.
    ///
    /// **This is a promise about the *first* zero, not an eventual one.** A
    /// looping consumer stops on it immediately, so a source that can return `0`
    /// and then more later — a decoder awaiting a refill, a file read over a
    /// socket — must not declare `EndOfStream`: it would end the read mid-stream
    /// with no error. Such a source is `Starved` (the consumer retries), and it
    /// signals completion by some means of its own. The two implementors here
    /// satisfy the promise: `FileIn` folds even a decode error into end-of-file,
    /// and a render only returns `0` for a zero-length request.
    EndOfStream,
}

/// A pull source of audio frames. Fills a caller-owned **flat interleaved**
/// buffer and reports how many **frames** it produced.
///
/// Generic over the sample element `S` (default `f32`). The channel count is
/// runtime, reported by [`layout`](Self::layout) — see the [module docs](self)
/// for why it is not a const parameter, and for the frames-not-samples rule
/// that governs every count on this boundary.
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
pub trait AudioIn<S = f32> {
    /// What a 0-frame poll means for *this* source. See [`OnEmpty`].
    const ON_EMPTY: OnEmpty;

    /// How many channels one frame of this source carries.
    ///
    /// Fixed for the life of the source: a caller sizes its buffer from this
    /// once, before the loop, and `poll_into` never changes it underneath.
    fn layout(&self) -> ChannelLayout;

    /// Fill the front of `out` with the next available frames and return the
    /// number of **FRAMES** written — not samples.
    ///
    /// `out` is flat interleaved at [`layout`](Self::layout)'s width, so it
    /// holds `out.len() / layout().count()` frames and the return is bounded by
    /// that. Samples past `returned * count` are left untouched, and a trailing
    /// partial frame (an `out` whose length is not a whole multiple of the
    /// width) is never partly filled — a short frame desynchronises the
    /// interleave for everything after it.
    ///
    /// `0` means "nothing available right now" for a live source, or
    /// end-of-stream for a finite one — which of the two is
    /// [`ON_EMPTY`](Self::ON_EMPTY).
    fn poll_into(&mut self, out: &mut [S]) -> usize;
}

/// A push destination for audio frames: write blocks of **flat interleaved**
/// samples incrementally, then close once.
///
/// Generic over the sample element `S` (default `f32`), matching [`AudioIn`].
/// The channel count is runtime, reported by [`layout`](Self::layout).
///
/// # Why `finalize` consumes `self`
///
/// A destination may need a final commit that can fail and must happen exactly
/// once (a WAV sink back-patches its header; a socket flushes and closes).
/// Taking `self` by value makes "you cannot write after finalizing" a
/// compile-time guarantee and gives the commit a place to surface I/O errors.
pub trait AudioOut<S = f32> {
    /// How many channels one frame of this destination carries.
    ///
    /// Fixed for the life of the sink — it is what the file header (or the
    /// peer) was already told, and it cannot be renegotiated mid-stream.
    fn layout(&self) -> ChannelLayout;

    /// Append `frames` — flat interleaved at [`layout`](Self::layout)'s width —
    /// to the destination. That is `frames.len() / layout().count()` **frames**;
    /// a trailing partial frame is ignored rather than written short, because a
    /// short frame desynchronises the interleave for everything after it.
    ///
    /// Called repeatedly as data arrives; implementations write incrementally
    /// and never buffer the whole stream.
    fn write(&mut self, frames: &[S]);

    /// Close the destination, flushing and committing. For a file sink this is
    /// where the header is back-patched, so a failure here can mean an
    /// unreadable file — surface it rather than swallowing it.
    fn finalize(self) -> std::io::Result<()>;
}

/// Move one block from an [`AudioIn`] to an [`AudioOut`]: poll as many frames as
/// `buf` holds from `src`, write exactly what it produced to `dst`, return that
/// count **in frames**.
///
/// This is the whole of "recording", minus the loop and the stop condition —
/// both of which are the *caller's* policy, not this function's. A recorder
/// runs this on a background thread until its stop flag is set:
///
/// ```ignore
/// let ch = src.layout().count() as usize;
/// let mut buf = vec![0.0f32; 1024 * ch];  // caller owns the buffer — no alloc per pump
/// while running.load(Ordering::Relaxed) {
///     if pump(&mut mic, &mut wav, &mut buf) == 0 {
///         std::thread::yield_now();       // nothing ready — a live source may starve briefly
///     }
/// }
/// wav.finalize()?;                        // caller finalizes once, after the loop
/// ```
///
/// `buf` is flat interleaved and therefore *sized* in samples (`frames * ch`) —
/// a flat slice has no other unit. Everything **returned** is in frames.
///
/// Generic over the element `S` and, not `dyn`, over the concrete source and
/// sink: the caller picks both at the call site, so `poll_into` and `write`
/// inline and the pump allocates nothing (the buffer is caller-owned).
/// Returning `0` means the source had nothing this pass — the caller decides
/// whether that's back-off (live source) or end-of-stream (finite source).
///
/// # The width check — replacing a lost compile error
///
/// The source and sink must agree on width, and with a runtime width that is not
/// a compile error. Two checks carry it instead — here, and at the one entry
/// point that owns a pairing:
///
/// - **this `debug_assert_eq!`**, which fires in every test and debug build the
///   moment a mismatched pair is pumped;
/// - **`tutti_io::Recorder::start`**, which returns an error for the same
///   condition, so a release build refuses the pairing up front rather than
///   writing a file whose channels rotate every frame.
///
/// A `debug_assert` rather than a hard panic because this is the *inner* check:
/// the constructor-level check is what a shipped binary relies on, and panicking
/// here would abort a pump thread mid-take over a condition its caller was
/// already given a chance to reject.
pub fn pump<S, I, O>(src: &mut I, dst: &mut O, buf: &mut [S]) -> usize
where
    S: Copy,
    I: AudioIn<S> + ?Sized,
    O: AudioOut<S> + ?Sized,
{
    debug_assert_eq!(
        src.layout(),
        dst.layout(),
        "pump source and sink must agree on channel width; \
         a mismatch rotates the sink's channels every frame"
    );
    // Once, outside anything per-frame.
    let ch = src.layout().count().max(1) as usize;
    let n = src.poll_into(buf);
    dst.write(&buf[..n * ch]);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finite in-memory [`AudioIn`], generic over element `S` and carrying a
    /// **runtime** width: hands out its frames in bounded chunks, returning a
    /// short-then-zero count at end-of-stream — the shape a decoded file has,
    /// and a stand-in for a live source in a test.
    ///
    /// The width is a *field*, not a type parameter, so the same fixture
    /// exercises stereo and any other width — the shape a data-determined
    /// source needs and a per-width type cannot give.
    struct SliceSource<S> {
        /// Flat interleaved at `layout`'s width.
        samples: Vec<S>,
        /// Read cursor, in FRAMES.
        pos: usize,
        layout: ChannelLayout,
        /// Cap per poll, in frames, to exercise the "source produces fewer than
        /// asked" path.
        chunk: usize,
    }

    impl<S: Copy> AudioIn<S> for SliceSource<S> {
        const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

        fn layout(&self) -> ChannelLayout {
            self.layout
        }

        fn poll_into(&mut self, out: &mut [S]) -> usize {
            let ch = self.layout.count() as usize;
            let total = self.samples.len() / ch;
            let n = (total - self.pos).min(out.len() / ch).min(self.chunk);
            out[..n * ch].copy_from_slice(&self.samples[self.pos * ch..(self.pos + n) * ch]);
            self.pos += n;
            n
        }
    }

    /// A sink that tallies every sample it's handed — no I/O, so the pump
    /// contract (write exactly the polled count, never the untouched tail) can
    /// be asserted without touching disk. Carries its own width, so a
    /// mismatched pair is *expressible* — which is exactly what the runtime
    /// check has to catch, since the type system cannot.
    struct CountingSink<S> {
        written: Vec<S>,
        layout: ChannelLayout,
    }

    impl<S> CountingSink<S> {
        fn new(layout: ChannelLayout) -> Self {
            Self {
                written: Vec::new(),
                layout,
            }
        }
    }

    impl<S: Copy> AudioOut<S> for CountingSink<S> {
        fn layout(&self) -> ChannelLayout {
            self.layout
        }
        fn write(&mut self, frames: &[S]) {
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
        let samples: Vec<f32> = (0..1000).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let mut src = SliceSource {
            samples: samples.clone(),
            pos: 0,
            layout: ChannelLayout::STEREO,
            chunk: 37, // deliberately coprime with the buffer so chunks straddle
        };
        let mut dst = CountingSink::new(ChannelLayout::STEREO);
        let mut buf = vec![0.0f32; 64 * 2];

        let mut total = 0;
        loop {
            let n = pump(&mut src, &mut dst, &mut buf);
            if n == 0 {
                break;
            }
            assert!(
                n <= buf.len() / 2,
                "pump reported more FRAMES than the buffer holds"
            );
            total += n;
        }

        assert_eq!(total, 1000);
        assert_eq!(
            dst.written, samples,
            "frames must arrive intact and in order"
        );
    }

    /// The same pump over a non-default frame — `f64`, six channels — the shape
    /// a 64-bit surround plugin bus wants. Proves the runtime width and the
    /// element type actually thread through `pump`, not just the stereo-`f32`
    /// default that would pass even if the generality were vestigial.
    #[test]
    fn pump_carries_a_64bit_six_channel_frame() {
        let samples: Vec<f64> = (0..500 * 6).map(|i| i as f64).collect();
        let mut src = SliceSource {
            samples: samples.clone(),
            pos: 0,
            layout: ChannelLayout::from(6u16),
            chunk: 41,
        };
        let mut dst: CountingSink<f64> = CountingSink::new(ChannelLayout::from(6u16));
        let mut buf = vec![0.0f64; 64 * 6];

        while pump(&mut src, &mut dst, &mut buf) != 0 {}

        assert_eq!(
            dst.written, samples,
            "wide frames must survive the pump intact"
        );
    }

    /// **Frames, not samples** — the invariant the module docs are entirely
    /// about, checked at a width where the two visibly differ.
    ///
    /// At stereo a sample-denominated count is only 2× off and can hide in a
    /// round number; at six channels it is 6× off and unmissable. Every count
    /// crossing this boundary is asserted: `poll_into`'s return, `pump`'s
    /// return, and the *sample* length the sink actually received.
    #[test]
    fn every_count_on_this_boundary_is_denominated_in_frames() {
        const CH: usize = 6;
        const FRAMES: usize = 100;

        let mut src = SliceSource {
            samples: vec![1.0f32; FRAMES * CH],
            pos: 0,
            layout: ChannelLayout::from(6u16),
            chunk: FRAMES, // no artificial short poll — measure the real ceiling
        };

        // A buffer holding exactly 10 frames' worth of samples.
        let mut buf = vec![0.0f32; 10 * CH];
        assert_eq!(
            src.poll_into(&mut buf),
            10,
            "poll_into returns FRAMES (10), not samples (60)"
        );
        assert_eq!(src.pos, 10, "and the source advanced by 10 FRAMES");

        // And through the pump: the return is frames, the sink got frames * ch.
        let mut src2 = SliceSource {
            samples: vec![1.0f32; FRAMES * CH],
            pos: 0,
            layout: ChannelLayout::from(6u16),
            chunk: FRAMES,
        };
        let mut dst = CountingSink::new(ChannelLayout::from(6u16));
        assert_eq!(
            pump(&mut src2, &mut dst, &mut buf),
            10,
            "pump returns FRAMES"
        );
        assert_eq!(
            dst.written.len(),
            10 * CH,
            "the sink received frames * channels SAMPLES"
        );
    }

    /// A live source stands in for a mic: it withholds frames (returning `0`)
    /// without being exhausted, then yields them later.
    ///
    /// This is the shape that makes [`OnEmpty`] load-bearing — a consumer that
    /// treats its `0` as end-of-stream stops with frames still to come.
    struct StarvingSource {
        samples: Vec<f32>,
        pos: usize,
        /// Poll counter; odd polls return nothing, mimicking a ring the
        /// producer has not refilled yet.
        polls: usize,
    }

    impl AudioIn for StarvingSource {
        const ON_EMPTY: OnEmpty = OnEmpty::Starved;

        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }

        fn poll_into(&mut self, out: &mut [f32]) -> usize {
            self.polls += 1;
            if self.polls % 2 == 1 {
                return 0; // "nothing ready yet" — but not finished
            }
            let total = self.samples.len() / 2;
            let n = (total - self.pos).min(out.len() / 2).min(4);
            out[..n * 2].copy_from_slice(&self.samples[self.pos * 2..(self.pos + n) * 2]);
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
    fn drain<I: AudioIn>(src: &mut I, dst: &mut CountingSink<f32>, max_polls: usize) {
        let mut buf = [0.0f32; 16 * 2];
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
        let samples: Vec<f32> = (0..40).flat_map(|i| [i as f32, -(i as f32)]).collect();

        let mut live = StarvingSource {
            samples: samples.clone(),
            pos: 0,
            polls: 0,
        };
        let mut live_sink: CountingSink<f32> = CountingSink::new(ChannelLayout::STEREO);
        drain(&mut live, &mut live_sink, 100);
        assert_eq!(
            live_sink.written, samples,
            "a starving source must be drained past its empty polls, not truncated at the first"
        );

        let mut finite = SliceSource {
            samples: samples.clone(),
            pos: 0,
            layout: ChannelLayout::STEREO,
            chunk: 7,
        };
        let mut finite_sink: CountingSink<f32> = CountingSink::new(ChannelLayout::STEREO);
        drain(&mut finite, &mut finite_sink, 100);
        assert_eq!(
            finite_sink.written, samples,
            "and a finite source must still deliver everything before stopping"
        );
        assert_eq!(
            finite.pos, 40,
            "the finite source is exhausted, so the loop ended by verdict not by ceiling"
        );
    }

    /// The width check, **inner half**: pumping a mismatched pair trips the
    /// `debug_assert` rather than quietly writing a stream whose channels
    /// rotate every frame.
    ///
    /// The outer half — a checked constructor that errors in release builds too
    /// — is `tutti_io::Recorder::start`, which is the one place both endpoints
    /// are in scope before any frame moves.
    #[test]
    #[should_panic(expected = "must agree on channel width")]
    fn pump_rejects_a_width_mismatch() {
        let mut src = SliceSource {
            samples: vec![0.0f32; 32],
            pos: 0,
            layout: ChannelLayout::STEREO,
            chunk: 8,
        };
        let mut dst = CountingSink::new(ChannelLayout::from(6u16));
        let mut buf = vec![0.0f32; 16];
        pump(&mut src, &mut dst, &mut buf);
    }
}
