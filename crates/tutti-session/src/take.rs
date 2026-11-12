//! Takes: a recording that knows where it belongs on the timeline (design 030
//! section 5).
//!
//! [`Session::record`] arms a take on an [`Input`]. It opens a plan tap on the
//! input's inlet node, so the take is made of what the graph was given, frame
//! for frame on the graph's own clock ([`TapReader::next_frame`]), and runs the
//! pump into a sink on the session's spawner. Nothing is resampled or shifted
//! in the file: where the take belongs is metadata, a [`Placement`] the take
//! states term by term.
//!
//! # Where the take is placed
//!
//! A sample the inlet emits at graph frame `F` was played by the performer in
//! answer to what the device played [`out_latency`](Terms::out_latency) after it
//! was rendered, and reached the graph [`in_latency`](Terms::in_latency) plus the
//! inlet's [`lead`](Terms::inlet_lead) after it was sung, so the take's first
//! frame belongs at
//!
//! ```text
//! start = F - (in_latency + out_latency + inlet_lead) + offset
//! ```
//!
//! A latency the device has not reported is not a zero: the term is `None` in
//! [`Terms`], left out of the sum, and [`Terms::is_complete`] says so.
//!
//! # Punch
//!
//! A [`Punch`] names where the take starts and, optionally, where it stops, as
//! an [`At`]. A beat is turned into a frame on the control side, through the
//! project's tempo map, at the frame the transport will reach if it keeps
//! rolling from where the playhead is now ([`Session::playhead`]); a beat punch while the
//! transport is stopped is refused ([`TakeError::TransportStopped`]), and a
//! locate between arming and the punch is not followed. The pump drops tapped
//! frames before the punch-in and stops at the punch-out. A punch-in already
//! past starts at once, and the take reports the real `F`.
//!
//! # A gap is silence in place
//!
//! A tap that the pump cannot drain in time loses frames
//! ([`TapReader::take_lost`]); a take, unlike a plain recording, keeps its
//! alignment over the gap by writing silence for it, and counts the frames
//! ([`Recorded::lost`]). A dropout in the capture itself reaches the take as the
//! silence the inlet played.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, OnceLock};

use thiserror::Error;
use tutti_codec::{FileOut, Format, Spec};
use tutti_cpal::StreamLatency;
use tutti_graph::{MissingSource, NodeKey, TapAlign, TapReader};
use tutti_tempo::TempoMap;
use tutti_types::{
    At, Beat, ChannelLayout, Frame, SampleRate, Samples, Seconds, Sink, SinkFailed,
    first_frame_at_or_after,
};

use crate::jobs::{JobFailed, Landed, Landings};
use crate::recorder::IDLE_PARK;
use crate::{Input, Session, SessionEvent, Trim};

/// How many frames the pump moves a pass.
const PASS_FRAMES: usize = 1024;

/// One take of a session: the number [`TakeEvent`]s carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TakeId(u64);

impl fmt::Display for TakeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "take {}", self.0)
    }
}

/// Where a take starts and, if it has one, where it stops.
///
/// ```
/// use tutti_session::Punch;
/// use tutti_types::{At, Beat};
///
/// // Beat 16 to beat 48.
/// let punch = Punch::at(Beat(16.0)).until(Beat(48.0)).unwrap();
/// // Open-ended, from the next block.
/// let _ = Punch::now();
/// // Backwards is refused where it is made.
/// assert!(Punch::at(Beat(16.0)).until(Beat(8.0)).is_err());
/// # let _ = (punch, At::NextBlock);
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Punch {
    at: At,
    until: Option<At>,
}

impl Default for Punch {
    /// [`Punch::now`].
    fn default() -> Self {
        Self::now()
    }
}

impl Punch {
    /// A take that starts at `at` and runs until it is finished.
    pub fn at(at: impl Into<At>) -> Self {
        Self {
            at: at.into(),
            until: None,
        }
    }

    /// A take that starts at once, from the next block.
    #[must_use]
    pub fn now() -> Self {
        Self::at(At::NextBlock)
    }

    /// This punch ending at `until`.
    ///
    /// # Errors
    ///
    /// [`PunchError`] when `until` is not after the punch-in: the same kind of
    /// position (two beats, two frames) that does not come later, or
    /// [`At::NextBlock`], which is a start and never an end. A beat against a
    /// frame cannot be compared until the take is armed; [`Session::record`]
    /// checks it there ([`TakeError::Punch`]).
    pub fn until(self, until: impl Into<At>) -> Result<Self, PunchError> {
        let until = until.into();
        let later = match (self.at, until) {
            (_, At::NextBlock) => false,
            (At::Beat(a), At::Beat(b)) => b.get() > a.get(),
            (At::Frame(a), At::Frame(b)) => b > a,
            // Not comparable until the transport's position is known.
            _ => true,
        };
        if later {
            Ok(Self {
                until: Some(until),
                ..self
            })
        } else {
            Err(PunchError { at: self.at, until })
        }
    }

    /// Where the take starts.
    #[must_use]
    pub fn start(&self) -> At {
        self.at
    }

    /// Where the take stops, if it is not open-ended.
    #[must_use]
    pub fn end(&self) -> Option<At> {
        self.until
    }
}

/// A punch that ends where it begins or before ([`Punch::until`]).
#[derive(Clone, Copy, Debug, PartialEq, Error)]
#[error("a punch ending at {until:?} is not after its start at {at:?}")]
pub struct PunchError {
    /// The punch-in.
    pub at: At,
    /// The refused punch-out.
    pub until: At,
}

/// The frames of the graph's clock a punch resolved to, when the take was
/// armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// The first frame kept; one already past means "at once".
    pub start: Frame,
    /// The first frame not kept, or `None` for an open-ended take.
    pub end: Option<Frame>,
}

/// Where a take goes: a file, or a sink of the caller's.
///
/// A file is written by [`FileOut`] in the format given. To dither, wrap a
/// file's sink yourself ([`DitherSink`](crate::DitherSink)) and give it to
/// [`sink`](Self::sink); such a take has no path, so no
/// [`clip`](Recorded::clip).
pub struct TakeTarget {
    kind: TargetKind,
}

enum TargetKind {
    File { path: PathBuf, format: Format },
    Sink(Box<dyn DynSink>),
}

impl fmt::Debug for TakeTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TargetKind::File { path, format } => f
                .debug_struct("TakeTarget::File")
                .field("path", path)
                .field("format", format)
                .finish(),
            TargetKind::Sink(_) => f.debug_struct("TakeTarget::Sink").finish_non_exhaustive(),
        }
    }
}

impl TakeTarget {
    /// A file at `path`, in `format`, as wide as the input and at the graph's
    /// rate.
    pub fn file(path: impl Into<PathBuf>, format: Format) -> Self {
        Self {
            kind: TargetKind::File {
                path: path.into(),
                format,
            },
        }
    }

    /// A WAV file of 32-bit float at `path`.
    pub fn wav(path: impl Into<PathBuf>) -> Self {
        Self::file(path, Format::Wav(tutti_codec::Pcm::Float32))
    }

    /// A sink of the caller's. It must be as wide as the input and at the
    /// graph's rate (a sink carries no rate to check); a width that differs is
    /// [`TakeError::Width`].
    pub fn sink(sink: impl Sink + Send + 'static) -> Self {
        Self {
            kind: TargetKind::Sink(Box::new(sink)),
        }
    }
}

/// A [`Sink`] that can be finalized through a box.
trait DynSink: Send {
    fn layout(&self) -> ChannelLayout;
    fn write(&mut self, interleaved: &[f32]) -> Result<(), SinkFailed>;
    fn finalize(self: Box<Self>) -> std::io::Result<()>;
}

impl<S: Sink + Send> DynSink for S {
    fn layout(&self) -> ChannelLayout {
        Sink::layout(self)
    }

    fn write(&mut self, interleaved: &[f32]) -> Result<(), SinkFailed> {
        Sink::write(self, interleaved)
    }

    fn finalize(self: Box<Self>) -> std::io::Result<()> {
        Sink::finalize(*self)
    }
}

/// What the device reports of its delay, read when a take starts: the
/// capture's and the playback's, in time, turned into frames at the graph's
/// rate. A direction nobody has reported is `None`.
#[derive(Debug)]
pub(crate) struct LatencyProbe {
    input: Arc<StreamLatency>,
    output: Option<Arc<StreamLatency>>,
}

impl LatencyProbe {
    pub(crate) fn new(input: Arc<StreamLatency>, output: Option<Arc<StreamLatency>>) -> Self {
        Self { input, output }
    }

    fn read(&self, rate: SampleRate) -> (Option<Samples>, Option<Samples>) {
        let frames = |cell: &StreamLatency| {
            cell.get()
                .map(|delay| Seconds(delay.as_secs_f64()).to_samples(rate))
        };
        (frames(&self.input), self.output.as_deref().and_then(frames))
    }
}

/// The terms of a take's placement, each as the take found it: a reading, read
/// from [`Placement::terms`] or [`Recorded::terms`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Terms {
    /// The capture's delay: how long before a graph block a frame was sung.
    /// `None`: the input has not reported one.
    pub in_latency: Option<Samples>,
    /// The playback's delay: how long after a block was rendered it was
    /// heard. `None`: the output has not reported one.
    pub out_latency: Option<Samples>,
    /// The inlet's fixed lead (`OpenedInput::lead`).
    pub inlet_lead: Samples,
    /// The input's manual trim (`InputConfig::offset`).
    pub offset: Trim,
}

impl Terms {
    /// The terms of a placement.
    #[must_use]
    pub(crate) fn new(
        in_latency: Option<Samples>,
        out_latency: Option<Samples>,
        inlet_lead: Samples,
        offset: Trim,
    ) -> Self {
        Self {
            in_latency,
            out_latency,
            inlet_lead,
            offset,
        }
    }

    /// Whether both latencies were reported. When they were not, the start is
    /// placed by the terms that were, and is that much later than it belongs.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.in_latency.is_some() && self.out_latency.is_some()
    }

    /// The start these terms put a take whose first frame is graph frame
    /// `first` at: `first - (in + out + lead) + offset`, with an unreported
    /// latency left out.
    #[must_use]
    pub fn start(&self, first: Frame) -> Start {
        let behind = self.in_latency.map_or(0, Samples::get) as i128
            + self.out_latency.map_or(0, Samples::get) as i128
            + self.inlet_lead.get() as i128;
        let at = i128::from(first.get()) - behind + self.offset.frames();
        match u64::try_from(at) {
            Ok(frame) => Start::At(Frame(frame)),
            Err(_) => Start::BeforeOrigin(Samples(usize::try_from(-at).unwrap_or(usize::MAX))),
        }
    }
}

/// A frame on the graph's clock that a take's start can fall before: a take
/// armed in the first moments of a session, placed earlier than it began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// The frame.
    At(Frame),
    /// Before the clock's origin by this many frames.
    BeforeOrigin(Samples),
}

/// Where a take that has started belongs: the first frame it holds, the terms
/// and the start they give. Metadata: nothing is shifted in the file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    first: Frame,
    terms: Terms,
    start: Start,
    start_beat: Beat,
}

impl Placement {
    /// The graph frame of the take's first frame: `F`.
    #[must_use]
    pub fn first_frame(&self) -> Frame {
        self.first
    }

    /// The terms.
    #[must_use]
    pub fn terms(&self) -> &Terms {
        &self.terms
    }

    /// Where the take belongs on the graph's clock.
    #[must_use]
    pub fn start(&self) -> Start {
        self.start
    }

    /// Where it belongs on the timeline: the beat the transport stood at, at
    /// [`start`](Self::start), taking the tempo map as it is now and the
    /// transport as rolling without a locate since it was armed.
    #[must_use]
    pub fn start_beat(&self) -> Beat {
        self.start_beat
    }
}

/// How a graph frame maps to a beat: the playhead at arming and the tempo.
#[derive(Debug)]
struct Anchor {
    beat: Beat,
    frame: Frame,
    map: Arc<TempoMap>,
    rate: SampleRate,
}

impl Anchor {
    /// The frames from the anchor to `frame`, signed.
    fn since(&self, frame: Frame) -> i128 {
        i128::from(frame.get()) - i128::from(self.frame.get())
    }

    fn beat_at(&self, start: Start) -> Beat {
        let delta = match start {
            Start::At(frame) => self.since(frame),
            Start::BeforeOrigin(n) => -(n.get() as i128) - i128::from(self.frame.get()),
        };
        let now = self.map.seconds_at(self.beat).get();
        self.map
            .beat_at_seconds(Seconds(now + delta as f64 / self.rate.get()))
    }

    /// The graph frame the transport reaches at `beat` if it rolls on from the
    /// playhead, or `None` when the beat is already behind it.
    fn frame_of(&self, beat: Beat) -> Option<Frame> {
        if beat.get().partial_cmp(&self.beat.get()) != Some(std::cmp::Ordering::Greater) {
            return None;
        }
        let ahead = (self.map.seconds_at(beat).get() - self.map.seconds_at(self.beat).get())
            * self.rate.get();
        let frames = first_frame_at_or_after(ahead).max(0) as u64;
        Some(self.frame + Samples(usize::try_from(frames).unwrap_or(usize::MAX)))
    }
}

/// What a take sends the session as it runs.
#[derive(Debug)]
enum Started {
    /// The first frame is in.
    Placed(Placement),
}

/// What the pump shares with the take's handle.
#[derive(Debug, Default)]
struct Shared {
    placement: OnceLock<Placement>,
}

/// A recording that has ended: what it holds and where it belongs.
#[derive(Clone, Debug, PartialEq)]
pub struct Recorded {
    path: Option<PathBuf>,
    placement: Placement,
    frames: Samples,
    lost: Samples,
}

impl Recorded {
    /// The file the take was written to; `None` for a sink of the caller's.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Where it belongs.
    #[must_use]
    pub fn placement(&self) -> &Placement {
        &self.placement
    }

    /// The terms of the placement.
    #[must_use]
    pub fn terms(&self) -> &Terms {
        &self.placement.terms
    }

    /// Where it belongs on the graph's clock ([`Placement::start`]).
    #[must_use]
    pub fn start(&self) -> Start {
        self.placement.start
    }

    /// The frames written, the silence over any gap included.
    #[must_use]
    pub fn frames(&self) -> Samples {
        self.frames
    }

    /// The frames the tap lost and the take filled with silence: zero means
    /// the file is the whole take.
    #[must_use]
    pub fn lost(&self) -> Samples {
        self.lost
    }

    /// The take as a clip that starts where it belongs
    /// ([`Placement::start_beat`]): add it to the graph, in place of the input
    /// if you like (`graph.replace(input.node(), done.clip()?)`), and a bounce
    /// can fork it.
    ///
    /// # Errors
    ///
    /// [`TakeError::NoFile`] for a take written to a sink of the caller's;
    /// [`Error::Decode`](crate::Error::Decode) when the file cannot be read
    /// back (a format whose decoder is not built in);
    /// [`Error::Place`](crate::Error::Place) for a wave the sampler cannot
    /// place.
    #[cfg(feature = "sampler")]
    pub fn clip(&self) -> crate::Result<tutti_sampler::Clip> {
        let path = self.path().ok_or(TakeError::NoFile)?;
        let wave = tutti_codec::Wave::read(path)?;
        Ok(tutti_sampler::Clip::new(wave)?.at(self.placement.start_beat))
    }
}

/// What a take reports to [`Session::update`] ([`SessionEvent::Take`]).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum TakeEvent {
    /// The first frame of the take arrived (the punch-in was reached).
    Started {
        /// The take.
        id: TakeId,
        /// Where it belongs.
        placement: Placement,
    },
    /// The take ended, by its punch-out or by [`Take::finish`], and its sink
    /// closed. Reported once.
    Finished {
        /// The take.
        id: TakeId,
        /// What it holds and where it belongs.
        recorded: Recorded,
    },
    /// The take failed: its sink, its job, or a transport it never reached.
    /// Reported once, instead of [`Finished`](Self::Finished).
    Failed {
        /// The take.
        id: TakeId,
        /// Why.
        error: TakeError,
    },
}

/// Why a take could not be armed or did not end cleanly. Convertible into the
/// session's [`Error`](crate::Error) (`Error::Take`).
#[derive(Clone, Debug, Error)]
#[non_exhaustive]
pub enum TakeError {
    /// The input is not one this session holds.
    #[error("{0:?} is not an input of this session")]
    NotAnInput(NodeKey),
    /// The input's inlet node, or a port of it, is not in the graph.
    #[error(transparent)]
    Tap(#[from] MissingSource),
    /// The punch cannot be: its end is not after its start.
    #[error(transparent)]
    Punch(#[from] PunchError),
    /// A beat punch while the transport is stopped: the frame it would reach
    /// is not known until it rolls. Punch at a frame, or roll first.
    #[error("a punch at {0:?} needs a rolling transport to know its frame")]
    TransportStopped(Beat),
    /// The punch-out is already past: there is nothing to record.
    #[error("the punch ends at {end}, before the graph's frame {now}")]
    Past {
        /// The first frame the take would not keep.
        end: Frame,
        /// The graph frame the playhead stood at.
        now: Frame,
    },
    /// The sink's width is not the input's.
    #[error("the take's sink is {sink:?} wide but the input is {input:?}")]
    Width {
        /// The input's.
        input: ChannelLayout,
        /// The sink's.
        sink: ChannelLayout,
    },
    /// The file could not be created.
    #[error("the take's file could not be created: {0}")]
    Open(#[source] Arc<tutti_codec::Error>),
    /// The sink failed: a write, or closing it (for a WAV, the header is then
    /// left unpatched and the file unreadable). What was written before the
    /// failure is in the sink.
    #[error("the take's sink failed: {0}")]
    Sink(#[source] Arc<std::io::Error>),
    /// The sink refused a write, yet its `finalize` reported no error: it broke
    /// [`Sink`]'s contract, which has `finalize` report the first failure.
    #[error("the sink refused a write and its finalize reported no error")]
    SinkRefused,
    /// The take's job panicked or was dropped by the spawner.
    #[error("the take's job did not finish: {0}")]
    Job(#[from] JobFailed),
    /// The take was finished (or its handle dropped) before its punch-in was
    /// reached: it holds nothing.
    #[error("the take ended before its punch-in")]
    NeverStarted,
    /// [`Take::finish`] found no result: the take's job ended without one
    /// (the update reports why as [`TakeEvent::Failed`]).
    #[error("the take ended without a result")]
    NoResult,
    /// A take written to a sink of the caller's has no file to make a clip
    /// from.
    #[error("the take was not written to a file")]
    NoFile,
}

/// A take that is in flight: [`Armed`] or [`Rolling`].
pub trait InFlight: sealed::Sealed {}

mod sealed {
    pub trait Sealed {}
}

/// The state of a take whose punch-in has not been seen.
#[derive(Clone, Copy, Debug)]
pub enum Armed {}

/// The state of a take whose first frame has arrived.
#[derive(Clone, Copy, Debug)]
pub enum Rolling {}

impl sealed::Sealed for Armed {}
impl sealed::Sealed for Rolling {}
impl InFlight for Armed {}
impl InFlight for Rolling {}

/// Stops the pump when the last handle of the take goes.
#[derive(Debug)]
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// A take in flight, in the state `S`: [`Armed`] until its first frame arrives
/// ([`rolling`](Take::rolling)), [`Rolling`] after.
///
/// Both states [`finish`](Take::finish), which stops it at once; a take with a
/// punch-out ends by itself. **Dropping the handle stops the take too**, without
/// waiting: the pump closes its sink on the spawner and the end is still
/// reported ([`TakeEvent::Finished`]).
#[must_use = "dropping a take stops it; keep it while it records, or finish it"]
pub struct Take<S> {
    id: TakeId,
    window: Window,
    shared: Arc<Shared>,
    stop: StopOnDrop,
    result: Receiver<Result<Recorded, TakeError>>,
    state: std::marker::PhantomData<fn() -> S>,
}

impl<S> fmt::Debug for Take<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Take")
            .field("id", &self.id)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl<S> Take<S> {
    /// The take's id, the one its events carry.
    #[must_use]
    pub fn id(&self) -> TakeId {
        self.id
    }

    /// The frames the punch resolved to when the take was armed.
    #[must_use]
    pub fn window(&self) -> Window {
        self.window
    }

    fn into_state<T>(self) -> Take<T> {
        Take {
            id: self.id,
            window: self.window,
            shared: self.shared,
            stop: self.stop,
            result: self.result,
            state: std::marker::PhantomData,
        }
    }
}

impl Take<Armed> {
    /// This take as [`Rolling`] once its first frame has arrived; the armed
    /// take back, unchanged, before.
    ///
    /// # Errors
    ///
    /// The take itself, while it is still waiting for its punch-in.
    pub fn rolling(self) -> Result<Take<Rolling>, Self> {
        if self.shared.placement.get().is_some() {
            Ok(self.into_state())
        } else {
            Err(self)
        }
    }
}

impl Take<Rolling> {
    /// Where the take belongs: known once it is rolling.
    ///
    /// # Panics
    ///
    /// Never: a rolling take has its placement.
    #[must_use]
    pub fn placement(&self) -> &Placement {
        self.shared
            .placement
            .get()
            .expect("a rolling take has its placement")
    }
}

impl<S: InFlight> Take<S> {
    /// Stops the take at once, waits for its sink to close and returns what it
    /// holds. A take with a punch-out that already ended returns what it
    /// recorded.
    ///
    /// Blocks for at most one pump pass. The end is also reported to
    /// [`Session::update`] ([`TakeEvent::Finished`]).
    ///
    /// # Errors
    ///
    /// [`TakeError::Sink`] when the sink failed, [`TakeError::NoResult`] when
    /// the job ended without a result.
    #[must_use = "the result says whether the take closed cleanly"]
    pub fn finish(self) -> Result<Recorded, TakeError> {
        self.stop.0.store(true, Ordering::Release);
        self.result.recv().unwrap_or(Err(TakeError::NoResult))
    }
}

impl Session {
    /// Arms a take on `input`: opens a plan tap on its inlet node and runs the
    /// pump into `target` on the session's spawner. The take records what the
    /// graph is given by the input, so it is on the graph's clock whatever the
    /// device does; the module docs say where it is placed.
    ///
    /// The take's pump holds one spawner job for the whole take: with a custom
    /// [`spawn_with`](crate::SessionBuilder::spawn_with) over a bounded pool and
    /// no worker free, the take does not start, and [`Take::finish`] blocks
    /// until the job runs (the default thread-per-job spawner is unaffected).
    ///
    /// The tap compiles in the next [`update`](Self::update). The pump drops
    /// what comes before the punch-in; [`TakeEvent::Started`] lands in the
    /// update after its first frame, and [`TakeEvent::Finished`] or
    /// [`TakeEvent::Failed`] once at the end.
    ///
    /// # Errors
    ///
    /// [`TakeError::NotAnInput`] for a node that is not an input of this
    /// session; [`TakeError::Tap`] when it has left the graph;
    /// [`TakeError::TransportStopped`], [`TakeError::Punch`] and
    /// [`TakeError::Past`] for a punch that cannot be resolved;
    /// [`TakeError::Open`] and [`TakeError::Width`] for the target. Nothing is
    /// left open on an error.
    pub fn record(
        &mut self,
        input: &Input,
        punch: Punch,
        target: TakeTarget,
    ) -> Result<Take<Armed>, TakeError> {
        let node = input.node();
        let held = self
            .inputs
            .held(node)
            .ok_or(TakeError::NotAnInput(NodeKey::from(node)))?;
        let (lead, offset) = (held.lead, held.config.offset);
        let width = self
            .graph
            .node_outputs(node)
            .map_err(|missing| MissingSource::Node {
                node: missing.node(),
            })?;
        let layout = ChannelLayout::from_count(u16::try_from(width).unwrap_or(u16::MAX));

        let rate = self.graph.prepared().sample_rate();
        let playhead = self.playhead();
        let anchor = Anchor {
            beat: playhead.beat(),
            frame: playhead.frame(),
            map: self.transport.settings().tempo_map(),
            rate,
        };
        let window = resolve(&punch, &anchor, playhead.playing())?;

        let (sink, path) = open_target(target, layout, rate)?;

        let ports: Vec<_> = (0..width).map(|channel| node.out(channel)).collect();
        // A second of frames: the pump parks `IDLE_PARK` at a time.
        let capacity = Samples(rate.get().ceil() as usize);
        let reader = self.graph.taps().open(&ports, TapAlign::Raw, capacity)?;

        let id = TakeId(self.inputs.next_take());
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (done, result) = channel();
        let pump = Pump {
            reader,
            sink,
            path,
            window,
            terms: (lead, offset),
            probe: self.inputs.latency_probe(),
            anchor,
            shared: Arc::clone(&shared),
            stop: Arc::clone(&stop),
        };
        self.jobs.submit_stream(
            move |landings: &Landings<Started>| {
                let end = pump.run(landings);
                // `finish` may have gone: then nobody wants the copy.
                let _ = done.send(end.clone());
                end
            },
            move |_session, landed, events: &mut Vec<SessionEvent>| {
                events.push(SessionEvent::Take(match landed {
                    Landed::Message(Started::Placed(placement)) => {
                        TakeEvent::Started { id, placement }
                    }
                    Landed::Done(Ok(recorded)) => TakeEvent::Finished { id, recorded },
                    Landed::Done(Err(error)) => TakeEvent::Failed { id, error },
                    Landed::Failed(failed) => TakeEvent::Failed {
                        id,
                        error: failed.into(),
                    },
                }));
            },
        );
        Ok(Take {
            id,
            window,
            shared,
            stop: StopOnDrop(stop),
            result,
            state: std::marker::PhantomData,
        })
    }
}

/// The frames a punch means, against the playhead the take was armed at.
fn resolve(punch: &Punch, anchor: &Anchor, rolling: bool) -> Result<Window, TakeError> {
    let frame_of = |at: At| -> Result<Frame, TakeError> {
        match at {
            At::Frame(frame) => Ok(frame),
            At::NextBlock => Ok(anchor.frame),
            At::Beat(beat) if !rolling => Err(TakeError::TransportStopped(beat)),
            // Already behind the playhead: at once.
            At::Beat(beat) => Ok(anchor.frame_of(beat).unwrap_or(anchor.frame)),
        }
    };
    let start = frame_of(punch.start())?;
    let end = punch.end().map(frame_of).transpose()?;
    if let Some(end) = end {
        // A beat against a frame, compared now that both are frames.
        if end <= start {
            return Err(PunchError {
                at: punch.start(),
                until: punch.end().unwrap_or(At::NextBlock),
            }
            .into());
        }
        if end <= anchor.frame {
            return Err(TakeError::Past {
                end,
                now: anchor.frame,
            });
        }
    }
    Ok(Window { start, end })
}

/// The target as a sink at the input's width, and the file it writes if it is
/// one.
fn open_target(
    target: TakeTarget,
    layout: ChannelLayout,
    rate: SampleRate,
) -> Result<(Box<dyn DynSink>, Option<PathBuf>), TakeError> {
    match target.kind {
        TargetKind::File { path, format } => {
            let file = FileOut::create(&Spec::of_width(format, layout, rate), &path)
                .map_err(|e| TakeError::Open(Arc::new(e)))?;
            Ok((Box::new(file), Some(path)))
        }
        TargetKind::Sink(sink) => {
            if sink.layout() != layout {
                return Err(TakeError::Width {
                    input: layout,
                    sink: sink.layout(),
                });
            }
            Ok((sink, None))
        }
    }
}

/// The pump of one take: the tap's reader into the sink, between the
/// punch-in and the punch-out, on the graph's frames.
struct Pump {
    reader: TapReader,
    sink: Box<dyn DynSink>,
    path: Option<PathBuf>,
    window: Window,
    /// The inlet's lead and the input's trim.
    terms: (Samples, Trim),
    probe: LatencyProbe,
    anchor: Anchor,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
}

impl Pump {
    fn run(mut self, landings: &Landings<Started>) -> Result<Recorded, TakeError> {
        let layout = self.reader.layout();
        let width = usize::from(layout.count());
        let mut scratch = vec![0.0f32; PASS_FRAMES * width];
        let silence = vec![0.0f32; PASS_FRAMES * width];
        let mut placement: Option<Placement> = None;
        // The graph frame the next frame written belongs to.
        let mut cursor: Option<Frame> = None;
        let (mut written, mut lost) = (0usize, 0usize);
        let mut failed = false;

        'pump: while !self.stop.load(Ordering::Acquire) && !landings.is_closed() {
            let run = self.reader.contiguous().get().min(PASS_FRAMES);
            if run == 0 {
                std::thread::sleep(IDLE_PARK);
                continue;
            }
            let first = self.reader.next_frame();
            let popped = self.reader.pop_into(&mut scratch[..run * width]).get();
            let past = first + Samples(popped);
            // The window of this pass: [lo, hi).
            let lo = first.max(self.window.start);
            let hi = self.window.end.map_or(past, |end| past.min(end));
            let over = self.window.end.is_some_and(|end| first >= end);
            if !over && lo < hi {
                // A gap before this pass: the frames the tap lost, kept in
                // place as silence so the take stays aligned.
                if let Some(next) = cursor {
                    let gap = lo.since(next).map_or(0, Samples::get);
                    if !write_silence(&mut *self.sink, &silence, width, gap) {
                        failed = true;
                        break 'pump;
                    }
                    written += gap;
                    lost += gap;
                } else {
                    let found = self.place(lo);
                    placement = Some(found);
                    let _ = self.shared.placement.set(found);
                    // Never blocks; the session may be gone, and then there is
                    // no one to tell.
                    let _ = landings.send(Started::Placed(found));
                }
                let from = lo.since(first).map_or(0, Samples::get);
                let to = hi.since(first).map_or(0, Samples::get);
                if self.sink.write(&scratch[from * width..to * width]).is_err() {
                    failed = true;
                    break 'pump;
                }
                written += to - from;
                cursor = Some(hi);
            }
            if self.window.end.is_some_and(|end| past >= end) || over {
                break 'pump;
            }
        }
        // A punch-out the tap's gap straddled: silence up to it, so the take is
        // exactly the frames the punch names.
        if !failed
            && let (Some(end), Some(next)) = (self.window.end, cursor)
            && self.reader.next_frame() >= end
        {
            let gap = end.since(next).map_or(0, Samples::get);
            if write_silence(&mut *self.sink, &silence, width, gap) {
                written += gap;
                lost += gap;
            } else {
                failed = true;
            }
        }
        let closed = self.sink.finalize();
        if let Err(error) = closed {
            return Err(TakeError::Sink(Arc::new(error)));
        }
        if failed {
            return Err(TakeError::SinkRefused);
        }
        // A take finished before its punch-in has nothing to place.
        let placement = placement.ok_or(TakeError::NeverStarted)?;
        Ok(Recorded {
            path: self.path,
            placement,
            frames: Samples(written),
            lost: Samples(lost),
        })
    }

    /// The placement of a take whose first frame is `first`: the device's
    /// delay as it reports now.
    fn place(&self, first: Frame) -> Placement {
        let (in_latency, out_latency) = self.probe.read(self.anchor.rate);
        let (lead, offset) = self.terms;
        let terms = Terms::new(in_latency, out_latency, lead, offset);
        let start = terms.start(first);
        Placement {
            first,
            terms,
            start,
            start_beat: self.anchor.beat_at(start),
        }
    }
}

/// Writes `frames` frames of silence, a pass at a time. `false`: the sink
/// refused.
fn write_silence(sink: &mut dyn DynSink, silence: &[f32], width: usize, frames: usize) -> bool {
    let mut left = frames;
    while left > 0 {
        let now = left.min(silence.len() / width);
        if sink.write(&silence[..now * width]).is_err() {
            return false;
        }
        left -= now;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;
    use tutti_graph::TapId;
    use tutti_types::Bpm;

    /// A sink that keeps what it is written.
    struct Keep(Arc<Mutex<Vec<f32>>>);

    impl Sink for Keep {
        fn layout(&self) -> ChannelLayout {
            ChannelLayout::MONO
        }
        fn write(&mut self, interleaved: &[f32]) -> Result<(), SinkFailed> {
            self.0.lock().unwrap().extend_from_slice(interleaved);
            Ok(())
        }
        fn finalize(self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// **A gap the tap lost is silence in place, counted, and the take is
    /// exactly the frames of its window.** Frames 0..100 arrive, 100..150 are
    /// lost, 150..250 arrive: the take holds 250 frames, the lost 50 are zeros
    /// where they were, and the later frames are not shifted back.
    /// The formula itself, with a start before the clock's origin kept as such
    /// rather than clamped, and a latency not reported leaving `is_complete`
    /// false.
    #[test]
    fn a_start_before_the_origin_is_stated_not_clamped() {
        let terms = Terms::new(
            Some(Samples(240)),
            Some(Samples(480)),
            Samples(7),
            Trim::NONE,
        );
        assert_eq!(terms.start(Frame(1_000)), Start::At(Frame(273)));
        assert_eq!(terms.start(Frame(100)), Start::BeforeOrigin(Samples(627)));
        assert!(terms.is_complete());
        let partial = Terms::new(Some(Samples(240)), None, Samples(0), Trim::NONE);
        assert!(!partial.is_complete());
    }

    #[test]
    fn a_lost_gap_is_silence_in_place_and_counted() {
        let (reader, mut writer) = TapReader::pair(TapId(0), 1, Samples(4096));
        let ramp = |from: usize, to: usize| (from..to).map(|i| i as f32 + 1.0).collect::<Vec<_>>();
        writer.push_interleaved(&ramp(0, 100));
        writer.skip(50);
        writer.push_interleaved(&ramp(150, 250));

        let kept = Arc::new(Mutex::new(Vec::new()));
        let map = Arc::new(TempoMap::constant(Bpm::of(120.0)).unwrap());
        let pump = Pump {
            reader,
            sink: Box::new(Keep(Arc::clone(&kept))),
            path: None,
            window: Window {
                start: Frame(0),
                end: Some(Frame(250)),
            },
            terms: (Samples(0), Trim::NONE),
            probe: LatencyProbe::new(Arc::default(), None),
            anchor: Anchor {
                beat: Beat(0.0),
                frame: Frame(0),
                map,
                rate: SampleRate(48_000.0),
            },
            shared: Arc::default(),
            stop: Arc::default(),
        };
        let mut session = Session::builder()
            .build_detached(SampleRate(48_000.0))
            .unwrap();
        let ended = Arc::new(Mutex::new(None));
        let into = Arc::clone(&ended);
        session.jobs.submit_stream(
            move |landings: &Landings<Started>| pump.run(landings),
            move |_, landed, _| {
                if let Landed::Done(end) = landed {
                    *into.lock().unwrap() = Some(end);
                }
            },
        );
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while ended.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the pump never ended");
            let _ = session.update();
            std::thread::yield_now();
        }
        let recorded = ended
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .expect("closed cleanly");
        let kept = kept.lock().unwrap();
        assert_eq!(kept.len(), 250);
        assert_eq!(recorded.lost(), Samples(50));
        assert_eq!(recorded.frames(), Samples(250));
        assert_eq!(kept[99], 100.0);
        assert!(
            kept[100..150].iter().all(|&s| s == 0.0),
            "the gap is silence"
        );
        assert_eq!(kept[150], 151.0, "later frames are not shifted back");
        assert_eq!(kept[249], 250.0);
    }
}
