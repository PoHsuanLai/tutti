//! [`TapIn`] — a tap's reader, as an [`AudioIn`].
//!
//! A tap (`tutti-graph`'s `Editor::open_tap`, or a session's
//! `Graph::open_tap`) copies sources out of the graph into one ring a lane. Its
//! [`TapReader`] drains them from any thread, perfectly usable by an analysis
//! thread directly, but *not* an [`AudioIn`], so on its own it can feed neither
//! a [`pump`](tutti_types::pump) nor a [`Recorder`](crate::Recorder).
//!
//! This is the join that makes *record what the graph is producing*
//! expressible in the I/O vocabulary, and deliberately nothing more: a newtype
//! that pops the reader into the flat interleaved frame layout `AudioIn`
//! speaks, as wide as the tap is.
//!
//! # The rate is the caller's to match
//!
//! Like every other [`AudioIn`], this carries no sample rate — the trait
//! deliberately has none. The tap runs at the graph's rate, so a sink built for
//! anything else writes a valid WAV that plays at the wrong speed. There is no
//! `matching_sink` here as there is on `MicIn`, because a tap has no device to
//! ask: the rate lives on `AudioConfig`, which is the host's.

use tutti_graph::TapReader;
use tutti_types::{AudioIn, OnEmpty};
use tutti_types::{ChannelLayout, Samples};

/// A tap's reader as an [`AudioIn`]: records what the graph is producing.
///
/// Construct it with [`new`](Self::new) from the reader a tap opened, and drain
/// it from a pump thread (for example a [`Recorder`](crate::Recorder)), never
/// from the audio thread. Its [`layout`](AudioIn::layout) is as wide as the
/// tap: a stereo output tap is stereo, a six-lane tap six channels, so the
/// sink must be built to match (`Recorder::start` refuses a mismatch).
///
/// Like every [`AudioIn`] it carries no sample rate: the tap runs at the
/// graph's rate, so build the sink at that rate or the file plays at the wrong
/// speed.
///
/// # Examples
///
/// ```no_run
/// use tutti_io::{Pcm, Recorder, TapIn, WavOut};
///
/// # fn go(reader: tutti_graph::TapReader) -> tutti_io::Result<()> {
/// let src = TapIn::new(reader);
/// let wav = WavOut::create("master.wav", 48_000.0, 2u16, Pcm::Float32)?;
/// let rec = Recorder::start(src, wav)?; // a stereo tap and a stereo sink pair
/// // ... later ...
/// rec.finish()
/// # }
/// ```
#[derive(Debug)]
pub struct TapIn {
    reader: TapReader,
}

impl TapIn {
    /// Wraps a tap's reader.
    ///
    /// Takes the reader by value because a tap has exactly one.
    pub fn new(reader: TapReader) -> Self {
        Self { reader }
    }
}

impl AudioIn for TapIn {
    /// The graph does not end.
    ///
    /// An empty tap means the graph has not rendered since the last poll — the
    /// audio thread was between blocks, or the tap was only just opened (it
    /// flows from the commit that carries it). It never means "finished", the
    /// way a decoded file's exhaustion does. A consumer that read this zero as
    /// end-of-stream would stop recording within a block of starting, which is
    /// exactly the mistake [`OnEmpty`] exists to make unrepresentable.
    const ON_EMPTY: OnEmpty = OnEmpty::Starved;

    /// As wide as the tap: one channel a lane.
    fn layout(&self) -> ChannelLayout {
        self.reader.layout()
    }

    /// Pop up to `out.len() / width` FRAMES into `out`, returning the FRAME
    /// count.
    ///
    /// `out` is flat interleaved — `width` samples per frame — so the return
    /// is the filled length over the width, never the sample count. A short or
    /// zero count is normal for a live source; the pump parks and retries.
    fn poll_into(&mut self, out: &mut [f32]) -> Samples {
        self.reader.pop_into(out)
    }
}

#[cfg(test)]
mod tests;
