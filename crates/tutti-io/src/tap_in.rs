//! [`TapIn`] — the analysis tap's consumer end, as an [`AudioIn`].
//!
//! `AudioTap` (in `tutti-core`) is the RT-push half: the audio callback copies
//! every master block into a lock-free ring, and `AudioTap::open` hands back the
//! consumer as a [`TapCons`] — perfectly usable by an analysis thread draining
//! it directly, but *not* an [`AudioIn`], so on its own it can feed neither a
//! [`pump`](tutti_types::pump) nor a [`Recorder`](crate::Recorder).
//!
//! This is the join that makes *record what I am hearing* expressible in the I/O
//! vocabulary, and deliberately nothing more: a newtype that pops the ring into
//! the flat interleaved frame layout `AudioIn` speaks.
//!
//! # The rate is the caller's to match
//!
//! Like every other [`AudioIn`], this carries no sample rate — the trait
//! deliberately has none. The tap runs at the graph's rate, so a sink built for
//! anything else writes a valid WAV that plays at the wrong speed. There is no
//! `matching_sink` here as there is on `MicIn`, because a tap has no device to
//! ask: the rate lives on `AudioConfig`, which is the host's.

use tutti_metering::TapCons;
use tutti_types::{AudioIn, OnEmpty};
use tutti_types::{ChannelLayout, Samples};

/// The master-output analysis tap's consumer end, as an [`AudioIn`]: records
/// what the graph is playing.
///
/// The audio callback copies every master block into a lock-free stereo ring
/// ([`tutti_metering::AudioTap`]); construct this with [`new`](Self::new) from what
/// `AudioTap::open` returned, and drain it from a pump thread (for example a
/// [`Recorder`](crate::Recorder)), never from the audio thread.
///
/// Like every [`AudioIn`] it carries no sample rate: the tap runs at the
/// graph's rate, so build the sink at that rate or the file plays at the wrong
/// speed.
///
/// # Examples
///
/// ```no_run
/// use tutti_io::{BitDepth, Recorder, TapIn, WavOut};
///
/// # fn go(tap: &tutti_metering::AudioTap) -> tutti_io::Result<()> {
/// let src = TapIn::new(tap.open().expect("tap is free"));
/// let wav = WavOut::create("master.wav", 48_000.0, 2u16, BitDepth::Float32)?;
/// let rec = Recorder::start(src, wav)?; // both are stereo, so this pairs
/// // ... later ...
/// rec.finish()
/// # }
/// ```
#[derive(Debug)]
pub struct TapIn {
    cons: TapCons,
}

impl TapIn {
    /// Wraps the consumer half of an opened analysis tap.
    ///
    /// Takes the consumer by value because a ring has exactly one reader, and
    /// `AudioTap::open` refuses while a consumer is live.
    pub fn new(cons: TapCons) -> Self {
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
        ChannelLayout::STEREO
    }

    /// Pop up to `out.len() / 2` FRAMES into `out`, returning the FRAME count.
    ///
    /// `out` is flat interleaved stereo — two samples per frame — so the return
    /// is half its filled length, never the sample count. A short or zero count
    /// is normal for a live source; the pump parks and retries.
    fn poll_into(&mut self, out: &mut [f32]) -> Samples {
        self.cons.pop_into(out)
    }
}

#[cfg(test)]
mod tests;
