//! Recording: a source pumped into a sink on a thread of its own.
//!
//! [`tutti_types::pump`] is the whole of "recording" minus the loop and the
//! stop condition. [`PumpLoop`] is one pass of that loop with no thread and no
//! clock, and [`Recorder`] is a thread that runs it until it is told to stop
//! or the source ends. Together they join any [`AudioIn`] to any [`AudioOut`]:
//! a `tutti_codec::FileIn` to a file, a graph tap's
//! [`TapReader`](tutti_graph::TapReader) to a `tutti_codec::FileOut`, a
//! capture device to either.
//!
//! - [`PumpLoop::new`] and [`Recorder::start`] refuse a source and sink whose
//!   channel widths disagree, before any frame moves. The same check is a
//!   `debug_assert` in [`tutti_types::pump`].
//! - The scratch buffer is allocated once, so the loop never allocates.
//! - A zero-frame poll means "back off" or "finished" according to the
//!   source's [`ON_EMPTY`](AudioIn::ON_EMPTY). `Starved` sources (every
//!   microphone, every tap) never end on their own; [`finish`](Recorder::finish)
//!   stops them and [`wait`](Recorder::wait) would block forever.
//! - Dropping a live recorder stops, joins and finalizes it, so a file's header
//!   is always patched. `Drop` cannot return the result; a failure is logged.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use thiserror::Error;
use tutti_types::{AudioIn, AudioOut, ChannelLayout, OnEmpty, Samples, pump};

/// How long a [`Recorder`] parks when a [`Starved`](OnEmpty::Starved) source
/// yields nothing, unless the caller names another
/// ([`Recorder::start_with_capacity`]). A live source has nothing ready
/// between pushes; sleeping avoids busy-spinning a core while still draining
/// its ring far faster than it can overrun.
pub const IDLE_PARK: Duration = Duration::from_millis(5);

/// Frames moved per pump pass unless the caller names a capacity.
const SCRATCH_FRAMES: Samples = Samples(1024);

/// Why a recording could not start or did not end cleanly.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RecorderError {
    /// The source and sink disagree on channel width.
    ///
    /// Returned by [`PumpLoop::new`] and [`Recorder::start`] before any frame
    /// moves: recording a mismatched pair would rotate the file's channels
    /// every frame. It is the release-build half of the width-agreement
    /// invariant (the debug half is [`pump`]'s `debug_assert_eq!`), and it
    /// carries both layouts as values, not prose.
    #[error(
        "recorder source is {src:?} but the sink is {sink:?}; \
         recording a mismatched pair rotates the file's channels every frame"
    )]
    ChannelWidthMismatch {
        /// What the [`AudioIn`] source reports.
        src: ChannelLayout,
        /// What the [`AudioOut`] sink was opened with.
        sink: ChannelLayout,
    },

    /// The source broke mid-take ([`AudioIn::is_failed`]), for a microphone
    /// because its device went away.
    ///
    /// The frames delivered before the failure are in the sink, which was
    /// finalized: the file is valid and short.
    #[error("the recording source failed; the take holds only what it delivered before")]
    SourceFailed,

    /// File I/O failed: writing frames, or closing the sink in
    /// [`finalize`](AudioOut::finalize).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// [`RecorderError`] as the `io::Error` a sink's
/// [`finalize`](AudioOut::finalize) speaks, for a host that reports every way
/// a take can end through one channel (`bevy-tutti`'s `PumpFinished`).
///
/// [`Io`](RecorderError::Io) unwraps to the error it carries. The others
/// become an `io::Error` with this error as its source, so the layouts stay
/// reachable through `get_ref`/`downcast`.
impl From<RecorderError> for std::io::Error {
    fn from(error: RecorderError) -> Self {
        match error {
            RecorderError::Io(io) => io,
            RecorderError::SourceFailed => std::io::Error::other(RecorderError::SourceFailed),
            mismatch @ RecorderError::ChannelWidthMismatch { .. } => {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, mismatch)
            }
        }
    }
}

impl RecorderError {
    /// The error a take's `io::Error` result stands for:
    /// [`SourceFailed`](Self::SourceFailed) if that is what it carries,
    /// [`Io`](Self::Io) otherwise.
    fn from_take(error: std::io::Error) -> Self {
        match error.get_ref().and_then(|e| e.downcast_ref::<Self>()) {
            Some(Self::SourceFailed) => Self::SourceFailed,
            _ => Self::Io(error),
        }
    }
}

/// What one [`PumpLoop::pump_once`] pass achieved.
///
/// The two "nothing moved" cases are separate because a driver must act
/// differently on each (park and re-poll, or stop), and the source's
/// [`ON_EMPTY`](AudioIn::ON_EMPTY) is what tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpPass {
    /// Frames moved from the source into the sink. Never zero, since a
    /// zero-frame pass is one of the other variants.
    Wrote(Samples),
    /// The source had nothing ready *yet* ([`OnEmpty::Starved`]). Park and come
    /// back; there is more coming.
    Starved,
    /// The source has nothing more, ever ([`OnEmpty::EndOfStream`]). Stop.
    Ended,
    /// The source broke ([`AudioIn::is_failed`]) after its buffered frames were
    /// drained, or the sink failed ([`AudioOut::is_failed`]). Stop and
    /// finalize what was written.
    ///
    /// Outranks [`Ended`](Self::Ended): a finite source that failed and is
    /// drained is a failure, not a clean end.
    Failed,
}

/// One source-to-sink pump, with no thread and no clock.
///
/// Owns both endpoints and a scratch buffer. [`pump_once`](Self::pump_once)
/// moves at most the capacity it was built with and allocates nothing. Step it
/// by hand to count frames exactly, or hand it to a [`Recorder`] to run on a
/// thread.
///
/// `S` is the sample element both endpoints speak. The channel width is not a
/// type parameter: the endpoints carry it as a runtime
/// [`ChannelLayout`].
///
/// # Examples
///
/// Recording what the graph is playing needs no device: a tap on the graph's
/// two outputs fills as the graph renders, the tap's reader is an [`AudioIn`]
/// as it is, and one pass moves a block into a file. Swap the reader for
/// `tutti_cpal::MicIn` and the same lines record a microphone: that
/// interchangeability is the reason the traits exist.
///
/// ```
/// use tutti_codec::{FileOut, Format, Pcm, Spec};
/// use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, Transport, Unforkable};
/// use tutti_session::{PumpLoop, PumpPass};
/// use tutti_types::graph::{OutPort, Source};
/// use tutti_types::{AudioOut, Beat, Bpm, ChannelLayout, NodeKey, SampleRate, Samples};
/// use tutti_types::{TapAlign, TapId};
///
/// /// A stereo source: 0.25 on the left, -0.25 on the right.
/// struct Tone;
/// impl Node for Tone {
///     fn shape(&self) -> Shape {
///         Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
///     }
///     fn prepare(&mut self, _: &Prepare) {}
///     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
///         io.output(0).fill(0.25);
///         io.output(1).fill(-0.25);
///         Status::Modified
///     }
///     fn reset(&mut self) {}
/// }
///
/// // A tap on both of the graph's outputs, copied as the graph renders.
/// let (mut editor, mut executor) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(64)));
/// editor.insert(NodeKey(1), "tone", Unforkable(Tone));
/// let out = |port| Source::Node(OutPort { node: NodeKey(1), port });
/// editor.spec_mut().topology.outputs = vec![out(0), out(1)];
/// let reader = editor
///     .open_tap(TapId(0), vec![out(0), out(1)], TapAlign::ToOutput, Samples(1024))
///     .expect("the ports exist");
/// editor.commit().expect("compiles");
/// executor.apply_pending();
/// editor.collect();
///
/// // The audio callback renders three frames.
/// let (mut left, mut right) = ([0.0f32; 3], [0.0f32; 3]);
/// let transport = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
/// executor.process(3, &transport, &[], &mut [&mut left[..], &mut right[..]]);
///
/// let dir = tempfile::tempdir().expect("temp dir");
/// let spec = Spec::new(Format::Wav(Pcm::Float32), ChannelLayout::STEREO, SampleRate(48_000.0));
/// let sink = FileOut::create(dir.path().join("take.wav"), &spec).expect("sink opens");
///
/// // `new` checks the widths agree, before a frame moves.
/// let mut pump = PumpLoop::new(reader, sink, Samples(1024)).expect("stereo into stereo");
/// assert_eq!(pump.pump_once(), PumpPass::Wrote(Samples(3)));
/// // An empty tap is a producer that has not caught up, not the end.
/// assert_eq!(pump.pump_once(), PumpPass::Starved);
///
/// // `finalize` takes `self`, so the header back-patch happens exactly once and
/// // writing after it is a compile error rather than a corrupt file.
/// pump.finalize().expect("header back-patches");
/// ```
///
/// # Two things worth knowing
///
/// - **`AudioIn::ON_EMPTY` is not optional bookkeeping.** A zero-frame poll
///   means "not yet" for a mic and "never again" for a decoded file, and
///   nothing in the count distinguishes them. Getting it wrong ends a take
///   milliseconds in with no error anywhere.
/// - **The width check is mandatory.** [`new`](Self::new) is the one place both
///   endpoints are in scope before a frame moves, so a width mismatch is an
///   error there rather than a file whose channels rotate every frame.
pub struct PumpLoop<I, O, S = f32> {
    src: I,
    sink: O,
    /// Allocated once at construction; `pump_once` never grows it.
    scratch: Vec<S>,
    /// Set by the first [`PumpPass::Failed`]; a [`Recorder`] holds the other
    /// end.
    failed: Arc<AtomicBool>,
    /// Frames the source discarded, summed ([`AudioIn::take_lost`]); a
    /// [`Recorder`] holds the other end.
    lost: Arc<AtomicU64>,
}

impl<I, O, S> std::fmt::Debug for PumpLoop<I, O, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PumpLoop")
            .field("scratch_len", &self.scratch.len())
            .finish_non_exhaustive()
    }
}

impl<I, O, S> PumpLoop<I, O, S>
where
    S: Copy + Default,
    I: AudioIn<S>,
    O: AudioOut<S>,
{
    /// A loop over `src` and `sink` moving at most `capacity` frames a pass.
    ///
    /// `capacity` is in **frames**, a [`Samples`], so a caller cannot hand over
    /// a sample count by mistake and get a buffer `width` times too large. The
    /// scratch is allocated once, sized
    /// [`capacity.interleaved_len(layout)`](Samples::interleaved_len) from the
    /// width both endpoints agreed on. A zero capacity is taken as one frame:
    /// an empty scratch would make every poll return zero, ending a finite
    /// take before its first frame and parking a live one forever.
    ///
    /// The **sample rate** is the caller's to match: [`AudioIn`] carries none,
    /// so nothing here can compare them.
    ///
    /// # Errors
    ///
    /// [`RecorderError::ChannelWidthMismatch`] when `src` and `sink` disagree
    /// on channel width, before any frame is written.
    pub fn new(src: I, sink: O, capacity: Samples) -> Result<Self, RecorderError> {
        let layout = src.layout();
        if layout != sink.layout() {
            return Err(RecorderError::ChannelWidthMismatch {
                src: layout,
                sink: sink.layout(),
            });
        }
        let frames = if capacity.is_zero() {
            Samples(1)
        } else {
            capacity
        };
        Ok(Self {
            src,
            sink,
            scratch: vec![S::default(); frames.interleaved_len(layout)],
            failed: Arc::new(AtomicBool::new(false)),
            lost: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Runs one pass: polls a block of frames from the source and writes them
    /// to the sink.
    ///
    /// A zero-frame poll is reported as [`Starved`](PumpPass::Starved) or
    /// [`Ended`](PumpPass::Ended) according to the source's own
    /// [`ON_EMPTY`](AudioIn::ON_EMPTY), unless the sink has failed or the
    /// source has failed and is drained, which is [`Failed`](PumpPass::Failed).
    pub fn pump_once(&mut self) -> PumpPass {
        let mut n = pump(&mut self.src, &mut self.sink, &mut self.scratch);
        self.lost
            .fetch_add(self.src.take_lost().get() as u64, Ordering::Relaxed);
        // A source can push its last frames and then fail between the poll that
        // came back empty and this check. The failure flag is read after the
        // empty poll, so every frame pushed before it is visible to a second
        // poll; only an empty second poll is a drained, failed source.
        if n.is_zero() && self.src.is_failed() {
            n = pump(&mut self.src, &mut self.sink, &mut self.scratch);
            if n.is_zero() {
                self.failed.store(true, Ordering::Release);
                return PumpPass::Failed;
            }
        }
        if self.sink.is_failed() {
            self.failed.store(true, Ordering::Release);
            return PumpPass::Failed;
        }
        if !n.is_zero() {
            return PumpPass::Wrote(n);
        }
        match I::ON_EMPTY {
            OnEmpty::Starved => PumpPass::Starved,
            OnEmpty::EndOfStream => PumpPass::Ended,
        }
    }

    /// Whether a pass has failed: the source broke or the sink did.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// The frames the source discarded so far ([`AudioIn::take_lost`]), summed
    /// over the passes run. A recording closes up over each gap.
    #[must_use]
    pub fn lost(&self) -> Samples {
        Samples(usize::try_from(self.lost.load(Ordering::Relaxed)).unwrap_or(usize::MAX))
    }

    /// Consumes the loop and closes the sink (for a file, back-patching its
    /// header).
    ///
    /// Taking `self` makes it once-only: a loop that has finalized cannot pump
    /// again.
    ///
    /// # Errors
    ///
    /// The sink's [`finalize`](AudioOut::finalize) error; for a WAV, the file
    /// is then unreadable. If the sink closed cleanly but the source had
    /// failed, [`RecorderError::SourceFailed`] as an `io::Error`: the file is
    /// valid and holds the frames delivered before the failure.
    pub fn finalize(self) -> std::io::Result<()> {
        let source_failed = self.src.is_failed();
        self.sink.finalize()?;
        if source_failed {
            return Err(RecorderError::SourceFailed.into());
        }
        Ok(())
    }
}

/// Pumps any [`AudioIn`] source into any [`AudioOut`] sink on a background
/// thread.
///
/// [`start`](Self::start) checks that source and sink have the same channel
/// width, spawns a dedicated thread that repeatedly runs a [`PumpLoop`] pass
/// (up to 1024 frames), and returns once the pump is live. When a live source
/// has nothing ready, the thread parks ([`IDLE_PARK`] unless named) and polls
/// again; a finite source ends the loop by itself.
///
/// End a take with one of:
///
/// - [`finish`](Self::finish), for a live source (a microphone, a tap): stops
///   the thread wherever it is, finalizes the sink and returns the result.
/// - [`wait`](Self::wait), for a finite source (a decoded file): lets the
///   source reach its end, then finalizes.
/// - [`finish_in_place`](Self::finish_in_place), `finish` without consuming the
///   recorder, for a host that stores the take in place (a Bevy component).
///   Pair it with [`request_stop`](Self::request_stop) and
///   [`is_finished`](Self::is_finished) to end a take without blocking.
/// - dropping the recorder, which does what `finish` does and logs a failure.
///
/// The sample rate is not checked, since [`AudioIn`] carries none: build the
/// sink at the source's rate.
///
/// # Examples
///
/// ```no_run
/// use tutti_codec::{FileOut, Format, Pcm, Spec};
/// use tutti_session::Recorder;
/// use tutti_types::{ChannelLayout, SampleRate};
///
/// # fn go(reader: tutti_graph::TapReader) -> Result<(), Box<dyn std::error::Error>> {
/// // A stereo tap and a stereo sink pair: a tap is an `AudioIn` as it is.
/// let spec = Spec::new(Format::Wav(Pcm::Float32), ChannelLayout::STEREO, SampleRate(48_000.0));
/// let rec = Recorder::start(reader, FileOut::create("master.wav", &spec)?)?;
/// // ... later ...
/// rec.finish()?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Recorder {
    /// Cleared by [`request_stop`](Self::request_stop) to break the pump loop.
    running: Arc<AtomicBool>,
    /// The running pump. Taking it makes the shutdown once-only: whichever of
    /// `finish`, `wait`, `finish_in_place` and `drop` runs first leaves `None`.
    handle: Option<JoinHandle<std::io::Result<()>>>,
    /// Raised by the pump on its first failed pass; see [`failed`](Self::failed).
    failed: Arc<AtomicBool>,
    /// Frames the source discarded; see [`lost`](Self::lost).
    lost: Arc<AtomicU64>,
}

impl Recorder {
    /// Spawns a thread pumping `src` into `sink`, 1024 frames a pass, parking
    /// [`IDLE_PARK`] when a live source is empty. Returns once the pump is
    /// live.
    ///
    /// # Errors
    ///
    /// [`RecorderError::ChannelWidthMismatch`] when `src` and `sink` disagree
    /// on channel width, before any frame is written.
    pub fn start<I, O, S>(src: I, sink: O) -> Result<Self, RecorderError>
    where
        I: AudioIn<S> + Send + 'static,
        O: AudioOut<S> + Send + 'static,
        S: Default + Copy + Send + 'static,
    {
        Self::start_with_capacity(src, sink, SCRATCH_FRAMES, IDLE_PARK)
    }

    /// Starts like [`start`](Self::start), moving at most `capacity` frames a
    /// pass and parking `park` when a [`Starved`](OnEmpty::Starved) source is
    /// empty (an [`EndOfStream`](OnEmpty::EndOfStream) one never parks: it
    /// exits). How long to wait is the host's tuning; *whether* to wait is the
    /// source's nature, which is why only this half is an argument.
    ///
    /// # Errors
    ///
    /// As [`PumpLoop::new`].
    pub fn start_with_capacity<I, O, S>(
        src: I,
        sink: O,
        capacity: Samples,
        park: Duration,
    ) -> Result<Self, RecorderError>
    where
        I: AudioIn<S> + Send + 'static,
        O: AudioOut<S> + Send + 'static,
        S: Default + Copy + Send + 'static,
    {
        Ok(Self::run(PumpLoop::new(src, sink, capacity)?, park))
    }

    /// Runs an already built loop on a thread of its own, parking `park` on a
    /// starved pass.
    pub fn run<I, O, S>(mut pump_loop: PumpLoop<I, O, S>, park: Duration) -> Self
    where
        I: AudioIn<S> + Send + 'static,
        O: AudioOut<S> + Send + 'static,
        S: Default + Copy + Send + 'static,
    {
        let running = Arc::new(AtomicBool::new(true));
        let failed = Arc::clone(&pump_loop.failed);
        let lost = Arc::clone(&pump_loop.lost);
        let flag = Arc::clone(&running);
        let handle = std::thread::spawn(move || {
            while flag.load(Ordering::Acquire) {
                match pump_loop.pump_once() {
                    PumpPass::Wrote(_) => {}
                    // Live source: the producer has not caught up.
                    PumpPass::Starved => std::thread::sleep(park),
                    // Finite source: there is no more to read. Broken source
                    // or sink: nothing more will come.
                    PumpPass::Ended | PumpPass::Failed => break,
                }
            }
            // Finalize exactly once, here, where the loop is owned. The result
            // rides the joined thread back to `finish`.
            pump_loop.finalize()
        });
        Self {
            running,
            handle: Some(handle),
            failed,
            lost,
        }
    }

    /// Stops the pump, waits for its thread, and finalizes the sink.
    ///
    /// Blocks for at most one pass (or one park). Frames the source has not
    /// yet delivered are not recorded; for a finite source that should run to
    /// its end, use [`wait`](Self::wait). After
    /// [`finish_in_place`](Self::finish_in_place) has already returned the
    /// result, this returns `Ok(())`.
    ///
    /// # Errors
    ///
    /// [`RecorderError::SourceFailed`] if the source broke mid-take; the sink
    /// was finalized, so the file is valid and holds what was delivered.
    /// [`RecorderError::Io`] if finalizing failed (for a WAV, the header was
    /// left unpatched and the file is unreadable) or the pump thread panicked.
    pub fn finish(mut self) -> Result<(), RecorderError> {
        self.finish_in_place()
            .unwrap_or(Ok(()))
            .map_err(RecorderError::from_take)
    }

    /// Waits for a **finite** take to reach its own end, then finalizes.
    ///
    /// [`finish`](Self::finish) cuts a source off wherever the pump happens to
    /// be, which silently truncates a finite one. This instead lets the loop
    /// run until the source reports
    /// [`OnEmpty::EndOfStream`], then returns the finalize result as `finish`
    /// does.
    ///
    /// **On a [`Starved`](OnEmpty::Starved) source this blocks until the source
    /// fails**, and every microphone and tap is one: the pump parks and
    /// re-polls rather than ending, and only a failure ([`failed`](Self::failed))
    /// or a stop ends it. After [`finish_in_place`](Self::finish_in_place) has
    /// already returned the result, this returns `Ok(())` without blocking.
    ///
    /// # Errors
    ///
    /// As [`finish`](Self::finish).
    pub fn wait(mut self) -> Result<(), RecorderError> {
        // Not `finish_in_place`: that clears the stop flag and truncates the take.
        self.join_pump()
            .unwrap_or(Ok(()))
            .map_err(RecorderError::from_take)
    }

    /// Signals the pump to stop, without waiting for it. Idempotent.
    ///
    /// The thread notices on its next pass, finalizes, and stops;
    /// [`is_finished`](Self::is_finished) then reads `true`, and
    /// [`finish_in_place`](Self::finish_in_place) collects the result without
    /// blocking.
    pub fn request_stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Whether the pump is still meant to run: `false` once a stop was
    /// requested.
    ///
    /// This reports the *flag*, not the thread: a finite source that ran out
    /// has stopped while this still reads `true`. Ask
    /// [`is_finished`](Self::is_finished) for that.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Whether the take failed: the source broke ([`AudioIn::is_failed`], for a
    /// microphone its device went away) or the sink did
    /// ([`AudioOut::is_failed`], for a file a write error such as a full disk).
    ///
    /// The pump stops and finalizes the frames already written, so the file
    /// stays readable up to the failure and [`is_finished`](Self::is_finished)
    /// turns `true`. The sink's own error, if any, comes from
    /// [`finish_in_place`](Self::finish_in_place).
    #[must_use]
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// The frames the source discarded so far ([`AudioIn::take_lost`]): a
    /// tap's ring that the pump did not drain in time, a capture ring that
    /// overflowed. A live take's health beside [`failed`](Self::failed).
    ///
    /// **A gap closes up.** The sink is written the frames that arrived, in
    /// order, so a file is shorter than the take by this many frames, with
    /// nothing in the audio to mark where. Zero means the file is the whole
    /// take. Readable after the take ends.
    #[must_use]
    pub fn lost(&self) -> Samples {
        Samples(usize::try_from(self.lost.load(Ordering::Relaxed)).unwrap_or(usize::MAX))
    }

    /// Whether the pump thread has stopped running passes, so
    /// [`finish_in_place`](Self::finish_in_place) will not block.
    ///
    /// Never blocks. `true` once a finite source has ended its own take, or
    /// once a requested stop has been noticed. `false` after a shutdown, since
    /// there is no pump left to report on.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_some_and(JoinHandle::is_finished)
    }

    /// Stops the pump, joins it and returns the sink's finalize result,
    /// leaving the recorder in place.
    ///
    /// [`finish`](Self::finish) for a host that stores the take where it cannot
    /// be moved out (a component, a field). `None` once the result was already
    /// collected, which is what makes a second finalize unrepresentable rather
    /// than a panic. A failed source comes back as
    /// [`RecorderError::SourceFailed`] inside the `io::Error` (see
    /// [`PumpLoop::finalize`]). Blocks for at most one pass (or one park);
    /// after [`is_finished`](Self::is_finished) it does not block at all.
    ///
    /// Clearing the stop flag is the half that matters for a **live** source:
    /// the loop exits on its own only for a finite one; a starving source
    /// parks and re-polls forever, so nothing else ever reaches the finalize
    /// below it.
    pub fn finish_in_place(&mut self) -> Option<std::io::Result<()>> {
        self.request_stop();
        self.join_pump()
    }

    /// Joins the pump and surfaces its finalize result, leaving `running`
    /// alone. The half [`finish_in_place`](Self::finish_in_place) and
    /// [`wait`](Self::wait) share, and the whole difference between them is
    /// the flag clear before this call.
    fn join_pump(&mut self) -> Option<std::io::Result<()>> {
        self.handle.take().map(|handle| {
            handle
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("recording thread panicked")))
        })
    }
}

/// Stops, joins and finalizes a recorder nobody shut down; a failure is
/// logged, since `Drop` has nowhere to return it.
impl Drop for Recorder {
    fn drop(&mut self) {
        if let Some(Err(error)) = self.finish_in_place() {
            tracing::warn!("recorder dropped; its sink did not close cleanly: {error}");
        }
    }
}

#[cfg(test)]
mod tests;
