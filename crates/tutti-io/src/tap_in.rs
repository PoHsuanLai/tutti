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
/// **A tap that falls behind loses frames, and the recording closes up over
/// the gap.** The rings are bounded (the capacity named at `open_tap`); what
/// the pump does not drain in time is dropped and counted. The file is then
/// shorter than the take by [`Recorder::lost`](crate::Recorder::lost) frames,
/// with no marker in the audio. A host that cares reads that count when the
/// take ends (`AudioPump`'s `PumpFinished` carries it too).
///
/// **Which tap.** To record what the output sounds like, give `TapIn` the
/// reader of `Session::open_heard_tap` (bevy's `AudioGraphRes::open_heard_tap`):
/// the engine's output after the declick fader, every device channel, so a
/// take across a transport stop fades as the engine's output buffer does (the f32 mix, before the device layer clamps and converts it). A plan tap
/// (`Graph::open_tap`, `Graph::open_output_tap`) is the explicit choice of the
/// graph's own signal, before the fader: a transport-gated source (a clip, the
/// click, a sampler voice) ends on the stop's frame, so that take has a step
/// there (measured: the source's whole level in one frame), and a block the
/// engine refuses leaves no frames in it.
///
/// Like every [`AudioIn`] it carries no sample rate: the tap runs at the
/// graph's rate, so build the sink at that rate or the file plays at the wrong
/// speed.
///
/// # Examples
///
/// ```no_run
/// use tutti_codec::Pcm;
/// use tutti_io::{Recorder, TapIn, WavOut};
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

    /// The frames the tap's rings refused since the last call: the pump did
    /// not drain them in time. A [`Recorder`](crate::Recorder) sums them
    /// ([`lost`](crate::Recorder::lost)); the recording closes up over each gap.
    fn take_lost(&mut self) -> Samples {
        self.reader.take_lost()
    }
}

#[cfg(test)]
mod tests;
