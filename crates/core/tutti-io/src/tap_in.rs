//! [`TapIn`] — the analysis tap's consumer end, as an [`AudioIn`].
//!
//! `AudioTap` (in `tutti-core`) is the RT-push half: the audio callback copies
//! every master block into a lock-free ring, and `AudioTap::open` hands back the
//! consumer. That consumer is a bare `HeapCons<(f32, f32)>` — perfectly usable
//! by an analysis thread that wants to drain it directly, but *not* an
//! [`AudioIn`], so it could not feed a [`pump`](tutti_core::io::pump) or a
//! [`Recorder`](crate::Recorder).
//!
//! Which meant the most obvious thing a host asks for — *record what I am
//! hearing* — was the one capture path the I/O vocabulary could not express.
//! Every piece existed; nothing joined them.
//!
//! This is that join, and deliberately nothing more: a newtype that pops the
//! ring into the frame layout `AudioIn` speaks.
//!
//! ```no_run
//! # use tutti_io::{TapIn, WavOut, BitDepth, Recorder};
//! # fn go(tap: &tutti_core::metering::AudioTap) -> std::io::Result<()> {
//! let src = TapIn::new(tap.open().expect("tap is free"));
//! let wav = WavOut::create(&"master.wav".into(), 48_000.0, 2u16, BitDepth::Float32)
//!     .expect("sink opens");
//! let rec = Recorder::start(src, wav)?;   // both are stereo, so this pairs
//! // ... later ...
//! rec.stop()
//! # }
//! ```
//!
//! # The rate is the caller's to match
//!
//! Like every other [`AudioIn`], this carries no sample rate — the trait
//! deliberately has none. The tap runs at the graph's rate, so a sink built for
//! anything else writes a valid WAV that plays at the wrong speed. There is no
//! `matching_sink` here as there is on `MicIn`, because a tap has no device to
//! ask: the rate lives on `AudioConfig`, which is the host's.

use ringbuf::{traits::Consumer, HeapCons};

use tutti_core::io::{AudioIn, OnEmpty};
use tutti_core::ChannelLayout;

/// The analysis tap's consumer end as an [`AudioIn`].
///
/// Construct with [`new`](Self::new) from whatever `AudioTap::open` returned.
/// Drained by a pump thread — never by the audio thread, which is the *producer*
/// side of this ring.
pub struct TapIn {
    cons: HeapCons<(f32, f32)>,
}

impl TapIn {
    /// Wrap the consumer half of an opened analysis tap.
    ///
    /// Takes the consumer by value because a ring has exactly one reader: the
    /// pump that owns this owns the drain. `AudioTap::open` enforces the other
    /// half — it refuses while a consumer is live — so together they make "two
    /// readers on one ring" unrepresentable rather than merely discouraged.
    pub fn new(cons: HeapCons<(f32, f32)>) -> Self {
        Self { cons }
    }
}

impl AudioIn for TapIn {
    /// The graph does not end.
    ///
    /// An empty tap means the callback has not pushed since the last poll — the
    /// audio thread was between blocks, or the tap was only just opened. It
    /// never means "finished", the way a decoded file's exhaustion does. A
    /// consumer that read this zero as end-of-stream would stop recording
    /// within a block of starting, which is exactly the mistake
    /// [`OnEmpty`] exists to make unrepresentable.
    const ON_EMPTY: OnEmpty = OnEmpty::Starved;

    /// Always stereo: the tap ring's element is `(f32, f32)`, so the width is a
    /// property of this *code*, not of any data flowing through it — exactly the
    /// case a fixed layout still fits. Widening it means widening the lock-free
    /// ring element in `tutti-core`'s `AudioTap`, which is a separate change.
    fn layout(&self) -> ChannelLayout {
        ChannelLayout::Stereo
    }

    fn poll_into(&mut self, out: &mut [f32]) -> usize {
        // Pop up to `out.len() / 2` FRAMES — the return is frames, the slice is
        // samples. A short or zero count is normal for a live source; the pump
        // parks and retries.
        let frames = out.len() / 2;
        let mut n = 0;
        while n < frames {
            match self.cons.try_pop() {
                Some((l, r)) => {
                    out[n * 2] = l;
                    out[n * 2 + 1] = r;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::{Producer, Split};
    use ringbuf::HeapRb;

    /// Frames the callback pushed come back out in order, interleaved L then R.
    ///
    /// The tap stores `(f32, f32)` and `AudioIn` speaks a flat interleaved
    /// slice; this is the conversion, so a transposition here would silently
    /// swap every recording's channels.
    #[test]
    fn pushed_frames_come_back_in_order_and_channel_side() {
        let (mut prod, cons) = HeapRb::<(f32, f32)>::new(16).split();
        for i in 0..4 {
            prod.try_push((i as f32, -(i as f32))).unwrap();
        }

        let mut tap = TapIn::new(cons);
        let mut out = [0.0f32; 8 * 2];
        assert_eq!(
            tap.poll_into(&mut out),
            4,
            "the return is FRAMES, not samples"
        );

        for i in 0..4 {
            assert_eq!(out[i * 2], i as f32, "left channel, frame {i}");
            assert_eq!(out[i * 2 + 1], -(i as f32), "right channel, frame {i}");
        }
    }

    /// A poll is bounded by the output slice, not by what the ring holds.
    ///
    /// The pump hands a fixed scratch buffer every pass; writing past it would
    /// be the kind of overrun no test above would notice.
    #[test]
    fn a_poll_never_writes_past_the_output_slice() {
        let (mut prod, cons) = HeapRb::<(f32, f32)>::new(64).split();
        for _ in 0..32 {
            prod.try_push((1.0, 1.0)).unwrap();
        }

        let mut tap = TapIn::new(cons);
        let mut out = [0.0f32; 4 * 2];
        assert_eq!(tap.poll_into(&mut out), 4, "must fill exactly the slice");

        // The rest is still queued, not dropped.
        let mut rest = [0.0f32; 32 * 2];
        assert_eq!(tap.poll_into(&mut rest), 28);
    }

    /// An empty tap yields nothing and says so — without claiming the end.
    ///
    /// The `ON_EMPTY` assertion is the load-bearing half: a `Starved` source
    /// makes a pump park and retry, and the recording survives the gap between
    /// audio callbacks. `EndOfStream` here would end every take immediately.
    #[test]
    fn an_empty_tap_starves_rather_than_ending() {
        let (_prod, cons) = HeapRb::<(f32, f32)>::new(8).split();
        let mut tap = TapIn::new(cons);

        let mut out = [0.0f32; 4 * 2];
        assert_eq!(tap.poll_into(&mut out), 0);
        assert_eq!(
            TapIn::ON_EMPTY,
            OnEmpty::Starved,
            "the graph does not end; an empty tap is a gap, not a finish"
        );
    }

    /// The join this type exists for: a tap consumer satisfies the bound
    /// `Recorder::start` and `pump` require.
    ///
    /// Asserted as a compile-time bound rather than by running a recorder — the
    /// defect it guards against is `TapIn` failing to *be* an `AudioIn`, which
    /// is a type error, not a behaviour.
    #[test]
    fn a_tap_is_accepted_where_a_live_source_is_required() {
        fn takes_a_live_source<I: AudioIn + Send + 'static>(_: I) {}

        let (_prod, cons) = HeapRb::<(f32, f32)>::new(8).split();
        takes_a_live_source(TapIn::new(cons));
    }
}
