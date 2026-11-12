//! The session's bounce (feature `export`; doc 019, design 015 §4.3): the
//! request ([`Bounce`]), the job ([`BounceJob`]: gathered on the control
//! thread, built and rendered on any), and its return
//! ([`Session::finish_bounce`]), which drops what the render leaves on the
//! main thread and releases the bounce's token.
//!
//! A [`Bounce`] is data. Every figure it does not state is derived from the
//! session when the bounce starts and stored once (doc 019 §3): the range from
//! the transport's loop, the clock from the project's tempo map, the render
//! rate from the session's, the latency and the tail from the compiled graph,
//! the master's width from the root. Each has one explicit method.
//!
//! **One bounce at a time**: [`Session::bounce_start`] mints a bounce token, a
//! clone of an `Arc<()>` the session holds, which rides in the job and then
//! in what it returns ([`BounceRendered`]). A bounce is in flight while any
//! token lives, and another is refused ([`BounceError::InFlight`]). With
//! `plugin`, [`Phase::RenderMode`](crate::Phase::RenderMode) announces
//! Offline from that level: `bounce_start` runs the phase before it returns
//! (so a plugin hears Offline before the job can render a block),
//! `finish_bounce` drops the token and runs it again, and a job or its return
//! dropped unfinished (a cancelled task, a panic on the worker) drops its
//! token, so the next update restores Realtime. A host that may start the next
//! bounce before its next `Phase::RenderMode` finishes with
//! [`Session::finish_bounce_unannounced`] instead, so its plugins are not told
//! Realtime and then Offline again between two bounces.
//!
//! **Cancelling and watching.** A job carries a [`BounceHandle`]
//! ([`BounceJob::handle`]), cloned to whoever watches:
//! [`cancel`](BounceHandle::cancel) stops the render at the next point the
//! pipeline checks (between the session's steps, every block, between its
//! stages, and before each file is finalized and put in place; or before it
//! starts), and [`progress`](BounceHandle::progress) reads its stage and
//! fraction, [`Stage::Preparing`](tutti_export::Stage::Preparing) included (the session's steps, in the band
//! the pipeline leaves it). A cancelled job returns [`BounceError::Cancelled`]
//! from [`Session::finish_bounce`], drops its token when the render has
//! stopped, and leaves a file target's path as it was.
//!
//! **A token that never drops** (a job `mem::forget`ed, or kept unrun
//! forever) keeps the bounce in flight: the plugins stay Offline and every
//! later bounce is refused. [`Phase::Collect`](crate::Phase::Collect) warns,
//! once, when one has been in flight for [`BOUNCE_STALL_WARNING`] of its
//! `now`.
//!
//! **Leftovers drop on the main thread.** The rendered graph, what a failed
//! fork build had made, an edit that never ran: each is wrapped so that
//! wherever it is dropped, it is sent back to the session instead, and
//! dropped by `finish_bounce` or the next
//! [`Phase::Collect`](crate::Phase::Collect). A forked in-process VST2
//! plugin closes its instance when dropped, which VST2 confines to the main
//! thread. Something sent once the session is gone drops where it is.
//!
//! **The master's kind is a type.** [`Bounce::master`] and
//! [`Bounce::master_buffers`] each turn a bounce with no master
//! ([`NoMaster`]) into a [`FileMaster`] or a [`BufferMaster`] bounce, and only
//! one with a master starts ([`Master`]): a bounce with no master is not
//! writable. The kind rides through [`BounceJob`] and [`BounceRendered`] to
//! [`Bounced`], whose master is a [`Written`] for a file and a [`Rendered`]
//! for planes, with no `Option` to unwrap. A cancelled bounce is
//! [`BounceError::Cancelled`].

use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tutti_codec::Format;
use tutti_dsp::DitherShape;
use tutti_export::{
    BufferTarget, CancelToken, Control, EventLossPolicy, FileTarget, GRAPH_MAX_BLOCK, Job,
    JobError, Normalize, NormalizeSpec, Outcome, Output, Placement, Produced, Progress, Rendered,
    Resample, Target, Written,
};
use tutti_graph::{CommitError, Editor, MissingSource, NodeKey, OutPort, TapAlign, TapId};
use tutti_tempo::TempoMap;
use tutti_transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport, RenderClock};
use tutti_types::{Beat, ChannelLayout, ChannelTopology, Frame, SampleRate, Samples, Seconds};

use crate::Session;
use crate::fork::PendingExport;

/// The span of the project a bounce renders, in the unit the caller thinks in.
///
/// Beats go to frames through the project's tempo map at the render's rate
/// (doc 020, section 6: the first frame at or after each beat, so a step or a
/// ramp is followed); the span's length is the difference of the two frames.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Range {
    /// The transport's loop region, when one is set and armed
    /// ([`BounceConfigError::NoRange`] otherwise).
    Loop,
    /// From one beat to another.
    Beats(Beat, Beat),
    /// From one time to another, on the project's tempo map.
    Seconds(Seconds, Seconds),
    /// From one frame to another, counted from the project's start at the
    /// render's rate.
    Frames(Frame, Frame),
}

/// `Beat(0.0)..Beat(8.0)` is [`Range::Beats`]: the span from one beat to the
/// other, end exclusive as a Rust range is.
impl From<std::ops::Range<Beat>> for Range {
    fn from(r: std::ops::Range<Beat>) -> Self {
        Self::Beats(r.start, r.end)
    }
}

/// `Seconds(0.0)..Seconds(2.0)` is [`Range::Seconds`].
impl From<std::ops::Range<Seconds>> for Range {
    fn from(r: std::ops::Range<Seconds>) -> Self {
        Self::Seconds(r.start, r.end)
    }
}

/// `Frame(0)..Frame(48_000)` is [`Range::Frames`].
impl From<std::ops::Range<Frame>> for Range {
    fn from(r: std::ops::Range<Frame>) -> Self {
        Self::Frames(r.start, r.end)
    }
}

/// Which leading frames a bounce drops: the latency the graph's plan
/// compensates every output by (a look-ahead limiter's, a hosted plugin's) is
/// silence ahead of the programme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum BounceLatency {
    /// The compiled plan's total latency, asked of the graph that renders (after
    /// the edit). The default.
    #[default]
    Graph,
    /// Nothing: the file starts with the latency's silence, and keeps the
    /// programme's last frames ([`BounceTail`] adds the latency that is not trimmed).
    Keep,
    /// This many frames.
    Frames(Samples),
}

/// What a bounce renders past the programme's end: a reverb's decay, a delay's
/// repeats, a plugin's declared tail.
///
/// A tail excludes latency, so whatever of the graph's latency the bounce does
/// not trim ([`BounceLatency`]) is rendered on top of the tail whichever this is,
/// or the programme's last frames are cut.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum BounceTail {
    /// The tail the graph reports, by `GraphTail::resolve`'s rule against
    /// `cap`: a graph that never decays renders `cap`; otherwise what its nodes
    /// reported, at most `cap` (a node that said nothing counts as none). The
    /// default, with a cap of [`BounceTail::CAP`].
    Graph {
        /// The most it renders.
        cap: Seconds,
    },
    /// No tail.
    None,
    /// This many frames.
    Frames(Samples),
}

impl BounceTail {
    /// The default cap: ten seconds.
    pub const CAP: Seconds = Seconds(10.0);
}

impl Default for BounceTail {
    fn default() -> Self {
        Self::Graph { cap: Self::CAP }
    }
}

/// What a bounce needs and was not given ([`BounceConfigError::Missing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Field {
    /// The [`Format`] of a file target.
    Format,
}

/// A bounce that has not chosen its master yet: [`Bounce::project`]'s.
/// It does not start; [`Bounce::master`] or [`Bounce::master_buffers`] choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoMaster;

/// A master into a file ([`Bounce::master`]); [`Bounced::master`] is its
/// [`Written`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMaster(PathBuf);

/// A master into memory ([`Bounce::master_buffers`]); [`Bounced::master`] is
/// its [`Rendered`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferMaster;

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::FileMaster {}
    impl Sealed for super::BufferMaster {}
}

/// Where a started bounce's master goes: [`FileMaster`] or [`BufferMaster`]
/// (sealed). It names what the finished bounce holds for the master, so the
/// caller who chose a file is handed a [`Written`] and one who chose memory a
/// [`Rendered`].
pub trait Master: sealed::Sealed + Sized + 'static {
    /// What the master is once rendered.
    type Result: std::fmt::Debug + Send;

    /// The file's path, or `None` for memory.
    #[doc(hidden)]
    fn path(self) -> Option<PathBuf>;

    /// The master's result out of what the pipeline produced for it.
    #[doc(hidden)]
    fn result(produced: Produced) -> Self::Result;
}

impl Master for FileMaster {
    type Result = Written;

    fn path(self) -> Option<PathBuf> {
        Some(self.0)
    }

    fn result(produced: Produced) -> Written {
        match produced {
            Produced::File(written) => written,
            // The master's target was built from this kind alone.
            other => unreachable!("a file master produces a file, not {other:?}"),
        }
    }
}

impl Master for BufferMaster {
    type Result = Rendered;

    fn path(self) -> Option<PathBuf> {
        None
    }

    fn result(produced: Produced) -> Rendered {
        match produced {
            Produced::Buffers(rendered) => rendered,
            // The master's target was built from this kind alone.
            other => unreachable!("a buffer master produces planes, not {other:?}"),
        }
    }
}

/// A bounce: what to render and where it goes, with every figure it does not
/// name derived from the session when it starts. Start one with
/// [`Session::bounce`] or [`Session::bounce_start`].
///
/// ```no_run
/// # use tutti_session::{Bounce, Range, Session};
/// # use tutti_codec::{Format, Pcm};
/// # use tutti_graph::NodeKey;
/// # use tutti_types::Beat;
/// # fn go(session: &mut Session, drums: NodeKey) -> Result<(), tutti_session::BounceError> {
/// let done = session.bounce(
///     Bounce::project()
///         .format(Format::Wav(Pcm::Int24))
///         .range(Range::Beats(Beat(0.0), Beat(16.0)))
///         .master("out/master.wav")
///         .stem(drums, "out/drums.wav"),
/// )?;
/// # let _ = done;
/// # Ok(())
/// # }
/// ```
///
/// `N` names a stem's node: a [`NodeKey`] for the session, or a host's own
/// handle that [`try_map`](Self::try_map) resolves to one before the bounce
/// starts.
#[must_use = "a bounce renders nothing until the session starts it"]
pub struct Bounce<N = NodeKey, M = NoMaster> {
    format: Option<Format>,
    master: M,
    stems: Vec<Stem<N>>,
    normalize_stems: StemGain,
    range: Option<Range>,
    until: Option<Beat>,
    clock: Option<ExportClock>,
    sample_rate: Option<SampleRate>,
    resample: Option<Resample>,
    latency: BounceLatency,
    tail: BounceTail,
    root_topology: Option<Option<ChannelTopology>>,
    channels: Option<ChannelLayout>,
    file_topology: Option<ChannelTopology>,
    serial: bool,
    dither: Option<DitherShape>,
    on_event_loss: EventLossPolicy,
    normalize: Option<Normalize>,
    edit: Option<PrepareEdit>,
}

impl<N> Bounce<N, NoMaster> {
    /// A bounce of the project's mix, with nothing chosen: a master
    /// ([`master`](Self::master) or [`master_buffers`](Self::master_buffers),
    /// without which it does not start), a format for a file master, and a range when no loop is set. Name nodes as
    /// stems with [`stem`](Self::stem); `N` is what names one (a [`NodeKey`],
    /// or a host's own handle for it: [`try_map`](Self::try_map) resolves them).
    pub fn project() -> Self {
        Self {
            format: None,
            master: NoMaster,
            stems: Vec::new(),
            normalize_stems: StemGain::None,
            range: None,
            until: None,
            clock: None,
            sample_rate: None,
            resample: None,
            latency: BounceLatency::default(),
            tail: BounceTail::default(),
            root_topology: None,
            channels: None,
            file_topology: None,
            serial: false,
            dither: Some(DitherShape::Triangular),
            on_event_loss: EventLossPolicy::default(),
            normalize: None,
            edit: None,
        }
    }

    /// The master into a file at `path`: written beside it and renamed over it
    /// when the bounce finishes, so a cancelled or failed bounce leaves what
    /// was at the path as it was. [`Bounced::master`] is a [`Written`], and a
    /// [`format`](Self::format) is required.
    pub fn master(self, path: impl Into<PathBuf>) -> Bounce<N, FileMaster> {
        self.with_master(FileMaster(path.into()))
    }

    /// The master into memory, one plane per channel: [`Bounced::master`] is
    /// a [`Rendered`]. Nothing is encoded, resampled, dithered or normalized.
    pub fn master_buffers(self) -> Bounce<N, BufferMaster> {
        self.with_master(BufferMaster)
    }
}

impl<N, M> Bounce<N, M> {
    /// The same bounce with another master.
    fn with_master<M2>(self, master: M2) -> Bounce<N, M2> {
        let Self {
            format,
            master: _,
            stems,
            normalize_stems,
            range,
            until,
            clock,
            sample_rate,
            resample,
            latency,
            tail,
            root_topology,
            channels,
            file_topology,
            serial,
            dither,
            on_event_loss,
            normalize,
            edit,
        } = self;
        Bounce {
            format,
            master,
            stems,
            normalize_stems,
            range,
            until,
            clock,
            sample_rate,
            resample,
            latency,
            tail,
            root_topology,
            channels,
            file_topology,
            serial,
            dither,
            on_event_loss,
            normalize,
            edit,
        }
    }

    /// Also writes `node`'s own outputs into a file at `path`, in the same
    /// pass as the master: see [`Stem`] ([`stem_with`](Self::stem_with) for one
    /// that places its channels or states its own format).
    ///
    /// A stem is the node's output as it leaves the node, aligned with the
    /// master (what a bus or the master applies downstream is not in it), at
    /// the node's own width.
    pub fn stem(self, node: N, path: impl Into<PathBuf>) -> Self {
        self.stem_with(Stem::new(node, path))
    }

    /// Also writes the [`Stem`] it describes.
    pub fn stem_with(mut self, stem: Stem<N>) -> Self {
        self.stems.push(stem);
        self
    }

    /// How the stems are normalized: [`StemGain::None`] (as rendered) without
    /// it. The master's own normalize is [`normalize`](Self::normalize).
    pub fn normalize_stems(mut self, gain: StemGain) -> Self {
        self.normalize_stems = gain;
        self
    }

    /// The same bounce with each stem's node named by what `f` makes of it:
    /// a host that names nodes by its own handle resolves them to
    /// [`NodeKey`]s once, when the bounce starts. The first `Err` is returned
    /// and nothing is kept.
    ///
    /// # Errors
    ///
    /// What `f` returns for the first node it refuses.
    pub fn try_map<N2, E>(self, mut f: impl FnMut(N) -> Result<N2, E>) -> Result<Bounce<N2, M>, E> {
        let Self {
            format,
            master,
            stems,
            normalize_stems,
            range,
            until,
            clock,
            sample_rate,
            resample,
            latency,
            tail,
            root_topology,
            channels,
            file_topology,
            serial,
            dither,
            on_event_loss,
            normalize,
            edit,
        } = self;
        let stems = stems
            .into_iter()
            .map(|stem| stem.try_map(&mut f))
            .collect::<Result<Vec<_>, E>>()?;
        Ok(Bounce {
            format,
            master,
            stems,
            normalize_stems,
            range,
            until,
            clock,
            sample_rate,
            resample,
            latency,
            tail,
            root_topology,
            channels,
            file_topology,
            serial,
            dither,
            on_event_loss,
            normalize,
            edit,
        })
    }

    /// The file format of the master: required for a file master, and
    /// ignored by [`master_buffers`](Self::master_buffers).
    pub fn format(mut self, format: Format) -> Self {
        self.format = Some(format);
        self
    }

    /// The span to render. Without it the transport's loop is rendered, and
    /// without that [`until`](Self::until).
    pub fn range(mut self, range: impl Into<Range>) -> Self {
        self.range = Some(range.into());
        self
    }

    /// Where the project ends, for a project with no loop set: the span is
    /// from beat 0 to `end`. Nothing is guessed (a bounce with neither a loop,
    /// a range nor this is [`BounceConfigError::NoRange`]); a host that knows
    /// its clips' end names it here.
    pub fn until(mut self, end: Beat) -> Self {
        self.until = Some(end);
        self
    }

    /// The render's time. Without it, an offline timeline over the project's
    /// tempo map, started at the range's first beat with the loop off, so a
    /// transport-driven node reads what playback gives it. See [`ExportClock`].
    pub fn clock(mut self, clock: ExportClock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// The rate the graph is forked, prepared and rendered at; the session's
    /// without it.
    pub fn sample_rate(mut self, rate: impl Into<SampleRate>) -> Self {
        self.sample_rate = Some(rate.into());
        self
    }

    /// Writes the file at another rate than the render's.
    pub fn resample(mut self, resample: Resample) -> Self {
        self.resample = Some(resample);
        self
    }

    /// What to trim from the head; see [`BounceLatency`].
    pub fn latency(mut self, latency: BounceLatency) -> Self {
        self.latency = latency;
        self
    }

    /// What to render past the end; see [`BounceTail`].
    pub fn tail(mut self, tail: BounceTail) -> Self {
        self.tail = tail;
        self
    }

    /// What the graph's output channels are, in place of the session's declared
    /// root topology; `None` is the engine's order for the width.
    pub fn root_topology(mut self, root: Option<ChannelTopology>) -> Self {
        self.root_topology = Some(root);
        self
    }

    /// The master file's width. Without it, the root's: a wider graph folds
    /// onto a narrower file, never truncates, and a narrowing with no placement
    /// is an error ([`tutti_export::Error`]).
    pub fn channels(mut self, channels: ChannelLayout) -> Self {
        self.channels = Some(channels);
        self
    }

    /// What the master file's channels are (the WAV speaker mask, Ogg
    /// Vorbis's channel order); the engine's order for the width without it.
    /// Its width must be the file's.
    pub fn file_topology(mut self, topology: ChannelTopology) -> Self {
        self.file_topology = Some(topology);
        self
    }

    /// Renders on one thread, without the session's worker pool.
    pub fn serial(mut self) -> Self {
        self.serial = true;
        self
    }

    /// The dither applied when quantizing to an integer width: triangular
    /// without it. `None` adds none. A float width and Ogg Vorbis ignore it.
    pub fn dither(mut self, dither: Option<DitherShape>) -> Self {
        self.dither = dither;
        self
    }

    /// What the bounce does when the graph dropped an event: fails, without
    /// this.
    pub fn on_event_loss(mut self, policy: EventLossPolicy) -> Self {
        self.on_event_loss = policy;
        self
    }

    /// Normalizes the master file to `normalize`: the render is kept, measured
    /// and written with the gain, never rendered twice. A file target only
    /// ([`BounceConfigError::NormalizeBuffers`]).
    pub fn normalize(mut self, normalize: Normalize) -> Self {
        self.normalize = Some(normalize);
        self
    }

    /// Edits the forked graph before it renders: see [`PrepareEdit`].
    pub fn edit(mut self, edit: PrepareEdit) -> Self {
        self.edit = Some(edit);
        self
    }
}

impl<N: std::fmt::Debug, M: std::fmt::Debug> std::fmt::Debug for Bounce<N, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bounce")
            .field("format", &self.format)
            .field("master", &self.master)
            .field("stems", &self.stems)
            .field("normalize_stems", &self.normalize_stems)
            .field("range", &self.range)
            .field("until", &self.until)
            .field("clock", &self.clock)
            .field("sample_rate", &self.sample_rate)
            .field("resample", &self.resample)
            .field("latency", &self.latency)
            .field("tail", &self.tail)
            .field("root_topology", &self.root_topology)
            .field("channels", &self.channels)
            .field("file_topology", &self.file_topology)
            .field("serial", &self.serial)
            .field("dither", &self.dither)
            .field("on_event_loss", &self.on_event_loss)
            .field("normalize", &self.normalize)
            .field("edit", &self.edit.is_some())
            .finish()
    }
}

/// Where a stem goes.
#[derive(Debug, Clone, PartialEq)]
enum StemTo {
    File(PathBuf),
    Buffers,
}

/// One node's own output, written beside the master in the same pass (doc 019
/// section 7): `N` names the node ([`NodeKey`] by default).
///
/// A stem is what the node produces, read where it leaves the node and
/// delayed to the time the master leaves the plan (so every stem and the
/// master are sample-aligned and trimmed alike); what the buses and the
/// master apply downstream of the node is not in it. It is the node's whole
/// output, in port order, at the node's own width, unless it says
/// otherwise:
///
/// - [`channels`](Self::channels) taps some of the node's output channels,
///   in the order given;
/// - [`topology`](Self::topology) states what the tapped channels are (the
///   engine's order for the width without it);
/// - [`layout`](Self::layout) is the file's width, when it is not the tapped
///   one: the channels are folded onto it from their topology, and a fold
///   that cannot place them refuses the bounce when it renders (nothing is
///   truncated);
/// - [`format`](Self::format) is the file's format, in place of the bounce's.
///
/// The master's root declaration is never applied to a stem: it describes the
/// graph's output bus, not a node's output.
#[derive(Debug, Clone, PartialEq)]
pub struct Stem<N = NodeKey> {
    node: N,
    to: StemTo,
    channels: Option<Vec<u16>>,
    layout: Option<ChannelLayout>,
    topology: Option<ChannelTopology>,
    format: Option<Format>,
}

impl<N> Stem<N> {
    /// `node`'s output into a file at `path`: written beside it and renamed
    /// over it when the bounce finishes, as the master is.
    pub fn new(node: N, path: impl Into<PathBuf>) -> Self {
        Self::to(node, StemTo::File(path.into()))
    }

    /// `node`'s output into memory ([`Bounced::stems`] is
    /// [`Produced::Buffers`]), one plane per channel, at the render's rate:
    /// nothing is encoded, resampled, dithered or normalized, as for
    /// [`Bounce::master_buffers`].
    pub fn buffers(node: N) -> Self {
        Self::to(node, StemTo::Buffers)
    }

    fn to(node: N, to: StemTo) -> Self {
        Self {
            node,
            to,
            channels: None,
            layout: None,
            topology: None,
            format: None,
        }
    }

    /// Taps only these output channels of the node, in this order, in place
    /// of all of them. A channel the node has not is
    /// [`BounceConfigError::StemChannelMissing`].
    pub fn channels(mut self, channels: impl IntoIterator<Item = u16>) -> Self {
        self.channels = Some(channels.into_iter().collect());
        self
    }

    /// What the tapped channels are, in lane order. Its width must be the
    /// number tapped (a stem whose width differs is refused when it renders).
    pub fn topology(mut self, topology: ChannelTopology) -> Self {
        self.topology = Some(topology);
        self
    }

    /// The file's width, when it is not the tapped one.
    pub fn layout(mut self, layout: ChannelLayout) -> Self {
        self.layout = Some(layout);
        self
    }

    /// The file's format, in place of the bounce's (ignored by
    /// [`buffers`](Self::buffers)).
    pub fn format(mut self, format: Format) -> Self {
        self.format = Some(format);
        self
    }

    /// The node this stem names.
    #[must_use]
    pub fn node(&self) -> &N {
        &self.node
    }

    /// The same stem with its node named by what `f` makes of it.
    ///
    /// # Errors
    ///
    /// What `f` returns.
    pub fn try_map<M, E>(self, f: impl FnOnce(N) -> Result<M, E>) -> Result<Stem<M>, E> {
        let Self {
            node,
            to,
            channels,
            layout,
            topology,
            format,
        } = self;
        Ok(Stem {
            node: f(node)?,
            to,
            channels,
            layout,
            topology,
            format,
        })
    }
}

/// How a bounce's stems are normalized ([`Bounce::normalize_stems`]). A stem
/// held in memory is never normalized, so a bounce with one and a gain other
/// than [`None`](Self::None) is [`BounceConfigError::NormalizeBuffers`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub enum StemGain {
    /// As rendered: what a DAW that adds its own gain wants.
    #[default]
    None,
    /// Each stem takes the gain the master was measured to need
    /// ([`Bounce::normalize`], required), so stems that sum to the master
    /// still do. Right only while the master is the plain sum of the stems;
    /// with processing on the master bus, the sum is the caller's to keep.
    Shared,
    /// Each stem is measured and normalized on its own. Breaks the balance
    /// between them, by design: independent stem libraries.
    Each(Normalize),
}

/// What a finished bounce produced: the master's result, which is what the
/// bounce's master kind says it is ([`Written`] for [`FileMaster`],
/// [`Rendered`] for [`BufferMaster`]), and one [`Produced`] per stem in the
/// order the bounce named them.
#[derive(Debug)]
pub struct Bounced<M: Master> {
    master: M::Result,
    stems: Vec<Produced>,
}

impl<M: Master> Bounced<M> {
    /// The master's result.
    #[must_use]
    pub fn master(&self) -> &M::Result {
        &self.master
    }

    /// The stems' results, in the order the bounce named them.
    #[must_use]
    pub fn stems(&self) -> &[Produced] {
        &self.stems
    }

    /// The master's result, owned.
    #[must_use]
    pub fn into_master(self) -> M::Result {
        self.master
    }

    /// The master's result and then each stem's, owned.
    #[must_use]
    pub fn into_parts(self) -> (M::Result, Vec<Produced>) {
        (self.master, self.stems)
    }

    /// The pipeline's outputs, the master first.
    fn new(produced: Vec<Produced>) -> Self {
        let mut produced = produced.into_iter();
        let master = produced
            .next()
            .expect("a bounce renders its master as output 0");
        Self {
            master: M::result(master),
            stems: produced.collect(),
        }
    }
}

/// The time an export renders in: the clock the renderer advances and the
/// timeline every transport-aware node reads, as **one** value, so a render
/// cannot be handed one and its nodes another.
///
/// - [`frozen`](Self::frozen): no musical time (an effect tail, a synth
///   patch, a test tone). The renderer's transport is stopped at beat 0.
/// - [`timeline`](Self::timeline): an `OfflineTimeline` (or any clock that
///   is also an `OfflineClock`: never the live transport), seeded at the
///   tempo and beat to render from, at the render's rate.
#[derive(Clone)]
pub struct ExportClock(Clock);

#[derive(Clone)]
enum Clock {
    Frozen,
    Timeline {
        clock: Arc<dyn RenderClock>,
        timeline: OfflineTransport,
    },
}

impl ExportClock {
    /// No musical time; see the type docs.
    pub fn frozen() -> Self {
        Self(Clock::Frozen)
    }

    /// Renders against `timeline`: the renderer advances it, and every
    /// node reads its block's transport from it through `Env`.
    pub fn timeline<T>(timeline: Arc<T>) -> Self
    where
        T: RenderClock + tutti_transport::OfflineClock + 'static,
    {
        Self(Clock::Timeline {
            clock: timeline.clone() as Arc<dyn RenderClock>,
            timeline: OfflineTransport::new(timeline),
        })
    }

    /// What the renderer advances.
    fn render_clock(&self) -> Arc<dyn RenderClock> {
        match &self.0 {
            Clock::Frozen => Arc::new(tutti_transport::FrozenClock),
            Clock::Timeline { clock, .. } => Arc::clone(clock),
        }
    }

    /// The render's timeline, as [`PreparedGraph::ctx`] hands it.
    fn offline(&self) -> OfflineTransport {
        match &self.0 {
            Clock::Frozen => OfflineTransport::new(Arc::new(Stopped(Arc::new(
                TempoMap::constant(tutti_types::Bpm::default()).expect("120 BPM is a map"),
            )))),
            Clock::Timeline { timeline, .. } => timeline.clone(),
        }
    }
}

impl std::fmt::Debug for ExportClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Clock::Frozen => f.write_str("ExportClock::Frozen"),
            Clock::Timeline { .. } => f.write_str("ExportClock::Timeline(..)"),
        }
    }
}

/// [`ExportClock::frozen`]'s timeline: stopped at beat 0, what
/// `FrozenClock` hands the renderer. A stopped transport's tempo is never
/// read; its map is the engine's default, 120 BPM.
struct Stopped(Arc<TempoMap>);

/// The frozen render's timeline: only the render reads it.
impl tutti_transport::OfflineClock for Stopped {}

impl tutti_transport::Timeline for Stopped {
    fn beat(&self) -> tutti_types::Beat {
        tutti_types::Beat(0.0)
    }
    fn tempo_map(&self) -> Arc<TempoMap> {
        Arc::clone(&self.0)
    }
    fn is_rolling(&self) -> bool {
        false
    }
    fn segment_generation(&self) -> u64 {
        // Never moves, so never jumps.
        0
    }
}

/// What a bounce refuses that no render would: a request that names no
/// target, format or span, a graph with nothing to render, and an edit that
/// left the fork unusable. The pipeline's own refusals are
/// [`tutti_export::Error`] ([`BounceError::Export`]).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum BounceConfigError {
    /// The bounce does not say something it needs.
    #[error("the bounce names no {0:?}")]
    Missing(Field),
    /// The graph has no outputs to render.
    #[error("the graph has no outputs to render")]
    GraphHasNoOutputs,
    /// A stem names a node that is not in the graph.
    #[error("the stem's node {node:?} is not in the graph")]
    StemNotInGraph {
        /// The node.
        node: NodeKey,
    },
    /// A stem names a node no output reaches: the fork holds exactly what the
    /// outputs hear, so it renders nothing there.
    #[error("the stem's node {node:?} is not heard by any output")]
    StemNotAudible {
        /// The node.
        node: NodeKey,
    },
    /// A stem taps an output channel its node has not (a node with no audio
    /// outputs has none to tap, `channel` 0), or taps none.
    #[error("the stem's node {node:?} has no output channel {channel}")]
    StemChannelMissing {
        /// The node.
        node: NodeKey,
        /// The channel asked for.
        channel: u16,
    },
    /// Two targets name one path.
    #[error("two targets write {0:?}")]
    DuplicatePath(PathBuf),
    /// [`StemGain::Shared`] takes the master's gain, and the bounce does not
    /// normalize its master ([`Bounce::normalize`]).
    #[error("the stems take the master's gain, and the master is not normalized")]
    SharedGainWithoutMaster,
    /// There is no span to render: no loop is set and the bounce names neither
    /// a range nor an end ([`Bounce::range`], [`Bounce::until`]). Nothing is
    /// guessed.
    #[error("no range: the transport has no loop and the bounce names no range or end")]
    NoRange,
    /// The range keeps no frame: it ends where it begins or before, or is
    /// shorter than a frame at the render's rate.
    #[error("the range is empty at the render's rate")]
    EmptyRange,
    /// The range is not a span of the project: a NaN, an infinity or a
    /// position before the start.
    #[error("the range is not a span of the project")]
    InvalidRange,
    /// [`Job::new`] refused the render: a rate that is not finite and
    /// positive, or more frames than [`tutti_export::MAX_FRAMES`].
    #[error("the render cannot be a job: {0}")]
    Job(JobError),
    /// A normalize was asked of a target held in memory (the master, or a
    /// stem under a [`StemGain`] other than `None`), which is not measured:
    /// ask a file, or apply a gain to the planes.
    #[error("a target held in memory cannot be normalized")]
    NormalizeBuffers,
    /// A prepare hook left the forked graph in a state that cannot commit.
    #[error("the prepare hook left the forked graph uncommittable: {0:?}")]
    PrepareHookUncommittable(CommitError),
    /// A prepare hook replaced the fork's editor with another graph's: it no
    /// longer feeds the fork's executor.
    #[error("the prepare hook replaced the forked graph's editor")]
    PrepareHookUnpaired,
}

/// Why a bounce could not start or finish.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BounceError {
    /// Another bounce is in flight: its job, or what the job returned, is
    /// still alive. Finish or drop it first.
    #[error("another bounce is in flight")]
    InFlight,
    /// The bounce asks for something no render could give.
    #[error("bounce: {0}")]
    Config(#[from] BounceConfigError),
    /// A node the fork needs cannot be forked (it has no fork source), named by
    /// its key.
    #[error("bounce: {node:?} cannot be forked")]
    NotForkable {
        /// The node.
        node: NodeKey,
    },
    /// The fork was refused: a deferred fork source failed (naming its node),
    /// or the graph's shape did not allow one.
    #[error("bounce: the graph could not be forked: {0}")]
    Fork(tutti_graph::ForkError),
    /// The bounce was cancelled ([`BounceHandle::cancel`]): nothing was
    /// produced, and a file target is as it was.
    #[error("the bounce was cancelled")]
    Cancelled,
    /// The pipeline's: an I/O or encoder error, a fork that failed mid-render
    /// (naming its node's key), a lost event.
    #[error("bounce: {0}")]
    Export(tutti_export::Error),
}

impl From<tutti_export::Error> for BounceError {
    fn from(e: tutti_export::Error) -> Self {
        match e {
            tutti_export::Error::NotForkable { key } => Self::NotForkable { node: key },
            tutti_export::Error::Fork(e) => Self::Fork(e),
            other => Self::Export(other),
        }
    }
}

/// The graph an bounce is about to render, handed to its
/// [`PrepareEdit`] on the job's thread once the fork is built.
///
/// `editor` is the fork's: edit it (insert nodes, `spec_mut`); the job
/// commits whatever the edit leaves, and a commit the fork refuses fails the
/// bounce with the reason. A fork's units are on its executor by now, so a
/// unit already in it cannot be refilled: insert a new one. The fork's
/// executor, pool and root are the job's; the edit does not see them.
#[derive(Debug)]
pub struct PreparedGraph<'a> {
    /// The fork's editor.
    pub editor: &'a mut Editor,
    /// The render's timeline, which the renderer advances. A node in the
    /// graph reads it through its block's `Env`; this is for the edit
    /// itself (to place what it inserts, say).
    pub ctx: &'a OfflineTransport,
}

impl PreparedGraph<'_> {
    /// A key no node in any graph holds, for a node the edit inserts
    /// (`editor.insert(prepared.fresh_key(), ..)`), minted from
    /// [`NodeKey::fresh`]'s process-wide counter as the live graph's are.
    pub fn fresh_key(&self) -> NodeKey {
        NodeKey::fresh()
    }
}

/// An edit of an bounce's forked graph, made once the fork is built,
/// before the render.
///
/// **It runs on the job's thread**, and so do the `prepare` of every node
/// it inserts and the drop of whatever it captured once it has run. It must
/// not insert, prepare or capture a unit confined to the main thread (an
/// in-process VST2 plugin); such a node goes into the live graph, where the
/// fork's gather builds it on the main thread. An edit that never runs (the
/// build failed, the job was dropped) is dropped on the main thread.
pub type PrepareEdit = Box<dyn for<'a> FnOnce(PreparedGraph<'a>) + Send>;

/// How long a bounce may be in flight, on the `now` the session's
/// [`Phase::Collect`](crate::Phase::Collect) is given, before it warns that
/// its token may have leaked: an hour. Generous: an offline render of a
/// long project, with plugin forks that launch servers, takes minutes.
pub const BOUNCE_STALL_WARNING: Duration = Duration::from_secs(60 * 60);

/// The session's bounce state: the token every bounce holds a clone of,
/// the channel leftovers come back on, and the stall watch.
#[derive(Debug)]
pub(crate) struct BounceState {
    token: Arc<()>,
    send: Sender<Box<dyn Send>>,
    recv: Receiver<Box<dyn Send>>,
    /// The `now` the collect first saw the current bounce in flight.
    since: Option<Instant>,
    /// Whether the current bounce's stall was warned about.
    stall_warned: bool,
    /// How many stalls were warned about: what a test reads.
    #[cfg(test)]
    stall_warnings: usize,
}

impl Default for BounceState {
    fn default() -> Self {
        let (send, recv) = channel();
        Self {
            token: Arc::new(()),
            send,
            recv,
            since: None,
            stall_warned: false,
            #[cfg(test)]
            stall_warnings: 0,
        }
    }
}

impl BounceState {
    /// Whether an bounce token lives.
    pub(crate) fn in_flight(&self) -> bool {
        Arc::strong_count(&self.token) > 1
    }

    /// Watches for a leaked token at `now`: warns once when an bounce has
    /// been in flight (as the collects saw it) for [`BOUNCE_STALL_WARNING`].
    /// Its clock restarts when nothing is in flight, and when an bounce
    /// starts ([`restart_stall_watch`](Self::restart_stall_watch)).
    pub(crate) fn watch_stall(&mut self, now: Instant) {
        if !self.in_flight() {
            self.since = None;
            self.stall_warned = false;
            return;
        }
        let since = *self.since.get_or_insert(now);
        if !self.stall_warned && now.saturating_duration_since(since) >= BOUNCE_STALL_WARNING {
            self.stall_warned = true;
            #[cfg(test)]
            {
                self.stall_warnings += 1;
            }
            tracing::warn!(
                "an bounce has been in flight for over {} minutes: if its job or what it \
                 returned leaked (forgotten, or kept unrun), hosted plugins stay offline \
                 and every later bounce is refused (BounceInFlight)",
                BOUNCE_STALL_WARNING.as_secs() / 60
            );
        }
    }

    /// Restarts the stall clock for an bounce starting now. A host that
    /// hands one bounce's slot to the next within a frame
    /// ([`Session::finish_bounce_unannounced`], then [`Session::bounce`])
    /// never lets a collect see nothing in flight, so without this a queue
    /// of bounces each well inside the bound would be warned about.
    fn restart_stall_watch(&mut self) {
        self.since = None;
        self.stall_warned = false;
    }

    /// Drops, on this thread, every leftover sent back so far.
    pub(crate) fn drop_leftovers(&self) {
        while let Ok(value) = self.recv.try_recv() {
            drop(value);
        }
    }
}

/// `T`, sent back to the session wherever it is dropped, unless
/// [`take`](Self::take)n first.
struct ToMain<T: Send + 'static> {
    value: Option<T>,
    to: Sender<Box<dyn Send>>,
}

impl<T: Send + 'static> ToMain<T> {
    fn new(value: T, to: Sender<Box<dyn Send>>) -> Self {
        Self {
            value: Some(value),
            to,
        }
    }

    /// The value, no longer sent anywhere: its new owner drops it.
    fn take(mut self) -> T {
        self.value.take().expect("taken once")
    }

    /// The same value, its type forgotten, still sent back to be dropped on
    /// the main thread.
    fn erase(self) -> ToMain<Box<dyn Send>> {
        let to = self.to.clone();
        ToMain::new(Box::new(self.take()), to)
    }
}

impl<T: Send + 'static> std::ops::Deref for ToMain<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value.as_ref().expect("not taken")
    }
}

impl<T: Send + 'static> std::ops::DerefMut for ToMain<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value.as_mut().expect("not taken")
    }
}

impl<T: Send + 'static> Drop for ToMain<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            // A session already gone hands the value back, and it drops
            // here.
            let _ = self.to.send(Box::new(value));
        }
    }
}

/// A handle on a running bounce: stops it, and reads how far it is. Cheap
/// to clone, `Send` and `Sync`; any host reads the same reading, from any
/// thread, without waiting on the render.
///
/// It wraps the render's [`CancelToken`] and the latest [`Progress`]. The
/// render writes the reading when it delivers one (each of the session's
/// steps, every 4096 frames of a stage that counts them, and every stage
/// change) on its own thread and a host reads it on its own; neither is an
/// audio thread, so a lock is enough.
#[derive(Debug, Clone, Default)]
pub struct BounceHandle {
    cancel: CancelToken,
    progress: Arc<Mutex<Option<Progress>>>,
}

impl BounceHandle {
    /// Asks the render to stop. It does at the next point the session or the
    /// pipeline checks (between the session's steps, every block, between
    /// stages, during the measurement and the conversion, before each file is
    /// finalized and before it is put in place: doc 019 section 6.2), or
    /// before it starts for a job that has not run. It reports
    /// [`BounceError::Cancelled`] and drops the bounce's token; every temporary
    /// file is removed and the target path is untouched. A cancel after the
    /// file is in place is too late and the render is `Finished`.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// How far the render is: the latest reading delivered, or `None` before
    /// the first (the job has not begun to run). Its
    /// [`stage`](Progress::stage) says what the render is doing and its
    /// [`fraction`](Progress::fraction) how far through the whole of it, the
    /// pipeline's own weights.
    #[must_use]
    pub fn progress(&self) -> Option<Progress> {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stop_requested(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The render's callback: records the reading.
    fn record(&self, p: &Progress) {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner) = Some(*p);
    }
}

/// The figures a bounce derives from the session at its start, and the two it
/// derives from the graph that renders (the latency and the tail), which are
/// asked of it after the edit.
#[derive(Debug, Clone, Copy)]
struct Figures {
    rate: SampleRate,
    /// The audible span, in frames at `rate`.
    length: Samples,
    latency: BounceLatency,
    tail: BounceTail,
    on_event_loss: EventLossPolicy,
    /// What the buffers' target would be too large to hold, for the error
    /// of a span past [`tutti_export::MAX_FRAMES`]: its width.
    held_in_memory: Option<ChannelLayout>,
}

impl Figures {
    /// The [`Job`], from what `graph` reports now.
    fn job(&self, graph: &tutti_export::RenderGraph) -> Result<Job, BounceError> {
        let reported = graph.reported_latency();
        let latency = match self.latency {
            BounceLatency::Graph => reported,
            BounceLatency::Keep => Samples(0),
            BounceLatency::Frames(n) => n,
        };
        let tail = match self.tail {
            BounceTail::None => Samples(0),
            BounceTail::Frames(n) => n,
            // `resolve`, not `samples().unwrap_or(cap)`: a node that never
            // said (`Tail::Unknown`) is spent at what the others reported,
            // not at the cap, which would append silence.
            BounceTail::Graph { cap } => graph.reported_tail().resolve(cap.to_samples(self.rate)),
        };
        // A tail excludes latency: the programme leaves the graph its
        // reported latency late, so the part of that latency the render does
        // not trim is rendered too, or the programme's end is cut.
        let kept = reported.remaining_after(latency);
        self.provisional(latency, tail + kept)
    }

    /// The job of these counts, refused as the bounce reports it.
    fn provisional(&self, latency: Samples, tail: Samples) -> Result<Job, BounceError> {
        match Job::new(self.rate, self.length, latency, tail) {
            Ok(job) => Ok(job.with_event_loss(self.on_event_loss)),
            // Planes that cannot be held are the pipeline's error, as when
            // the allocation fails.
            Err(JobError::TooLong { total: Some(total) }) if self.held_in_memory.is_some() => {
                Err(tutti_export::Error::RenderTooLarge {
                    frames: total,
                    channels: self.held_in_memory.expect("checked"),
                }
                .into())
            }
            Err(e) => Err(BounceConfigError::Job(e).into()),
        }
    }
}

/// How many steps the session reports as [`Stage::Preparing`](tutti_export::Stage::Preparing): the gather
/// (done when the job is made), the fork's build, the edit, the commit and
/// assembly.
const PREPARING_STEPS: u32 = 4;

/// A bounce, gathered on the control thread: `Send`, and
/// [`run`](Self::run) builds the fork (a hosted plugin's fork launches a
/// `plugin-server` and loads its state), runs the edit, and renders, on
/// any thread. Hand what it returns to [`Session::finish_bounce`].
///
/// Holds the bounce's token: dropping it unrun ends the bounce (the next
/// update announces Realtime), and what it holds goes back to the session
/// to drop.
///
/// **Do not leak it.** A job (or its [`BounceRendered`]) that is
/// `mem::forget`ed, or kept unrun forever, keeps its token: the bounce
/// stays in flight, hosted plugins stay Offline, and every later
/// [`Session::bounce_start`] is refused with [`BounceError::InFlight`].
/// Nothing else reports it; the session warns once after
/// [`BOUNCE_STALL_WARNING`].
#[must_use = "a bounce renders nothing until it is run and finished"]
pub struct BounceJob<M: Master> {
    pending: ToMain<PendingExport>,
    edit: Option<ToMain<PrepareEdit>>,
    ctx: OfflineTransport,
    clock: Arc<dyn RenderClock>,
    master: Target,
    /// What each stem taps and where it goes, in order.
    stems: Vec<StemTap>,
    figures: Figures,
    drops: Sender<Box<dyn Send>>,
    token: Arc<()>,
    handle: BounceHandle,
    kind: PhantomData<fn() -> M>,
}

impl<M: Master> std::fmt::Debug for BounceJob<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BounceJob")
            .field("ctx", &self.ctx)
            .field("stems", &self.stems.len())
            .field("figures", &self.figures)
            .field("drops", &self.drops)
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl<M: Master> BounceJob<M> {
    /// The handle that cancels this job and reads its progress.
    #[must_use]
    pub fn handle(&self) -> BounceHandle {
        self.handle.clone()
    }

    /// Builds the fork, runs the edit, commits it, and renders. Any thread;
    /// it blocks for the whole render, or until [`BounceHandle::cancel`]
    /// stops it.
    pub fn run(self) -> BounceRendered<M> {
        let Self {
            pending,
            edit,
            ctx,
            clock,
            master,
            stems,
            figures,
            drops,
            token,
            handle,
            kind: _,
        } = self;
        // The band the render leaves for the session's steps depends on the
        // targets alone: the readers do not exist until the taps are open.
        let targets: Vec<Target> = std::iter::once(master.clone())
            .chain(stems.iter().map(|stem| stem.target.clone()))
            .collect();
        let record = &mut |p: &Progress| handle.record(p);
        let mut control = Control::new(&handle.cancel).on_progress(record);
        // The gather was the first step, made when the job was.
        control.preparing(&targets, 1, PREPARING_STEPS);
        if handle.stop_requested() {
            // Cancelled before it began: nothing is built, and what the job
            // holds goes back to the main thread.
            drop((pending, edit));
            return BounceRendered::cancelled(None, token);
        }
        let mut pair = match pending.take().build() {
            Ok(pair) => ToMain::new(pair, drops.clone()),
            Err((e, remains)) => {
                drop(ToMain::new(remains, drops));
                return BounceRendered::failed(e.into(), None, token);
            }
        };
        control.preparing(&targets, 2, PREPARING_STEPS);
        if handle.stop_requested() {
            // Cancelled while the fork was building (a plugin's server
            // launches here, half a second or more).
            drop(edit);
            return BounceRendered::cancelled(Some(pair.erase()), token);
        }
        if let Some(edit) = edit {
            (edit.take())(PreparedGraph {
                editor: pair.editor_mut(),
                ctx: &ctx,
            });
        }
        // One tap per stem, on the editor the commit below installs.
        let readers = match open_stem_taps(pair.editor_mut(), &stems) {
            Ok(readers) => readers,
            Err(e) => {
                return BounceRendered::failed(e.into(), Some(pair.erase()), token);
            }
        };
        control.preparing(&targets, 3, PREPARING_STEPS);
        // A fork's editor holds whatever the edit left, uncommitted: send
        // it, and apply it here so the render's first block runs it.
        if let Err(e) = pair.commit() {
            return BounceRendered::failed(
                BounceConfigError::PrepareHookUncommittable(e).into(),
                Some(pair.erase()),
                token,
            );
        }
        // The edit held `&mut Editor`, which can be swapped for another
        // graph's: seal only a pair that is still one, and keep an unpaired
        // one to drop on the main thread, where a pinned unit belongs.
        if !pair.is_paired() {
            return BounceRendered::failed(
                BounceConfigError::PrepareHookUnpaired.into(),
                Some(pair.erase()),
                token,
            );
        }
        let mut graph = match pair.take().assemble() {
            Ok(graph) => ToMain::new(graph, drops),
            // Not reached: the pair was just checked.
            Err(e) => return BounceRendered::failed(e.into(), None, token),
        };
        control.preparing(&targets, 4, PREPARING_STEPS);
        if handle.stop_requested() {
            return BounceRendered::cancelled(Some(graph.erase()), token);
        }
        // The graph's own figures, asked of the graph that renders: after the
        // edit, which may have changed it.
        let job = match figures.job(&graph) {
            Ok(job) => job,
            Err(e) => return BounceRendered::failed(e, Some(graph.erase()), token),
        };
        let outputs = std::iter::once(Output::master(master))
            .chain(
                readers
                    .into_iter()
                    .zip(stems)
                    .map(|(reader, stem)| Output::tap(reader, stem.placement, stem.target)),
            )
            .collect();
        let result = tutti_export::render(&mut graph, &job, clock.as_ref(), outputs, &mut control)
            .map_err(BounceError::from)
            .and_then(|outcome| match outcome {
                Outcome::Finished(produced) => Ok(Bounced::new(produced)),
                Outcome::Cancelled => Err(BounceError::Cancelled),
            });
        BounceRendered {
            result,
            graph: Some(graph.erase()),
            _token: token,
        }
    }
}

/// What one stem taps, and what its lanes are: the part of a [`Stem`] the
/// fork's editor needs to open its tap.
#[derive(Debug)]
struct StemTap {
    node: NodeKey,
    /// The node's output ports, one lane each.
    ports: Vec<u16>,
    placement: Placement,
    target: Target,
}

/// Opens one plan tap per stem on the fork's editor, aligned to the outputs'
/// time, each lane with the ring the pipeline's drain needs (`2 x` the
/// fork's block), and returns their readers in order. The commit that follows
/// installs them.
///
/// The ids are above any the edit hook opened. A node or channel the edit
/// removed since the start's check is refused here, by the same names.
fn open_stem_taps(
    editor: &mut Editor,
    stems: &[StemTap],
) -> Result<Vec<tutti_graph::TapReader>, BounceConfigError> {
    let mut next = editor
        .spec()
        .topology
        .taps
        .keys()
        .next_back()
        .map_or(0, |id| id.0 + 1);
    let capacity = Samples(2 * GRAPH_MAX_BLOCK.get());
    stems
        .iter()
        .map(|stem| {
            let sources = stem
                .ports
                .iter()
                .map(|&port| {
                    tutti_graph::Source::Node(OutPort {
                        node: stem.node,
                        port,
                    })
                })
                .collect();
            let id = TapId(next);
            next += 1;
            editor
                .open_tap(id, sources, TapAlign::ToOutput, capacity)
                .map_err(|missing| match missing {
                    MissingSource::Port { node, port, .. } => {
                        BounceConfigError::StemChannelMissing {
                            node,
                            channel: port,
                        }
                    }
                    MissingSource::Node { node } => BounceConfigError::StemNotInGraph { node },
                    // A stem's lanes are node ports, nothing else.
                    other => unreachable!("a stem taps node ports, not {other:?}"),
                })
        })
        .collect()
}

/// What a finished [`BounceJob`] returns: the result, the rendered graph
/// (dropped on the main thread by [`Session::finish_bounce`]), and the
/// bounce's token. `Send`.
#[must_use = "finish the bounce with `Session::finish_bounce` (or `finish_bounce_unannounced`)"]
pub struct BounceRendered<M: Master> {
    result: Result<Bounced<M>, BounceError>,
    /// The fork, sealed or not: whatever it holds is dropped on the main thread.
    graph: Option<ToMain<Box<dyn Send>>>,
    _token: Arc<()>,
}

impl<M: Master> BounceRendered<M> {
    fn cancelled(graph: Option<ToMain<Box<dyn Send>>>, token: Arc<()>) -> Self {
        Self {
            result: Err(BounceError::Cancelled),
            graph,
            _token: token,
        }
    }

    fn failed(e: BounceError, graph: Option<ToMain<Box<dyn Send>>>, token: Arc<()>) -> Self {
        Self {
            result: Err(e),
            graph,
            _token: token,
        }
    }
}

impl<M: Master> std::fmt::Debug for BounceRendered<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BounceRendered")
            .field(
                "result",
                &match &self.result {
                    Ok(b) => format!("Ok({} stems)", b.stems().len()),
                    Err(BounceError::Cancelled) => "Cancelled".to_string(),
                    Err(_) => "Err".to_string(),
                },
            )
            .field("_token", &self._token)
            .finish_non_exhaustive()
    }
}

/// The range a bounce renders: the one it names, else the transport's loop,
/// else from beat 0 to the end it names; never a guess.
fn chosen(
    range: Option<Range>,
    looped: Option<tutti_types::LoopRange>,
    until: Option<Beat>,
) -> Result<Range, BounceConfigError> {
    range
        .or_else(|| looped.map(|_| Range::Loop))
        .or_else(|| until.map(|end| Range::Beats(Beat(0.0), end)))
        .ok_or(BounceConfigError::NoRange)
}

/// The span `range` is on `map`: the beat the render starts at and its length
/// in frames at `rate`.
///
/// Beats go through the map's `frame_at_or_after`, as the walker places
/// them (doc 020, section 6), so a step or a ramp inside the span is the
/// length it has in playback.
fn span(
    range: Range,
    loop_range: Option<tutti_types::LoopRange>,
    map: &TempoMap,
    rate: SampleRate,
) -> Result<(Beat, Samples), BounceConfigError> {
    let range = match range {
        Range::Loop => {
            let looped = loop_range.ok_or(BounceConfigError::NoRange)?;
            Range::Beats(looped.start(), looped.end())
        }
        other => other,
    };
    let (start, frames) = match range {
        Range::Loop => unreachable!("replaced by the loop's beats above"),
        Range::Beats(a, b) => {
            if !a.get().is_finite() || !b.get().is_finite() || a.get() < 0.0 || b.get() < 0.0 {
                return Err(BounceConfigError::InvalidRange);
            }
            let (from, to) = (
                map.frame_at_or_after(a, rate),
                map.frame_at_or_after(b, rate),
            );
            (a, to.get().saturating_sub(from.get()))
        }
        Range::Seconds(a, b) => {
            if !a.get().is_finite() || !b.get().is_finite() || a.get() < 0.0 || b.get() < 0.0 {
                return Err(BounceConfigError::InvalidRange);
            }
            let long = Seconds((b.get() - a.get()).max(0.0)).to_samples(rate);
            (map.beat_at_seconds(a), long.get() as u64)
        }
        Range::Frames(a, b) => (map.beat_at_frame(a, rate), b.get().saturating_sub(a.get())),
    };
    match usize::try_from(frames) {
        Ok(0) => Err(BounceConfigError::EmptyRange),
        Ok(n) => Ok((start, Samples(n))),
        Err(_) => Err(BounceConfigError::Job(JobError::TooLong { total: None })),
    }
}

impl Session {
    /// Starts a bounce: checks what it names, derives what it does not,
    /// gathers its fork of the live graph here (what it reads of the live graph
    /// and its nodes, now; an in-process VST2 plugin's second instance is built
    /// here too), mints the bounce's token, and (with `plugin`) announces
    /// Offline to every hosted plugin before it returns.
    /// [`run`](BounceJob::run) the job on any thread, then
    /// [`finish_bounce`](Self::finish_bounce) what it returns.
    ///
    /// **No `update` is needed first.** The fork is the graph's spec as it
    /// stands at this call, so an insert, a wire or a mode set since the last
    /// [`update`](Self::update) is in it, committed or not (the live executor
    /// is not touched). What `update` derives from the session's own
    /// declarations (modulation lanes, a clip's node) reaches the graph in its
    /// phases, so a bounce after declaring those wants an `update` first.
    ///
    /// The fork renders what the graph is driven to play from the bounce's
    /// clock, from silence: every node isolated and reset, reading that
    /// clock through its `Env`; controls as they are now; a hosted plugin
    /// forked by state transfer (a fresh instance in a new `plugin-server`,
    /// loaded with the live one's state).
    ///
    /// # Errors
    ///
    /// [`BounceError::InFlight`] while another bounce's token lives;
    /// [`BounceError::Config`] for a request that names no format or
    /// range, whose range keeps no frame, or for a graph with no outputs (all
    /// before anything is forked); [`BounceError::NotForkable`] and
    /// [`BounceError::Fork`] for a node the fork cannot copy.
    pub fn bounce_start<M: Master>(
        &mut self,
        bounce: Bounce<NodeKey, M>,
    ) -> Result<BounceJob<M>, BounceError> {
        if self.bounce.in_flight() {
            return Err(BounceError::InFlight);
        }
        let Bounce {
            format,
            master,
            stems,
            normalize_stems,
            range,
            until,
            clock,
            sample_rate,
            resample,
            latency,
            tail,
            root_topology,
            channels,
            file_topology,
            serial,
            dither,
            on_event_loss,
            normalize,
            edit,
        } = bounce;
        if self.graph.outputs() == 0 {
            return Err(BounceConfigError::GraphHasNoOutputs.into());
        }
        // The width the master file has without being told: the root's.
        let outputs = self.graph.outputs();
        let rate = sample_rate.unwrap_or_else(|| self.transport.sample_rate());

        // The span: the range named, else the transport's loop, else the end
        // the bounce names; never a guess.
        let map = self.transport.settings().tempo_map();
        let looped = self.transport.motion().loop_region().active();
        let range = chosen(range, looped, until)?;
        let (start, length) = span(range, looped, &map, rate)?;

        let channels = channels.unwrap_or_else(|| ChannelLayout::clamped(outputs));
        let target: Target = match master.path() {
            Some(path) => {
                let format = format.ok_or(BounceConfigError::Missing(Field::Format))?;
                let file = FileTarget::new(path, format, channels)
                    .with_topology(file_topology)
                    .with_resample(resample)
                    .with_dither(dither)
                    .with_normalize(normalize.map(NormalizeSpec::own));
                file.into()
            }
            None => {
                if normalize.is_some() {
                    return Err(BounceConfigError::NormalizeBuffers.into());
                }
                let planes = BufferTarget::new(channels).with_topology(file_topology);
                planes.into()
            }
        };
        // The stems: what each taps and where it goes, refused by name before
        // anything is forked. The master is output 0, which `Shared` reads.
        let mut taps = Vec::with_capacity(stems.len());
        for stem in stems {
            let Stem {
                node,
                to,
                channels: tapped,
                layout,
                topology,
                format: own_format,
            } = stem;
            if !self.graph.contains(node) {
                return Err(BounceConfigError::StemNotInGraph { node }.into());
            }
            if !self.graph.reaches_outputs(node) {
                return Err(BounceConfigError::StemNotAudible { node }.into());
            }
            let width = self.graph.node_outputs(node);
            let ports = tapped.unwrap_or_else(|| (0..width as u16).collect());
            if ports.is_empty() {
                return Err(BounceConfigError::StemChannelMissing { node, channel: 0 }.into());
            }
            if let Some(&channel) = ports.iter().find(|&&c| usize::from(c) >= width) {
                return Err(BounceConfigError::StemChannelMissing { node, channel }.into());
            }
            let lanes = ChannelLayout::from_count(ports.len().try_into().unwrap_or(u16::MAX));
            let placement =
                Placement::new(topology.unwrap_or_else(|| ChannelTopology::for_width(lanes)));
            let layout = layout.unwrap_or(lanes);
            let target: Target = match to {
                StemTo::File(path) => {
                    let format = own_format
                        .or(format)
                        .ok_or(BounceConfigError::Missing(Field::Format))?;
                    let gain = match normalize_stems {
                        StemGain::None => None,
                        StemGain::Each(n) => Some(NormalizeSpec::own(n)),
                        StemGain::Shared => Some(NormalizeSpec::from_output(
                            normalize.ok_or(BounceConfigError::SharedGainWithoutMaster)?,
                            0,
                        )),
                    };
                    FileTarget::new(path, format, layout)
                        .with_resample(resample)
                        .with_dither(dither)
                        .with_normalize(gain)
                        .into()
                }
                StemTo::Buffers => {
                    if normalize_stems != StemGain::None {
                        return Err(BounceConfigError::NormalizeBuffers.into());
                    }
                    BufferTarget::new(layout).into()
                }
            };
            taps.push(StemTap {
                node,
                ports,
                placement,
                target,
            });
        }
        // Two files at one path would each rename over the other.
        let all = || std::iter::once(&target).chain(taps.iter().map(|stem| &stem.target));
        for (i, one) in all().enumerate() {
            if let Target::File(file) = one
                && all()
                    .take(i)
                    .any(|t| matches!(t, Target::File(f) if f.path() == file.path()))
            {
                return Err(BounceConfigError::DuplicatePath(file.path().to_path_buf()).into());
            }
        }
        // What a span past the frame limit would be too large to hold: the
        // first target kept in memory.
        let held_in_memory = all().find_map(|t| match t {
            Target::Buffers(b) => Some(b.channels()),
            _ => None,
        });
        let figures = Figures {
            rate,
            length,
            latency,
            tail,
            on_event_loss,
            held_in_memory,
        };
        // What the counts the bounce names make of the job, refused now rather
        // than after the fork is built; the graph's own figures are added
        // once it is.
        figures.provisional(
            match latency {
                BounceLatency::Frames(n) => n,
                BounceLatency::Graph | BounceLatency::Keep => Samples(0),
            },
            match tail {
                BounceTail::Frames(n) => n,
                BounceTail::Graph { .. } | BounceTail::None => Samples(0),
            },
        )?;

        // The render's time: the bounce's clock, or the project's tempo map
        // from the range's first beat with the loop off, so a transport-driven
        // node reads what playback gives it.
        let clock = clock.unwrap_or_else(|| {
            ExportClock::timeline(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
                start_beat: start,
                tempo_map: Arc::clone(&map),
                sample_rate: rate,
                loop_range: None,
            })))
        });
        // The edit hook's timeline: the same object the renderer advances, or
        // one stopped at beat 0 for a frozen clock.
        let ctx = clock.offline();
        let pending = match self.graph.gather_for_bounce(rate, serial, root_topology) {
            Ok(pending) => pending,
            Err(tutti_export::Error::Fork(
                tutti_graph::ForkError::NoOutputs { .. }
                | tutti_graph::ForkError::NoSuchNode { .. },
            )) => {
                return Err(BounceConfigError::GraphHasNoOutputs.into());
            }
            Err(e) => return Err(e.into()),
        };
        // A new bounce: its own stall clock, from its first collect.
        self.bounce.restart_stall_watch();
        let drops = self.bounce.send.clone();
        let job = BounceJob {
            pending: ToMain::new(pending, drops.clone()),
            edit: edit.map(|edit| ToMain::new(edit, drops.clone())),
            ctx,
            clock: clock.render_clock(),
            master: target,
            stems: taps,
            figures,
            drops,
            token: Arc::clone(&self.bounce.token),
            handle: BounceHandle::default(),
            kind: PhantomData,
        };
        // Offline is announced here, on the control thread, before the job
        // can render a block.
        #[cfg(feature = "plugin")]
        self.render_mode_phase();
        Ok(job)
    }

    /// Ends a bounce: drops the rendered graph (and every leftover sent
    /// back so far) here, releases the bounce's token and (with `plugin`)
    /// announces Realtime when no other bounce is in flight. Returns what
    /// the render produced.
    ///
    /// A main-thread call (design 015 §4.2).
    ///
    /// # Errors
    ///
    /// The render's [`BounceError`]: [`Cancelled`](BounceError::Cancelled) for
    /// a bounce its [`BounceHandle`] stopped, a failed fork build or render (a
    /// fork that failed mid-render names its node's key), an I/O or encoder
    /// error.
    pub fn finish_bounce<M: Master>(
        &mut self,
        rendered: BounceRendered<M>,
    ) -> Result<Bounced<M>, BounceError> {
        let result = self.finish_bounce_unannounced(rendered);
        #[cfg(feature = "plugin")]
        self.render_mode_phase();
        result
    }

    /// [`finish_bounce`](Self::finish_bounce) without its announcement:
    /// drops the rendered graph and the leftovers here and releases the
    /// bounce's token, but leaves telling the plugins Realtime to the next
    /// [`Phase::RenderMode`](crate::Phase::RenderMode) (the next
    /// [`update`](Self::update), or the host's own run of the phase).
    ///
    /// For a host that may start another bounce before that phase runs (a
    /// queue of them, one finished and the next started in one frame):
    /// with `finish_bounce` each plugin would be told Realtime and then
    /// Offline again between the two, and three of the four formats take a
    /// mode change only while deactivated, so each announcement is a
    /// deactivate and reactivate (and, for a plugin whose latency follows
    /// the mode, a re-plan). The host must run `Phase::RenderMode` after it,
    /// or the plugins stay Offline once the last bounce ends.
    ///
    /// A main-thread call (design 015 §4.2).
    ///
    /// # Errors
    ///
    /// As [`finish_bounce`](Self::finish_bounce).
    pub fn finish_bounce_unannounced<M: Master>(
        &mut self,
        rendered: BounceRendered<M>,
    ) -> Result<Bounced<M>, BounceError> {
        crate::main_thread();
        let BounceRendered {
            result,
            graph,
            _token,
        } = rendered;
        if let Some(graph) = graph {
            drop(graph.take());
        }
        self.bounce.drop_leftovers();
        drop(_token);
        result
    }

    /// [`bounce_start`](Self::bounce_start), run on this thread, and finished.
    ///
    /// # Errors
    ///
    /// As `bounce_start` and `finish_bounce`.
    pub fn bounce<M: Master>(
        &mut self,
        bounce: Bounce<NodeKey, M>,
    ) -> Result<Bounced<M>, BounceError> {
        let job = self.bounce_start(bounce)?;
        self.finish_bounce(job.run())
    }

    /// Drops, here, what bounces sent back so far: what a dropped job or a
    /// dropped return held (its fork, a hook's edit). `Phase::Collect` (each
    /// [`update`](Self::update)) and [`finish_bounce`](Self::finish_bounce)
    /// do this; a host that polls its bounces on a frame of its own calls it
    /// there. A main-thread call: a forked in-process VST2 plugin closes
    /// when dropped.
    pub fn drop_bounce_leftovers(&mut self) {
        crate::main_thread();
        self.bounce.drop_leftovers();
    }

    /// Whether a bounce is in flight: its job or what it returned is
    /// still alive.
    pub fn bounce_in_flight(&self) -> bool {
        self.bounce.in_flight()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InPort, Phase, PhaseCx, Root};
    use tutti_export::Stage;
    use tutti_graph::ForkByClone;
    use tutti_tempo::{Shape, TempoPoint};
    use tutti_types::{Bpm, SampleRate};

    const RATE: SampleRate = SampleRate(48_000.0);

    /// A render of a few blocks into memory.
    fn short() -> Bounce<NodeKey, BufferMaster> {
        Bounce::project()
            .master_buffers()
            .range(Range::Seconds(Seconds(0.0), Seconds(0.01)))
    }

    /// A detached session whose outputs hear a dc node: something to bounce.
    fn session() -> Session {
        let mut session = Session::builder().build_detached(RATE).unwrap();
        let (dc, _) = session
            .graph()
            .add(ForkByClone(tutti_nodes::testing::Const::mono(0.0)));
        session.graph().connect(dc, Root).unwrap();
        session
    }

    /// The planes of a finished buffers bounce.
    fn planes(out: Result<Bounced<BufferMaster>, BounceError>) -> Vec<Vec<f32>> {
        out.expect("renders").into_master().into_planes()
    }

    /// **A bounce renders what was edited, with no `update` between.** A node
    /// inserted and wired since the last update is in the fork: the planes
    /// hold its level, not silence.
    #[test]
    fn a_bounce_renders_an_edit_no_update_has_committed() {
        let mut session = session();
        let (half, _) = session
            .graph()
            .add(ForkByClone(tutti_nodes::testing::Const::mono(0.5)));
        session.graph().connect(half, Root).unwrap();
        assert!(session.graph().is_edited(), "no update has run");
        let out = planes(session.bounce(short()));
        assert!(out[0].iter().all(|&s| s == 0.5), "the edit is heard");
    }

    /// **`Beat(a)..Beat(b)` is the range from beat `a` to beat `b`**, and the
    /// same for seconds and frames.
    #[test]
    fn a_rust_range_is_a_range() {
        assert_eq!(
            Range::from(Beat(0.0)..Beat(8.0)),
            Range::Beats(Beat(0.0), Beat(8.0))
        );
        assert_eq!(
            Range::from(Seconds(1.0)..Seconds(2.0)),
            Range::Seconds(Seconds(1.0), Seconds(2.0))
        );
        assert_eq!(
            Range::from(Frame(0)..Frame(48_000)),
            Range::Frames(Frame(0), Frame(48_000))
        );
    }

    /// 120 BPM for four beats, then 60: a step at beat 4, which is second 2.
    fn stepped() -> TempoMap {
        TempoMap::new([
            TempoPoint {
                beat: Beat(0.0),
                tempo: Bpm::of(120.0),
                shape: Shape::Step,
            },
            TempoPoint {
                beat: Beat(4.0),
                tempo: Bpm::of(60.0),
                shape: Shape::Step,
            },
        ])
        .expect("two steps")
    }

    /// **A range in beats is the frames the map puts between them.** At 120
    /// BPM and 48 kHz a beat is 24 000 frames, so beats 1 to 3 are 48 000; over
    /// a step to 60 BPM at beat 4 (second 2), beats 3 to 6 are one beat at 120
    /// (24 000) and two at 60 (96 000), 120 000 frames. The render starts on
    /// the range's first beat. Written out from the definition, not from the
    /// conversion's code.
    #[test]
    fn a_range_in_beats_is_the_frames_the_map_puts_between_them() {
        let constant = TempoMap::constant(Bpm::of(120.0)).unwrap();
        let (start, length) =
            span(Range::Beats(Beat(1.0), Beat(3.0)), None, &constant, RATE).unwrap();
        assert_eq!((start, length), (Beat(1.0), Samples(48_000)));
        let (start, length) =
            span(Range::Beats(Beat(3.0), Beat(6.0)), None, &stepped(), RATE).unwrap();
        assert_eq!((start, length), (Beat(3.0), Samples(120_000)));
    }

    /// **Seconds and frames name the same span as the beats they are.** Two
    /// seconds from second 1 is 96 000 frames, starting at beat 2 at 120 BPM;
    /// frames 48 000 to 144 000 are the same span. The duration is `f64`
    /// seconds, rounded once to the nearest frame: 2000 beats at 93 BPM is
    /// 61 935 484 frames, where an `f32` second count lands on another frame.
    #[test]
    fn seconds_and_frames_are_the_span_they_name() {
        let map = TempoMap::constant(Bpm::of(120.0)).unwrap();
        let by_seconds = span(Range::Seconds(Seconds(1.0), Seconds(3.0)), None, &map, RATE);
        let by_frames = span(
            Range::Frames(Frame(48_000), Frame(144_000)),
            None,
            &map,
            RATE,
        );
        assert_eq!(by_seconds, Ok((Beat(2.0), Samples(96_000))));
        assert_eq!(by_frames, by_seconds);
        let long = (2000.0f64 / 93.0) * 60.0;
        assert_eq!(
            span(
                Range::Seconds(Seconds(0.0), Seconds(long)),
                None,
                &map,
                RATE
            )
            .unwrap()
            .1,
            Samples(61_935_484)
        );
    }

    /// **A range that is not a span, or keeps no frame, is refused by name.**
    /// Not a length (NaN, infinity, before the start) is `InvalidRange`;
    /// inverted, empty, or under half a frame is `EmptyRange`; the loop with
    /// none set is `NoRange`; the transport's loop is its beats.
    #[test]
    fn a_range_that_is_not_a_span_is_refused() {
        let map = TempoMap::constant(Bpm::of(120.0)).unwrap();
        let go = |r| span(r, None, &map, RATE);
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            assert_eq!(
                go(Range::Seconds(Seconds(0.0), Seconds(bad))),
                Err(BounceConfigError::InvalidRange),
                "{bad}"
            );
            assert_eq!(
                go(Range::Beats(Beat(0.0), Beat(bad))),
                Err(BounceConfigError::InvalidRange),
                "{bad}"
            );
        }
        assert_eq!(
            go(Range::Beats(Beat(4.0), Beat(2.0))),
            Err(BounceConfigError::EmptyRange)
        );
        assert_eq!(
            go(Range::Seconds(Seconds(1.0), Seconds(1.0))),
            Err(BounceConfigError::EmptyRange)
        );
        // Under half a frame rounds to nothing.
        assert_eq!(
            go(Range::Seconds(Seconds(0.0), Seconds(0.4 / 48_000.0))),
            Err(BounceConfigError::EmptyRange)
        );
        assert_eq!(go(Range::Loop), Err(BounceConfigError::NoRange));
        let looped = tutti_types::LoopRange::new(Beat(2.0), Beat(4.0));
        assert_eq!(
            span(Range::Loop, looped, &map, RATE),
            Ok((Beat(2.0), Samples(48_000)))
        );
    }

    /// **Nothing is guessed, and nothing is half-started.** A bounce with no
    /// range, loop or end, no format for a file, or a normalize on
    /// memory is refused by name before anything is forked: no token is held
    /// after.
    #[test]
    fn a_bounce_that_misses_what_it_needs_is_refused_before_it_forks() {
        let mut session = session();
        fn refused<M: Master>(session: &mut Session, b: Bounce<NodeKey, M>) -> BounceConfigError {
            match session.bounce_start(b) {
                Err(BounceError::Config(e)) => e,
                other => panic!("expected a config error, got {other:?}"),
            }
        }
        assert_eq!(
            refused(&mut session, Bounce::project().master_buffers()),
            BounceConfigError::NoRange
        );
        assert_eq!(
            refused(
                &mut session,
                Bounce::project().until(Beat(1.0)).master("x.wav")
            ),
            BounceConfigError::Missing(Field::Format)
        );
        assert_eq!(
            refused(
                &mut session,
                short().normalize(Normalize::peak(tutti_types::Db(-1.0)))
            ),
            BounceConfigError::NormalizeBuffers
        );
        assert!(!session.bounce_in_flight());
    }

    /// **The range is the one named, else the loop, else beat 0 to the end
    /// named, else none.** An explicit range wins over a set loop, which wins
    /// over `until`; and a bounce that names an end renders from beat 0 to it
    /// (two beats at 120 BPM and 48 kHz are 48 000 frames).
    #[test]
    fn the_range_is_named_then_the_loop_then_the_end() {
        let looped = tutti_types::LoopRange::new(Beat(1.0), Beat(2.0));
        let named = Range::Seconds(Seconds(0.0), Seconds(1.0));
        assert_eq!(chosen(Some(named), looped, Some(Beat(9.0))), Ok(named));
        assert_eq!(chosen(None, looped, Some(Beat(9.0))), Ok(Range::Loop));
        assert_eq!(
            chosen(None, None, Some(Beat(9.0))),
            Ok(Range::Beats(Beat(0.0), Beat(9.0)))
        );
        assert_eq!(chosen(None, None, None), Err(BounceConfigError::NoRange));

        let mut session = session();
        let until = Bounce::project().master_buffers().until(Beat(2.0));
        assert_eq!(planes(session.bounce(until))[0].len(), 48_000);
    }

    /// **The session's steps are reported as `Preparing`, in the band the
    /// render leaves it.** The edit runs after the fork is built (step 2 of
    /// 4) and its hook reads the handle: a streaming bounce's band is 0 to
    /// 0.05, so the reading is `Preparing` at 0.025; with a normalize the band
    /// is 0 to 0.03. After the render the reading is the last one, 1.0.
    #[test]
    fn the_sessions_steps_are_reported_as_preparing() {
        let mut session = session();
        let seen: Arc<Mutex<Vec<Progress>>> = Arc::default();
        let job_handle: Arc<Mutex<Option<BounceHandle>>> = Arc::default();
        let (into, handle) = (Arc::clone(&seen), Arc::clone(&job_handle));
        let job = session
            .bounce_start(short().edit(Box::new(move |_: PreparedGraph<'_>| {
                let handle = handle.lock().unwrap().clone().expect("set before run");
                into.lock().unwrap().extend(handle.progress());
            })))
            .expect("starts");
        let handle = job.handle();
        assert!(handle.progress().is_none(), "nothing before the job runs");
        *job_handle.lock().unwrap() = Some(handle.clone());
        let done = session.finish_bounce(job.run()).unwrap();
        drop(done);
        let at_edit = seen.lock().unwrap()[0];
        assert_eq!(at_edit.stage(), Stage::Preparing);
        assert!((at_edit.fraction() - 0.025).abs() < 1e-6, "{at_edit:?}");
        assert_eq!(at_edit.frames(), None);
        let last = handle.progress().expect("reported");
        assert_eq!(last.stage(), Stage::Finalizing);
        assert_eq!(last.fraction(), 1.0);
    }

    /// **An unannounced finish leaves the mode to the next
    /// `Phase::RenderMode`**; the announcing one (and `bounce`) announces
    /// Realtime itself. No plugin is needed: the mode asserted is the one the
    /// session announced.
    #[cfg(feature = "plugin")]
    #[test]
    fn an_unannounced_finish_leaves_the_mode_to_the_phase() {
        use tutti_plugin::RenderMode;
        let mut session = session();
        let job = session.bounce_start(short()).unwrap();
        assert_eq!(session.render_mode(), RenderMode::Offline);
        session.finish_bounce_unannounced(job.run()).unwrap();
        assert!(!session.bounce_in_flight(), "the token is released");
        assert_eq!(session.render_mode(), RenderMode::Offline, "not announced");
        // The next bounce starts with no Realtime in between.
        let job = session.bounce_start(short()).unwrap();
        assert_eq!(session.render_mode(), RenderMode::Offline);
        session.finish_bounce_unannounced(job.run()).unwrap();
        session.run_phase(
            Phase::RenderMode,
            PhaseCx::at(Instant::now()),
            &mut Vec::new(),
        );
        assert_eq!(
            session.render_mode(),
            RenderMode::Realtime,
            "the phase announces"
        );

        session.bounce(short()).unwrap();
        assert_eq!(
            session.render_mode(),
            RenderMode::Realtime,
            "the headless finish announces"
        );
    }

    /// **An untrimmed bounce with the graph's tail keeps the programme's
    /// end.** A tail excludes latency, so a constant through a lookahead
    /// limiter (240 frames at 48 kHz, no tail) leaves the graph 240 frames
    /// late: rendered untrimmed, the buffers are 480 + 240 frames, the
    /// first 240 silent and every one after them programme. Trimmed (the
    /// default), they are the 480 frames of programme alone.
    #[test]
    fn an_untrimmed_bounce_renders_the_latency_past_the_programme() {
        use tutti_types::{ChannelLayout, Db};
        const DURATION: usize = 480;
        const LATENCY: usize = 240;
        let mut session = session();
        let (dc, _) = session
            .graph()
            .add(ForkByClone(tutti_nodes::testing::Const::mono(0.25)));
        let (lim, _) = session.graph().add(tutti_nodes::LimiterNode::with_channels(
            ChannelLayout::MONO,
            Db(-6.0),
            Db(-1.0),
        ));
        session
            .graph()
            .set_sources(InPort::new(lim, 0), [crate::OutPort::new(dc, 0)])
            .unwrap();
        session.graph().connect(lim, Root).unwrap();
        assert_eq!(session.graph_ref().node_latency(lim), Samples(LATENCY));

        let request = || {
            Bounce::project()
                .master_buffers()
                .range(Range::Frames(Frame(0), Frame(DURATION as u64)))
        };

        let untrimmed = planes(session.bounce(request().latency(BounceLatency::Keep)));
        assert_eq!(untrimmed[0].len(), DURATION + LATENCY, "duration + latency");
        assert!(untrimmed[0][..LATENCY].iter().all(|&s| s == 0.0));
        assert!(
            untrimmed[0][LATENCY..].iter().all(|&s| s != 0.0),
            "every programme frame is kept"
        );

        let trimmed = planes(session.bounce(request()));
        assert_eq!(trimmed[0].len(), DURATION);
        assert!(trimmed[0].iter().all(|&s| s != 0.0));
    }

    /// **A frozen bounce's timeline is stopped at beat 0**: what its
    /// `PrepareEdit` is handed (`PreparedGraph::ctx`, the timeline a node
    /// it builds binds to) reads not rolling, at beat 0. No forked node
    /// observes it while the render's transport is stopped; a hook does.
    #[test]
    fn a_frozen_bounces_timeline_is_stopped_at_beat_zero() {
        let mut session = session();
        let seen: Arc<Mutex<Option<(bool, tutti_types::Beat)>>> = Arc::default();
        let into = Arc::clone(&seen);
        session
            .bounce(short().clock(ExportClock::frozen()).edit(Box::new(
                move |prepared: PreparedGraph<'_>| {
                    *into.lock().unwrap() = Some((prepared.ctx.is_rolling(), prepared.ctx.beat()));
                },
            )))
            .expect("the bounce renders");
        let (rolling, beat) = seen.lock().unwrap().expect("the edit ran");
        assert!(!rolling, "a frozen bounce's timeline must not roll");
        assert_eq!(beat, tutti_types::Beat(0.0));
    }

    /// **The default clock is the project's tempo map from the range's first
    /// beat**: the edit hook's timeline reads the range's start beat, and the
    /// tempo of the session's map (a set tempo, not a default 120).
    #[test]
    fn the_default_clock_starts_at_the_ranges_first_beat_on_the_projects_tempo() {
        let mut session = session();
        session.transport().settings().set_tempo(Bpm::of(90.0));
        let seen: Arc<Mutex<Option<(tutti_types::Beat, Bpm)>>> = Arc::default();
        let into = Arc::clone(&seen);
        session
            .bounce(
                Bounce::project()
                    .master_buffers()
                    .range(Range::Beats(Beat(3.0), Beat(4.0)))
                    .edit(Box::new(move |prepared: PreparedGraph<'_>| {
                        *into.lock().unwrap() = Some((
                            prepared.ctx.beat(),
                            prepared.ctx.tempo_map().tempo_at(prepared.ctx.beat()),
                        ));
                    })),
            )
            .expect("renders");
        assert_eq!(
            seen.lock().unwrap().expect("the edit ran"),
            (Beat(3.0), Bpm::of(90.0))
        );
    }

    /// **A bounce kept in flight past `BOUNCE_STALL_WARNING` is warned about
    /// once**, on the collect's `now`; its clock restarts once nothing is in
    /// flight, so the next leaked bounce is warned about too.
    ///
    /// The clock also restarts when a bounce starts: a host handing one
    /// bounce's slot to the next within a frame (Bevy) never lets a collect
    /// see nothing in flight, and two back-to-back bounces, each inside the
    /// bound, are not one long stall.
    #[test]
    fn a_leaked_bounce_is_warned_about_once() {
        let mut session = session();
        let collect = |session: &mut Session, at: Instant| {
            session.run_phase(Phase::Collect, PhaseCx::at(at), &mut Vec::new());
            session.bounce.stall_warnings
        };
        let t0 = Instant::now();
        let job = session.bounce_start(short()).expect("the bounce starts");
        assert_eq!(collect(&mut session, t0), 0, "just started");
        let minute = Duration::from_secs(60);
        assert_eq!(
            collect(&mut session, t0 + BOUNCE_STALL_WARNING - minute),
            0,
            "inside the bound"
        );
        assert_eq!(collect(&mut session, t0 + BOUNCE_STALL_WARNING), 1);
        assert_eq!(
            collect(&mut session, t0 + BOUNCE_STALL_WARNING + minute),
            1,
            "once"
        );
        assert_eq!(collect(&mut session, t0 + 3 * BOUNCE_STALL_WARNING), 1);

        // The job dropped: nothing in flight, and the next bounce's clock
        // starts at its own first collect.
        drop(job);
        let t1 = t0 + 4 * BOUNCE_STALL_WARNING;
        assert_eq!(collect(&mut session, t1), 1);
        let job = session.bounce_start(short()).expect("the next starts");
        assert_eq!(collect(&mut session, t1 + minute), 1, "a new clock");
        assert_eq!(
            collect(&mut session, t1 + minute + BOUNCE_STALL_WARNING),
            2,
            "the second leak is warned about too"
        );
        drop(job);

        // Back to back, with no collect between: 50 minutes, then 59.
        let t2 = t1 + 4 * BOUNCE_STALL_WARNING;
        assert_eq!(collect(&mut session, t2), 2, "nothing in flight");
        let start = |session: &mut Session| session.bounce_start(short()).expect("a bounce starts");
        let first = start(&mut session);
        assert_eq!(collect(&mut session, t2), 2);
        assert_eq!(collect(&mut session, t2 + 50 * minute), 2);
        session.finish_bounce_unannounced(first.run()).unwrap();
        let second = start(&mut session);
        assert_eq!(
            collect(&mut session, t2 + 70 * minute),
            2,
            "the second bounce has been in flight for 20 minutes, not 70"
        );
        assert_eq!(collect(&mut session, t2 + 129 * minute), 2, "59 minutes");
        session.finish_bounce_unannounced(second.run()).unwrap();
    }
}
