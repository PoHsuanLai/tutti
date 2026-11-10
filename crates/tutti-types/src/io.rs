//! The engine's edge vocabulary: [`Source`] (frames pulled in) and [`Sink`]
//! (frames written out), and [`Pull`], what a pull answers.
//!
//! An **edge** is a stream on a clock other than the graph's, read by pulling
//! interleaved frames that may starve or end: a microphone, a decoded file, a
//! graph tap's reader, a file encoder. A `tutti_graph::Node`, by contrast, is
//! called by the graph's clock, planar, one block at a time; the rings in
//! `tutti-graph` (the tap going out, the inlet coming in) are where an edge
//! meets the graph. Disk streaming inside the sampler is positional and is not
//! this vocabulary (doc 025). A future edge (a network socket, a generator) would
//! implement these traits; none exists today.
//!
//! ```text
//!   Source  ──pull──▶  [your buffer]  ──write──▶  Sink
//!   (mic, file, tap)                              (a file)
//! ```
//!
//! Recording is a source pumped into a sink: pull a block, write it, repeat.
//! `tutti-session`'s `PumpLoop` is that pass and its `Recorder` the thread that
//! runs it.
//!
//! Homed in `tutti-types`, the root leaf, so every subsystem can implement
//! these without depending on any particular engine crate.
//!
//! # The verdict comes back from the call
//!
//! An empty read means nothing by itself: the producer may be behind, finished,
//! or broken, and only the source knows which. [`Pull`] says so with each call
//! ([`Starved`](Pull::Starved), [`Ended`](Pull::Ended),
//! [`Failed`](Pull::Failed)), so a source cannot be written without answering,
//! and a caller cannot ignore the answer ([`Pull`] is `#[must_use]`). Treating a
//! live microphone as finished would end a recording at its first empty ring,
//! with no error anywhere; the type makes that a match arm the source wrote, not a
//! flag a caller had to remember.
//!
//! # The buffer: a flat interleaved `&[f32]`, width from [`ChannelLayout`]
//!
//! Both traits take a flat interleaved slice. The channel count is a runtime
//! [`ChannelLayout`] the source or sink reports from `layout`, because a width
//! that is a property of the *data* (a decoded file, a device a user just plugged
//! in) cannot be a const parameter.
//!
//! # Counts are in FRAMES. Always. Everywhere.
//!
//! - [`Source::pull`] answers [`Pull::Frames`] with a frame count, a
//!   [`Samples`]. The samples it wrote are
//!   [`returned.interleaved_len(layout())`](Samples::interleaved_len).
//! - [`Sink::write`] is handed
//!   [`Samples::from_interleaved_len(interleaved.len(), layout())`](Samples::from_interleaved_len)
//!   frames. A trailing partial frame is not a frame.
//!
//! Those two conversions are the only crossings between the units, so a stride
//! mistake has one home. A bare `usize` from a slice length does not type-check
//! where a frame count is expected.
//!
//! # Width agreement is a runtime check
//!
//! "A stereo source cannot feed a 6-channel sink" is not a type error with a
//! runtime width. Two runtime checks carry it, both load-bearing: a recorder's
//! `start` returns an error, and its pump asserts in debug builds (see
//! `tutti-session`'s `Recorder` and `PumpLoop`).
//!
//! # Not the RT hot path
//!
//! Neither trait is invoked per sample on the audio thread. They move frames in
//! blocks on a cold or background path (a capture pump, an offline render).

use crate::{ChannelLayout, Samples};

/// What one [`Source::pull`] found.
///
/// `Frames` and a verdict never arrive together: frames now, [`Failed`](Self::Failed)
/// or [`Ended`](Self::Ended) on the next call. `V` is the frame count
/// ([`Samples`]) unless an adapter carries more.
#[must_use = "a pull's verdict (starved, ended, failed) is the only way to tell a late source from a finished one"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull<V = Samples> {
    /// At least one frame was produced.
    Frames(V),
    /// Nothing yet; more may come. A live source (a microphone, a tap) says
    /// this when its producer has not caught up. A consumer that loops should
    /// back off and pull again.
    Starved,
    /// Nothing more, ever. A finite source (a decoded file) says this. A
    /// consumer that loops should stop.
    Ended,
    /// Broken: nothing more will come however long a consumer waits. The frames
    /// before the break came with earlier pulls.
    Failed,
}

/// Why a [`Pull`] produced no frames: [`Pull::ready`]'s error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// [`Pull::Starved`].
    Starved,
    /// [`Pull::Ended`].
    Ended,
    /// [`Pull::Failed`].
    Failed,
}

impl<V> Pull<V> {
    /// The frames, or why there are none: the stable stand-in for `?`.
    ///
    /// # Errors
    ///
    /// The [`Stop`] a verdict stands for.
    pub fn ready(self) -> Result<V, Stop> {
        match self {
            Self::Frames(v) => Ok(v),
            Self::Starved => Err(Stop::Starved),
            Self::Ended => Err(Stop::Ended),
            Self::Failed => Err(Stop::Failed),
        }
    }

    /// Maps the frames and keeps the verdict.
    pub fn map<U>(self, f: impl FnOnce(V) -> U) -> Pull<U> {
        match self {
            Self::Frames(v) => Pull::Frames(f(v)),
            Self::Starved => Pull::Starved,
            Self::Ended => Pull::Ended,
            Self::Failed => Pull::Failed,
        }
    }

    /// Why there are no frames, or `None` when there are some.
    #[must_use]
    pub fn stop(&self) -> Option<Stop> {
        match self {
            Self::Frames(_) => None,
            Self::Starved => Some(Stop::Starved),
            Self::Ended => Some(Stop::Ended),
            Self::Failed => Some(Stop::Failed),
        }
    }
}

/// A source of audio frames, pulled in order on a clock the graph does not keep.
///
/// Fills a caller-owned **flat interleaved** buffer and answers with a
/// [`Pull`]. The channel count is runtime, reported by [`layout`](Self::layout):
/// see the [module docs](self) for why, and for the frames-not-samples rule that
/// governs every count here.
///
/// # Rules
///
/// - [`Frames`](Pull::Frames) and a verdict never arrive together.
/// - A live source reads its fault flag **before** it reads its ring and
///   answers [`Failed`](Pull::Failed) only if the ring was empty, so a failure
///   never hides frames pushed before it (`tutti_rt::FaultLatch`).
/// - A synchronous source (a file) never answers [`Starved`](Pull::Starved).
///   One enum serves both kinds: [`Pull::ready`] maps the arm that cannot occur
///   to an `Err`.
pub trait Source {
    /// How many channels one frame of this source carries.
    ///
    /// Fixed for the life of the source: a caller sizes its buffer from this
    /// once, before the loop, and `pull` never changes it underneath.
    fn layout(&self) -> ChannelLayout;

    /// Fills the front of `out` with the next available frames.
    ///
    /// `out` is flat interleaved at [`layout`](Self::layout)'s width, so it
    /// holds [`Samples::from_interleaved_len(out.len(), layout())`](Samples::from_interleaved_len)
    /// frames and a [`Frames`](Pull::Frames) count is bounded by that. Samples
    /// past `n.interleaved_len(layout())` are left untouched, and a trailing
    /// partial frame is never partly filled: a short frame desynchronises the
    /// interleave for everything after it.
    fn pull(&mut self, out: &mut [f32]) -> Pull;

    /// The frames this source discarded since the last call, and clears the
    /// count: a gap in what `pull` delivered.
    ///
    /// A live source with a bounded buffer (a graph tap's ring, a capture
    /// ring) drops what a slow consumer does not drain in time. The stream a
    /// consumer reads then **closes up over the gap**: a recording of it is
    /// shorter than the take by the frames lost, with no marker in the audio.
    /// This is the marker. The default is zero, right for a source that never
    /// discards.
    fn take_lost(&mut self) -> Samples {
        Samples::ZERO
    }
}

/// A sink for audio frames: written incrementally, then closed once.
///
/// Takes blocks of **flat interleaved** samples, then is closed with
/// [`finalize`](Self::finalize). The channel count is runtime, reported by
/// [`layout`](Self::layout).
///
/// # Why `finalize` consumes `self`
///
/// A destination may need a final commit that can fail and must happen exactly
/// once (a WAV sink back-patches its header). Taking `self` makes "you cannot
/// write after finalizing" a compile-time guarantee and gives the commit a
/// place to surface I/O errors.
pub trait Sink {
    /// How many channels one frame of this destination carries.
    ///
    /// Fixed for the life of the sink: it is what the file header was already
    /// told, and it cannot be renegotiated mid-stream.
    fn layout(&self) -> ChannelLayout;

    /// Appends `interleaved`, flat interleaved **samples** at
    /// [`layout`](Self::layout)'s width: [`Samples::from_interleaved_len(interleaved.len(), layout())`](Samples::from_interleaved_len)
    /// **frames**. A trailing partial frame is ignored rather than written
    /// short, because a short frame desynchronises the interleave for
    /// everything after it.
    ///
    /// A caller computes the slice length with [`Samples::interleaved_len`],
    /// never by hand.
    ///
    /// # Errors
    ///
    /// [`SinkFailed`] once a write has failed; later writes are skipped and
    /// fail the same way. The detail of the *first* failure comes from
    /// [`finalize`](Self::finalize).
    fn write(&mut self, interleaved: &[f32]) -> Result<(), SinkFailed>;

    /// Closes the destination, flushing and committing.
    ///
    /// For a file sink this is where the header is back-patched, so a failure
    /// here can mean an unreadable file: surface it rather than swallowing it.
    /// It also reports the first [`write`](Self::write) failure, with its detail.
    ///
    /// # Errors
    ///
    /// The first write failure, or any I/O error from the final flush or commit.
    fn finalize(self) -> std::io::Result<()>;
}

/// A [`Sink::write`] that failed, now or earlier.
///
/// A marker, not the detail: a sink is synchronous on the caller's thread, so
/// "write, then ask whether it failed" is a `Result`, and the first
/// `io::Error` waits for [`Sink::finalize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkFailed;

impl std::fmt::Display for SinkFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a write to the sink failed; finalize reports the first failure")
    }
}

impl std::error::Error for SinkFailed {}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ready`, `map` and `stop` agree on every arm, so a caller can use any of
    /// them. Mutation: map `Ended` to `Stop::Starved` in `ready` and the first
    /// assertion fails.
    #[test]
    fn a_pull_names_why_it_has_no_frames() {
        assert_eq!(Pull::Frames(Samples(3)).ready(), Ok(Samples(3)));
        assert_eq!(Pull::<Samples>::Starved.ready(), Err(Stop::Starved));
        assert_eq!(Pull::<Samples>::Ended.ready(), Err(Stop::Ended));
        assert_eq!(Pull::<Samples>::Failed.ready(), Err(Stop::Failed));

        assert_eq!(Pull::Frames(Samples(3)).stop(), None);
        assert_eq!(Pull::<Samples>::Starved.stop(), Some(Stop::Starved));
        assert_eq!(Pull::<Samples>::Ended.stop(), Some(Stop::Ended));
        assert_eq!(Pull::<Samples>::Failed.stop(), Some(Stop::Failed));
    }

    /// `map` changes the frames and keeps the verdict.
    #[test]
    fn map_keeps_the_verdict() {
        assert_eq!(
            Pull::Frames(Samples(2)).map(|n| n.get() * 2),
            Pull::Frames(4)
        );
        assert_eq!(Pull::<Samples>::Ended.map(|n| n.get()), Pull::Ended);
        assert_eq!(Pull::<Samples>::Starved.map(|n| n.get()), Pull::Starved);
        assert_eq!(Pull::<Samples>::Failed.map(|n| n.get()), Pull::Failed);
    }
}
