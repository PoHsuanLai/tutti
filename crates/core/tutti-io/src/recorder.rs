//! [`Recorder`] — the background pump that drives any source into any sink.
//!
//! Recording is `pump(src, sink)`: poll a block of frames from an
//! [`AudioIn`], write it to an [`AudioOut`] (a [`WavOut`](crate::WavOut), a
//! socket, a counter in a test), repeat. This is the caller side of
//! [`tutti_core::io::pump`] — the loop and the stop policy that the pump
//! function itself deliberately leaves out.
//!
//! # The one pump loop
//!
//! This is the workspace's only `AudioIn → AudioOut` thread loop. `bevy-tutti`
//! had a second one (its `AudioPump` component ran the same poll / park /
//! break / finalize body on its own thread); design 015 folded it into
//! this one. The component now holds a `Recorder` and adds only what an ECS
//! needs on top: polling for the end without blocking
//! ([`is_finished`](Recorder::is_finished)), asking for the end without
//! blocking ([`request_stop`](Recorder::request_stop)), and joining in place
//! ([`shutdown`](Recorder::shutdown)) so a despawn can finalize. That is the
//! whole of the difference, so it is three methods here and not a second loop.
//!
//! # It takes a source, not a device
//!
//! [`start`](Recorder::start) is generic over the [`AudioIn`], so a microphone
//! is one option among several: a socket, a decoded file, or a generated signal
//! record identically. That is what keeps this crate device-free and lets it sit
//! *below* `tutti-cpal` rather than inside it — a host opens its own `MicIn` and
//! hands it over. It is generic over the [`AudioOut`] and the sample element
//! too, for the same reason.
//!
//! # Threading, and the seam through it
//!
//! The pump runs on its own thread, never a shared pool. It does not run to
//! completion — a live take ends when someone stops it — so occupying a pool
//! slot would starve every other job on it. The thread owns both endpoints
//! outright, which is what gives
//! [`finalize`](tutti_core::io::AudioOut::finalize) a clear home: it consumes
//! the sink by value and may run only once, so the thread breaks its loop on the
//! stop flag, finalizes, and returns the `io::Result` that
//! [`stop`](Recorder::stop) recovers by joining.
//!
//! *How* the loop runs is separated from *what it does*. [`PumpLoop`] is one
//! pass over both endpoints — poll, write, decide — and holds no thread and no
//! clock. A [`PumpDriver`] decides what to do with it: [`ThreadDriver`] (what
//! [`start`](Recorder::start) uses) spawns the thread and parks
//! [`IDLE_PARK`] on a starving source, and [`ManualDriver`] hands the loop
//! straight back so a caller can run passes and count frames.
//!
//! That split exists because a recorder is otherwise only observable through
//! wall clock. A driven loop makes its claims by *counting passes*: "three
//! pumps of a source that yields on every other poll wrote two frames" is
//! exact, and it fails for the reason it names.
//!
//! # Dropping is a shutdown, not a leak
//!
//! Ending a take is the *only* way `finalize` runs, so it cannot be left to a
//! caller remembering to ask. [`Drop`] performs the same stop-and-join
//! [`stop`](Recorder::stop) does — the two share one
//! [`shutdown`](Recorder::shutdown), and the taken `JoinHandle` is what stops
//! them joining twice.
//!
//! This is not defensive tidiness. The loop exits on its own only for a finite
//! source ([`EndOfStream`](OnEmpty::EndOfStream) breaks); a
//! [`Starved`](OnEmpty::Starved) one — every microphone capture — parks and
//! re-polls forever. Without the `Drop` impl a dropped live recorder spins a
//! thread for the life of the process and never back-patches the WAV header,
//! which leaves the file unreadable *despite* its audio being on disk.
//!
//! `Drop` cannot return the finalize `Result` and this workspace does not panic
//! in `Drop`, so that path stores its outcome in a
//! [`FinalizeStatus`] handle taken beforehand. [`stop`](Recorder::stop) and
//! [`shutdown`](Recorder::shutdown) remain the paths that hand the result
//! straight back.
//!
//! The scratch buffer is allocated once before the loop, sized
//! `capacity.interleaved_len(layout)` samples at the source's own width
//! (`capacity` is 1024 frames unless
//! [`start_with_capacity`](Recorder::start_with_capacity) names another); the
//! loop body never allocates.
//!
//! Whether a zero-FRAME poll means "back off" or "finished" is the source's
//! own [`ON_EMPTY`](tutti_core::io::AudioIn::ON_EMPTY), and the pump loop is
//! where that const is spent. `AudioIn` unifies live and finite sources, so the
//! same zero carries two meanings: a mic has nothing ready *yet*, a decoded file
//! has nothing ready *ever*. Reading a live source as finite ends a take
//! milliseconds in, with no error anywhere; reading a finite one as live spins
//! the thread forever. Neither is detectable from the count alone, which is why
//! the source declares it.
//!
//! # The width check lives here
//!
//! [`start`](Recorder::start) refuses a source and sink whose channel counts
//! disagree. `AudioIn` and `AudioOut` carry the frame width as a runtime
//! [`ChannelLayout`](tutti_core::ChannelLayout), because only a runtime width
//! can express one that comes from *data* — a decoded file, a surround capture
//! device — so "a stereo mic cannot feed a 6-channel WAV" is not a type error.
//! It is checked twice at runtime instead: a `debug_assert` on the two layouts
//! inside [`pump`], and **the error `start` returns.**
//!
//! This is the right home for the checked half because it is the one place both
//! endpoints are in scope *before any frame moves*. A mismatch caught here costs
//! a returned error; the same mismatch caught nowhere writes a file whose
//! channels rotate every frame and reads back as a subtly-wrong recording rather
//! than as a failure.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tutti_core::io::{pump, AudioIn, AudioOut, OnEmpty};
use tutti_core::Samples;

use crate::error::{Error, Result};

/// Frames moved per pump pass unless the caller names a capacity. One
/// bufferful, allocated once before the loop so the pump body stays
/// allocation-free. ~21ms at 48kHz — small enough to bound how far the sink can
/// lag the source, large enough to amortize per-pass cost.
const SCRATCH_FRAMES: Samples = Samples(1024);

/// How long a default [`ThreadDriver`] parks when a
/// [`Starved`](OnEmpty::Starved) source yields nothing. A live mic has nothing
/// ready between callback pushes; sleeping avoids busy-spinning a core while
/// still draining the ring far faster than it can overrun.
///
/// This belongs to the *driver*, not to the pump: it is a pacing decision about
/// a thread, and [`PumpLoop`] has neither. [`ThreadDriver::with_park`] names
/// another.
pub const IDLE_PARK: Duration = Duration::from_millis(5);

/// What one [`PumpLoop::pump_once`] pass achieved.
///
/// The two "nothing moved" cases are separate because a driver must act
/// differently on each — park and re-poll, or stop — and the source's
/// [`ON_EMPTY`](AudioIn::ON_EMPTY) is what tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpPass {
    /// Frames moved from the source into the sink — never zero, since a
    /// zero-frame pass is one of the other two variants.
    Wrote(Samples),
    /// The source had nothing ready *yet* ([`OnEmpty::Starved`]). Park and come
    /// back; there is more coming.
    Starved,
    /// The source has nothing more, ever ([`OnEmpty::EndOfStream`]). Stop.
    Ended,
}

/// One source→sink pump, with no thread and no clock.
///
/// Owns both endpoints and a scratch buffer. [`pump_once`](Self::pump_once)
/// moves at most the capacity the recorder was started with (1024 frames by
/// default) and allocates nothing.
///
/// `S` is the sample element both endpoints speak. The channel width is not a
/// type parameter: the endpoints carry it as a runtime `ChannelLayout`.
///
/// Built by [`Recorder::start_with_capacity`] after it has checked the two
/// widths agree, and handed to a [`PumpDriver`]; a custom driver runs it.
pub struct PumpLoop<I, O, S = f32> {
    src: I,
    sink: O,
    /// Allocated once at construction; `pump_once` never grows it.
    scratch: Vec<S>,
}

impl<I, O, S> PumpLoop<I, O, S>
where
    S: Copy,
    I: AudioIn<S>,
    O: AudioOut<S>,
{
    /// Runs one pass: polls a block of frames from the source and writes them
    /// to the sink.
    ///
    /// A zero-frame poll is reported as [`Starved`](PumpPass::Starved) or
    /// [`Ended`](PumpPass::Ended) according to the source's own
    /// [`ON_EMPTY`](AudioIn::ON_EMPTY).
    pub fn pump_once(&mut self) -> PumpPass {
        let n = pump(&mut self.src, &mut self.sink, &mut self.scratch);
        if !n.is_zero() {
            return PumpPass::Wrote(n);
        }
        match I::ON_EMPTY {
            OnEmpty::Starved => PumpPass::Starved,
            OnEmpty::EndOfStream => PumpPass::Ended,
        }
    }

    /// Consumes the loop and closes the sink (for a WAV, back-patching its
    /// header).
    ///
    /// Taking `self` makes it once-only: a driver that has finalized cannot
    /// pump again.
    ///
    /// # Errors
    ///
    /// The sink's [`finalize`](AudioOut::finalize) error; for a WAV, the file
    /// is then unreadable.
    pub fn finalize(self) -> std::io::Result<()> {
        self.sink.finalize()
    }
}

/// Decides where a [`PumpLoop`] runs and how it waits.
///
/// [`Recorder`] owns the take — the stop flag, the once-only shutdown, the
/// finalize outcome — and delegates only the execution; a driver decides
/// nothing about when a take ends.
///
/// [`ThreadDriver`] is what [`Recorder::start`] uses; [`ManualDriver`] lets a
/// test run passes by hand and count frames. An implementation must finalize
/// the loop exactly once when it stops.
pub trait PumpDriver {
    /// The handle of the running take, joinable for its finalize result.
    type Running: RunningPump;

    /// Starts running `loop_`, stopping when `running` is cleared.
    ///
    /// `running` is the [`Recorder`]'s stop flag. An implementation must check
    /// it between passes — a [`Starved`](PumpPass::Starved) source never ends on
    /// its own, so this flag is the only thing that ends a live take.
    fn drive<I, O, S>(self, loop_: PumpLoop<I, O, S>, running: Arc<AtomicBool>) -> Self::Running
    where
        I: AudioIn<S> + Send + 'static,
        O: AudioOut<S> + Send + 'static,
        S: Copy + Send + 'static;
}

/// The handle a [`PumpDriver`] hands back: something that can be waited on for
/// the finalize result.
///
/// `Send + Sync` so a [`Recorder`] is too: a host stores a take wherever it
/// keeps state (a Bevy component, a resource, a struct shared across threads),
/// and an `!Sync` handle would rule most of those out.
pub trait RunningPump: Send + Sync {
    /// Waits for the pump to stop and returns what
    /// [`finalize`](PumpLoop::finalize) reported.
    ///
    /// Called exactly once, by [`Recorder`]'s shutdown. Takes `Box<Self>`
    /// because [`Recorder`] stores the handle as a `dyn RunningPump`.
    ///
    /// # Errors
    ///
    /// The finalize error, or an error if the pump thread panicked.
    fn join(self: Box<Self>) -> std::io::Result<()>;

    /// Whether the pump has stopped running passes, so [`join`](Self::join)
    /// would not block.
    ///
    /// Never blocks. This is how a host that cannot block (a frame loop)
    /// notices that a finite source has ended its own take.
    fn is_finished(&self) -> bool;
}

/// The default driver: one dedicated thread per take.
///
/// Parks [`IDLE_PARK`] (or the duration given to
/// [`with_park`](Self::with_park)) when the source is starved rather than
/// spinning a core, and stops on [`Ended`](PumpPass::Ended) or on the cleared
/// stop flag. This is what [`Recorder::start`] uses.
#[derive(Debug, Clone, Copy)]
pub struct ThreadDriver {
    park: Duration,
}

impl ThreadDriver {
    /// A thread driver that parks `park` on a [`Starved`](PumpPass::Starved)
    /// pass.
    ///
    /// Only a starving source ever parks; an
    /// [`EndOfStream`](OnEmpty::EndOfStream) one exits instead. How long to
    /// wait is the host's tuning; *whether* to wait is the source's nature,
    /// which is why only this half is an argument.
    #[must_use]
    pub const fn with_park(park: Duration) -> Self {
        Self { park }
    }

    /// How long this driver parks on a starved pass.
    #[must_use]
    pub const fn park(&self) -> Duration {
        self.park
    }
}

impl Default for ThreadDriver {
    /// Parks [`IDLE_PARK`].
    fn default() -> Self {
        Self::with_park(IDLE_PARK)
    }
}

impl PumpDriver for ThreadDriver {
    type Running = std::thread::JoinHandle<std::io::Result<()>>;

    fn drive<I, O, S>(self, mut loop_: PumpLoop<I, O, S>, running: Arc<AtomicBool>) -> Self::Running
    where
        I: AudioIn<S> + Send + 'static,
        O: AudioOut<S> + Send + 'static,
        S: Copy + Send + 'static,
    {
        let park = self.park;
        std::thread::spawn(move || {
            while running.load(Ordering::Acquire) {
                match loop_.pump_once() {
                    PumpPass::Wrote(_) => {}
                    // Live source: the producer has not caught up.
                    PumpPass::Starved => std::thread::sleep(park),
                    // Finite source: there is no more to read.
                    PumpPass::Ended => break,
                }
            }
            // Finalize exactly once, here, where the loop is owned. The result
            // rides the joined thread back to `stop`.
            loop_.finalize()
        })
    }
}

impl RunningPump for std::thread::JoinHandle<std::io::Result<()>> {
    fn join(self: Box<Self>) -> std::io::Result<()> {
        std::thread::JoinHandle::join(*self)
            .unwrap_or_else(|_| Err(std::io::Error::other("recording thread panicked")))
    }

    fn is_finished(&self) -> bool {
        std::thread::JoinHandle::is_finished(self)
    }
}

/// A driver that runs no passes of its own: the caller runs them.
///
/// Hand one to [`Recorder::start_with`] and keep the [`ManualPump`] it was built
/// with. The recorder then behaves exactly as it does over a thread — same stop
/// flag, same once-only shutdown, same `Drop` finalization — while every pass is
/// a call the caller made and can count, so a test can assert exact frame
/// counts instead of sleeping.
///
/// Both drivers run [`PumpLoop::pump_once`] and finalize through
/// [`PumpLoop::finalize`]; only the pacing differs.
///
/// # Examples
///
/// ```
/// use tutti_core::{AudioTap, Samples};
/// use tutti_io::{BitDepth, ManualDriver, PumpPass, Recorder, TapIn, WavOut};
///
/// # fn main() -> tutti_io::Result<()> {
/// let tap = AudioTap::new();
/// let src = TapIn::new(tap.open().expect("a fresh tap has no other reader"));
/// let dir = tempfile::tempdir()?;
/// let wav = WavOut::create(dir.path().join("take.wav"), 48_000.0, 2u16, BitDepth::Float32)?;
///
/// let (driver, pump) = ManualDriver::new();
/// let rec = Recorder::start_with(src, wav, driver)?;
///
/// tap.push(&[0.5, -0.5, 0.25, -0.25], 2);
/// assert_eq!(pump.pump_once(), Some(PumpPass::Wrote(Samples(2))));
/// assert_eq!(pump.pump_once(), Some(PumpPass::Starved));
///
/// rec.stop()?;
/// assert_eq!(pump.pump_once(), None); // finalized
/// # Ok(())
/// # }
/// ```
pub struct ManualDriver {
    /// Shared with the [`ManualPump`] the caller kept. `None` once the take has
    /// been finalized, which is what makes finalize once-only across the two
    /// handles.
    slot: Arc<std::sync::Mutex<Option<Box<dyn ErasedPump>>>>,
}

/// The caller's half of a [`ManualDriver`]: runs passes and reports what they
/// did.
///
/// After the recorder is stopped or dropped the loop is finalized and gone, so
/// [`pump_once`](Self::pump_once) returns `None` — which is how a test observes
/// that a shutdown ran.
pub struct ManualPump {
    slot: Arc<std::sync::Mutex<Option<Box<dyn ErasedPump>>>>,
}

/// [`PumpLoop`] with its endpoint and element types erased.
///
/// The driver is handed a `PumpLoop<I, O, S>` for caller-chosen types, but
/// [`ManualPump`] is built *before* the recorder exists and so cannot name
/// them. Two methods, both already on `PumpLoop`; `finalize` takes `Box<Self>`
/// because it consumes the loop.
trait ErasedPump: Send {
    fn pump_once(&mut self) -> PumpPass;
    fn finalize(self: Box<Self>) -> std::io::Result<()>;
}

impl<I, O, S> ErasedPump for PumpLoop<I, O, S>
where
    S: Copy + Send,
    I: AudioIn<S> + Send,
    O: AudioOut<S> + Send,
{
    fn pump_once(&mut self) -> PumpPass {
        PumpLoop::pump_once(self)
    }
    fn finalize(self: Box<Self>) -> std::io::Result<()> {
        PumpLoop::finalize(*self)
    }
}

impl ManualDriver {
    /// Creates a driver and the handle that runs its passes.
    #[must_use]
    pub fn new() -> (Self, ManualPump) {
        let slot = Arc::new(std::sync::Mutex::new(None));
        (
            Self {
                slot: Arc::clone(&slot),
            },
            ManualPump { slot },
        )
    }
}

impl ManualPump {
    /// Runs one pump pass on the calling thread, or returns `None` if the
    /// recorder has not started yet or the take has been finalized.
    pub fn pump_once(&self) -> Option<PumpPass> {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_mut().map(|l| l.pump_once())
    }

    /// Runs passes until one writes nothing, or `budget` passes have run, and
    /// returns the total frames written.
    ///
    /// The budget bounds a source that never runs dry.
    #[must_use]
    pub fn pump_until_dry(&self, budget: usize) -> Samples {
        let mut total = Samples::ZERO;
        for _ in 0..budget {
            match self.pump_once() {
                Some(PumpPass::Wrote(n)) => total += n,
                _ => break,
            }
        }
        total
    }
}

/// A [`ManualDriver`]'s running take.
///
/// Joining it finalizes the loop and takes it out of the shared slot, after
/// which every [`ManualPump::pump_once`] returns `None`.
pub struct ManualRunning {
    slot: Arc<std::sync::Mutex<Option<Box<dyn ErasedPump>>>>,
}

impl RunningPump for ManualRunning {
    fn join(self: Box<Self>) -> std::io::Result<()> {
        let taken = self.slot.lock().unwrap_or_else(|p| p.into_inner()).take();
        match taken {
            Some(l) => l.finalize(),
            // Already finalized. Only reachable if a caller cloned the running
            // handle, which the type system does not allow — kept as a total
            // match rather than an unreachable panic.
            None => Ok(()),
        }
    }

    /// Always `false`: this driver runs no passes of its own, so it never
    /// stops on its own either. The caller who runs the passes also ends the
    /// take.
    fn is_finished(&self) -> bool {
        false
    }
}

impl PumpDriver for ManualDriver {
    type Running = ManualRunning;

    fn drive<I, O, S>(self, loop_: PumpLoop<I, O, S>, _running: Arc<AtomicBool>) -> Self::Running
    where
        I: AudioIn<S> + Send + 'static,
        O: AudioOut<S> + Send + 'static,
        S: Copy + Send + 'static,
    {
        // The stop flag is unused here on purpose. Nothing runs between the
        // caller's own `pump_once` calls, so there is no loop to break; the
        // recorder's shutdown ends the take by taking the loop out of the slot,
        // and that is what `join` does below.
        *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(loop_));
        ManualRunning { slot: self.slot }
    }
}

/// Pumps any [`AudioIn`] source into any [`AudioOut`] sink on a background
/// thread.
///
/// [`start`](Self::start) checks that source and sink have the same channel
/// width, spawns a dedicated thread that repeatedly polls up to 1024 frames
/// from the source and writes them, and returns once the pump is live. The
/// loop does not allocate. When a live source has nothing ready, the thread
/// parks for 5 ms and polls again; a finite source ends the loop by itself.
///
/// End a take with one of:
///
/// - [`stop`](Self::stop) — for a live source (a microphone, [`TapIn`](crate::TapIn)):
///   stops the thread wherever it is, finalizes the sink and returns the result.
/// - [`wait`](Self::wait) — for a finite source (a decoded file): lets the
///   source reach its end, then finalizes.
/// - [`shutdown`](Self::shutdown) — `stop` without consuming the recorder, for
///   a host that stores the take in place (a Bevy component). Pair it with
///   [`request_stop`](Self::request_stop) and
///   [`is_finished`](Self::is_finished) to end a take without blocking.
/// - dropping the recorder — does what `stop` does and stores the finalize
///   result in [`finalize_status`](Self::finalize_status).
///
/// The sample rate is not checked — [`AudioIn`] carries none — so build the
/// sink at the source's rate (`tutti_cpal::MicIn::matching_sink` does this for
/// a microphone).
///
/// # Examples
///
/// ```no_run
/// # use tutti_io::{Recorder, WavOut, BitDepth};
/// # fn go<I: tutti_core::io::AudioIn + Send + 'static>(src: I) -> tutti_io::Result<()> {
/// let wav = WavOut::create("take.wav", 48_000.0, 2u16, BitDepth::Float32)
///     .expect("sink opens");
/// let rec = Recorder::start(src, wav)?;   // errors if the widths disagree
/// // ... later ...
/// rec.stop()
/// # }
/// ```
pub struct Recorder {
    /// Cleared by [`request_stop`](Self::request_stop) (and so by every
    /// shutdown) to break the pump loop.
    running: Arc<AtomicBool>,
    /// The running pump, joinable for the `finalize` result. `Option` because
    /// taking it is what makes the shutdown once-only: `stop`, `wait`,
    /// `shutdown` and `drop` all join through it, and whichever runs first
    /// leaves `None` behind for the others.
    ///
    /// Boxed rather than generic on the driver: `Recorder` is one type a caller
    /// stores in a field, and making it `Recorder<D>` would push the choice of
    /// driver into every signature that holds a take. The endpoints and the
    /// sample element are erased into the driver for the same reason.
    handle: Option<Box<dyn RunningPump>>,
    /// Where a shutdown that cannot return its result leaves it.
    ///
    /// Only `drop` writes here — every other shutdown returns the same value to
    /// its caller instead. It is a *shared* cell rather than a plain field so a
    /// caller can hold `finalize_status` across the drop and read the outcome
    /// afterwards, which is the only moment the answer exists on that path.
    ///
    /// `None` means no drop-path finalize has completed: either another
    /// shutdown ran, or the recorder is still alive.
    status: Arc<FinalizeStatus>,
}

/// The finalize outcome of a [`Recorder`] that was dropped rather than
/// stopped.
///
/// Take it from [`Recorder::finalize_status`] *before* the recorder is
/// dropped, and read it afterwards. [`Recorder::stop`] returns the same result
/// directly and is the path to prefer; `Drop` cannot return anything, and a
/// failed finalize means an unpatched WAV header — an unreadable file — so the
/// error is stored here rather than lost.
#[derive(Default)]
pub struct FinalizeStatus {
    /// Set once the drop-path finalize has run, whatever its outcome.
    /// Separate from the message so "succeeded" and "has not happened" are
    /// distinguishable without locking.
    done: AtomicBool,
    /// The error text, when the drop-path finalize failed. Behind a `Mutex`
    /// because `io::Error` is neither `Copy` nor cheap to publish atomically,
    /// and this is written at most once, off any hot path.
    error: std::sync::Mutex<Option<String>>,
}

impl FinalizeStatus {
    /// Returns whether a drop-path finalize has completed.
    ///
    /// `false` after [`Recorder::stop`]: that path returns the result directly
    /// and deliberately leaves nothing here.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Returns the drop-path finalize error's message, if there was one.
    ///
    /// `None` covers two cases that [`is_done`](Self::is_done) separates: the
    /// finalize succeeded, or it has not run.
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Record a completed drop-path finalize.
    fn set(&self, result: std::io::Result<()>) {
        if let Err(e) = result {
            *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
        }
        // Released last, so a reader that observes `is_done` also observes the
        // message written above it.
        self.done.store(true, Ordering::Release);
    }
}

impl Recorder {
    /// Spawns a background thread pumping `src` into `sink`. Returns once
    /// the pump is live.
    ///
    /// The **sample rate** is the caller's to match: [`AudioIn`] carries none,
    /// so nothing here can compare them. For a microphone, build the sink with
    /// `tutti_cpal::MicIn::matching_sink`.
    ///
    /// # Errors
    ///
    /// [`Error::ChannelWidthMismatch`] when `src` and `sink` disagree on
    /// channel width, before any frame is written.
    pub fn start<I, O, S>(src: I, sink: O) -> Result<Self>
    where
        I: AudioIn<S> + Send + 'static,
        O: AudioOut<S> + Send + 'static,
        S: Default + Copy + Send + 'static,
    {
        Self::start_with(src, sink, ThreadDriver::default())
    }

    /// Starts like [`start`](Self::start), with the caller choosing how the
    /// pump runs.
    ///
    /// The width check, the stop flag, the once-only shutdown and the `Drop`
    /// finalization are all identical — a driver decides only *where the loop
    /// runs and how it waits*, never when the take ends. Use a
    /// [`ManualDriver`] to run passes by hand in a test, or
    /// [`ThreadDriver::with_park`] to tune the back-off.
    ///
    /// # Errors
    ///
    /// [`Error::ChannelWidthMismatch`] when `src` and `sink` disagree on width,
    /// checked before the driver is handed anything — so no frame is ever
    /// written to a file whose channels would rotate.
    pub fn start_with<I, O, S, D>(src: I, sink: O, driver: D) -> Result<Self>
    where
        I: AudioIn<S> + Send + 'static,
        O: AudioOut<S> + Send + 'static,
        S: Default + Copy + Send + 'static,
        D: PumpDriver,
        D::Running: 'static,
    {
        Self::start_with_capacity(src, sink, SCRATCH_FRAMES, driver)
    }

    /// Starts like [`start_with`](Self::start_with), moving at most `capacity`
    /// frames per pass.
    ///
    /// `capacity` is in **frames** — a [`Samples`], so a caller cannot hand
    /// over a sample count by mistake and get a buffer `width` times too large.
    /// The scratch is allocated once, sized
    /// [`capacity.interleaved_len(layout)`](Samples::interleaved_len) from the
    /// width both endpoints agreed on, so the loop body never allocates. A zero
    /// capacity is taken as one frame: an empty scratch would make every poll
    /// return zero, ending a finite take before its first frame and parking a
    /// live one forever.
    ///
    /// # Errors
    ///
    /// [`Error::ChannelWidthMismatch`], as [`start_with`](Self::start_with).
    pub fn start_with_capacity<I, O, S, D>(
        src: I,
        sink: O,
        capacity: Samples,
        driver: D,
    ) -> Result<Self>
    where
        I: AudioIn<S> + Send + 'static,
        O: AudioOut<S> + Send + 'static,
        S: Default + Copy + Send + 'static,
        D: PumpDriver,
        D::Running: 'static,
    {
        let layout = src.layout();
        if layout != sink.layout() {
            return Err(Error::ChannelWidthMismatch {
                src: layout,
                sink: sink.layout(),
            });
        }
        // Sized once, here, from the width both endpoints agreed on above.
        let frames = if capacity.is_zero() {
            Samples(1)
        } else {
            capacity
        };
        let scratch_samples = frames.interleaved_len(layout);

        let running = Arc::new(AtomicBool::new(true));
        let pump_loop = PumpLoop {
            src,
            sink,
            // Allocated once; `pump_once` never allocates.
            scratch: vec![S::default(); scratch_samples],
        };

        let handle = driver.drive(pump_loop, Arc::clone(&running));

        Ok(Self {
            running,
            handle: Some(Box::new(handle)),
            status: Arc::new(FinalizeStatus::default()),
        })
    }

    /// Stops the pump, waits for its thread, and finalizes the sink.
    ///
    /// Blocks for at most one pass (or one park). Frames the source has not
    /// yet delivered are not recorded; for a finite source that should run to
    /// its end, use [`wait`](Self::wait). Dropping the recorder does the same
    /// as `stop` but can only store the result in
    /// [`finalize_status`](Self::finalize_status). After
    /// [`shutdown`](Self::shutdown) has already returned the result, this
    /// returns `Ok(())`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if finalizing failed (for a WAV, the header was left
    /// unpatched and the file is unreadable) or the pump thread panicked.
    pub fn stop(mut self) -> Result<()> {
        // `self` is consumed, so the `Drop` that follows finds `handle` already
        // taken and does nothing — the join happens exactly once.
        Ok(self.shutdown().unwrap_or(Ok(()))?)
    }

    /// Waits for a **finite** take to reach its own end, then finalizes.
    ///
    /// [`stop`](Self::stop) cuts a source off wherever the pump happens to be,
    /// which silently truncates a finite one. This instead lets the loop run
    /// until the source reports
    /// [`OnEmpty::EndOfStream`](tutti_core::io::OnEmpty::EndOfStream), then
    /// returns the finalize result as `stop` does.
    ///
    /// **On a [`Starved`](tutti_core::io::OnEmpty::Starved) source this blocks
    /// forever**, and every microphone capture is one — the pump parks and
    /// re-polls rather than ending. Choose the method that matches your
    /// source's `ON_EMPTY`. After [`shutdown`](Self::shutdown) has already
    /// returned the result, this returns `Ok(())` without blocking.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if finalizing failed or the pump thread panicked.
    pub fn wait(mut self) -> Result<()> {
        // Deliberately not `shutdown()`: the one line that differs is the one
        // that would truncate the take.
        Ok(self.join_pump().unwrap_or(Ok(()))?)
    }

    /// Asks the pump to finish, without waiting for it. Idempotent.
    ///
    /// The driver notices on its next pass, finalizes, and stops;
    /// [`is_finished`](Self::is_finished) then reads `true`, and
    /// [`shutdown`](Self::shutdown) collects the result without blocking.
    pub fn request_stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Whether the pump is still meant to run: `false` once a stop was
    /// requested.
    ///
    /// This reports the *flag*, not the driver: a finite source that ran out
    /// has stopped while this still reads `true`. Ask
    /// [`is_finished`](Self::is_finished) for that.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Whether the pump has stopped running passes, so
    /// [`shutdown`](Self::shutdown) will not block.
    ///
    /// Never blocks. `true` once a finite source has ended its own take, or
    /// once a requested stop has been noticed. `false` after a shutdown, since
    /// there is no pump left to report on; a [`ManualDriver`] take is never
    /// finished on its own (its caller runs the passes).
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| h.is_finished())
    }

    /// Stops the pump, joins it and returns the sink's finalize result, leaving
    /// the recorder in place.
    ///
    /// [`stop`](Self::stop) for a host that stores the take where it cannot be
    /// moved out (a component, a field). `None` when a shutdown already ran,
    /// which is what makes a second finalize unrepresentable rather than a
    /// panic. Blocks for at most one pass (or one park); after
    /// [`is_finished`](Self::is_finished) it does not block at all.
    ///
    /// Clearing the stop flag is the half that matters for a **live** source.
    /// The pump loop exits on its own only for a finite one
    /// ([`OnEmpty::EndOfStream`] breaks); a starving source — every microphone
    /// capture — parks and re-polls forever, so nothing else ever reaches the
    /// `finalize` below it.
    pub fn shutdown(&mut self) -> Option<std::io::Result<()>> {
        self.request_stop();
        self.join_pump()
    }

    /// Returns the handle where a drop-path finalize leaves its outcome.
    ///
    /// Take it *before* dropping the recorder; it outlives the recorder. After
    /// [`stop`](Self::stop), [`wait`](Self::wait) or
    /// [`shutdown`](Self::shutdown) it stays untouched, since those return the
    /// result directly.
    pub fn finalize_status(&self) -> Arc<FinalizeStatus> {
        Arc::clone(&self.status)
    }

    /// Join the pump and surface its finalize result, leaving `running`
    /// alone.
    ///
    /// The half [`shutdown`](Self::shutdown) and [`wait`](Self::wait) share,
    /// and the whole difference between them is the flag clear before this
    /// call. Taking `handle` is what makes either once-only.
    fn join_pump(&mut self) -> Option<std::io::Result<()>> {
        // The driver finalizes the sink and returns that io::Result.
        self.handle.take().map(|handle| handle.join())
    }
}

/// Stop, join and finalize a recorder nobody shut down.
///
/// Without this, dropping a live recorder leaks the pump thread *and* the
/// take: the loop only breaks on `running`, so a
/// [`Starved`](OnEmpty::Starved) source parks and re-polls forever,
/// `finalize` never runs, and the WAV header is never back-patched, which
/// leaves the file unreadable.
///
/// The finalize `Result` has nowhere to go from here, and this workspace does
/// not panic in `Drop`. It is stored in [`FinalizeStatus`] instead, which a
/// caller reads through a handle taken before the drop.
impl Drop for Recorder {
    fn drop(&mut self) {
        // `stop`, `wait` and `shutdown` already took `handle`, so this is a
        // no-op on those paths and must not overwrite the status they
        // deliberately left clear.
        if let Some(result) = self.shutdown() {
            self.status.set(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WavOut;
    use tutti_core::io::{AudioIn, OnEmpty};
    use tutti_core::pcm::BitDepth;
    use tutti_core::ChannelLayout;

    /// A finite in-memory [`AudioIn`] standing in for a live mic: hands out its
    /// frames in bounded chunks, returning a short-then-zero count at
    /// end-of-stream. Lets the pump→`WavOut`→readback path be proven without
    /// touching real hardware. Has a runtime width, like the `SliceSource`
    /// fixture in `tutti_types::io`'s own tests.
    struct SliceSource {
        /// Flat interleaved at `layout`'s width.
        samples: Vec<f32>,
        /// Read cursor, in FRAMES.
        pos: usize,
        layout: ChannelLayout,
    }

    impl AudioIn for SliceSource {
        const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

        fn layout(&self) -> ChannelLayout {
            self.layout
        }

        fn poll_into(&mut self, out: &mut [f32]) -> Samples {
            let ch = self.layout.count() as usize;
            let total = self.samples.len() / ch;
            let n = (total - self.pos).min(out.len() / ch);
            out[..n * ch].copy_from_slice(&self.samples[self.pos * ch..(self.pos + n) * ch]);
            self.pos += n;
            Samples(n)
        }
    }

    /// Pumping a fake source into a real [`WavOut`] and finalizing yields a WAV
    /// whose frame count matches what was fed — the round trip the `Recorder`
    /// runs, minus only the mic and the thread (both untestable without a
    /// device). Proves `WavOut::create` is reachable from this crate and that
    /// the pump moves every frame into a readable file.
    #[test]
    fn pump_into_wav_sink_round_trips_frame_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recorded.wav");

        const FRAMES: usize = 3000;
        let samples: Vec<f32> = (0..FRAMES)
            .flat_map(|i| [i as f32 / 3000.0, -(i as f32) / 3000.0])
            .collect();
        let mut src = SliceSource {
            samples,
            pos: 0,
            layout: ChannelLayout::STEREO,
        };
        let mut wav =
            WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink should open");

        // The exact loop the pump thread runs — allocate the scratch once, drain
        // to exhaustion. (No idle-park: the fake source never returns 0 early.)
        let mut scratch = vec![0.0f32; SCRATCH_FRAMES.interleaved_len(ChannelLayout::STEREO)];
        while !pump(&mut src, &mut wav, &mut scratch).is_zero() {}
        wav.finalize()
            .expect("finalize should back-patch the header");

        let reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 48_000);
        // Two samples (L, R) per stereo frame.
        assert_eq!(reader.len() as usize, FRAMES * 2);
    }

    /// **The replacement for the lost compile error, outer half.**
    ///
    /// A width mismatch is a returned error, checked BEFORE the thread spawns,
    /// so no frame is ever written to a file whose channels would rotate. This
    /// is the guarantee the runtime `ChannelLayout` gave up at the type level.
    ///
    /// The matching pair must still be accepted, which is the half that stops
    /// this from passing vacuously.
    #[test]
    fn start_rejects_a_source_sink_width_mismatch() {
        let dir = tempfile::tempdir().unwrap();

        let stereo_src = || SliceSource {
            samples: vec![0.0f32; 64],
            pos: 0,
            layout: ChannelLayout::STEREO,
        };

        // Stereo source, 6-channel sink: refused.
        let wide_path = dir.path().join("mismatch.wav");
        let wide =
            WavOut::create(&wide_path, 48_000.0, 6u16, BitDepth::Float32).expect("sink opens");
        let err = Recorder::start(stereo_src(), wide)
            .err()
            .expect("a width mismatch must be refused, not recorded");
        // The typed variant carries both layouts as values, so a caller can
        // branch on the widths without parsing the message.
        assert!(
            matches!(
                err,
                Error::ChannelWidthMismatch { src, sink }
                    if src == ChannelLayout::STEREO && sink == ChannelLayout::from(6u16)
            ),
            "expected ChannelWidthMismatch carrying both layouts, got: {err:?}"
        );
        assert!(
            err.to_string().contains("Stereo") && err.to_string().contains("6"),
            "the error must name both widths, got: {err}"
        );

        // Stereo source, mono sink: also refused. A narrowing mismatch is just
        // as wrong as a widening one, and it is the direction a caller is most
        // likely to think "it'll just downmix".
        let mono_path = dir.path().join("mismatch_mono.wav");
        let mono =
            WavOut::create(&mono_path, 48_000.0, 1u16, BitDepth::Float32).expect("sink opens");
        assert!(Recorder::start(stereo_src(), mono).is_err());

        // And the matching pair is accepted.
        let ok_path = dir.path().join("match.wav");
        let ok = WavOut::create(&ok_path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");
        let rec = Recorder::start(stereo_src(), ok).expect("matching widths must be accepted");
        rec.stop().expect("finalize");
    }

    /// A non-stereo pairing records end to end, and every count on the way
    /// through stays denominated in FRAMES.
    ///
    /// At six channels a sample-denominated count is 6× off, so the frame total
    /// read back off the finished file is a direct check on the whole chain:
    /// `poll_into` → `pump` → `write` → header.
    #[test]
    fn a_six_channel_source_records_at_its_own_width() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("surround.wav");

        const FRAMES: usize = 500;
        const CH: usize = 6;
        let src = SliceSource {
            samples: (0..FRAMES * CH).map(|i| (i % 97) as f32 * 0.001).collect(),
            pos: 0,
            layout: ChannelLayout::from(6u16),
        };
        let wav = WavOut::create(
            &path,
            48_000.0,
            ChannelLayout::from(6u16),
            BitDepth::Float32,
        )
        .expect("sink opens");

        let (driver, pump) = ManualDriver::new();
        let rec = Recorder::start_with(src, wav, driver).expect("matching 6ch widths");

        // Drain by counting, not by sleeping: `stop` clears the run flag
        // immediately and a threaded pump can exit before its first pass. Here
        // the passes are the caller's, so "drained" is a fact.
        let written = pump.pump_until_dry(16);
        assert_eq!(
            written,
            Samples(FRAMES),
            "the pump moved {written} frames of {FRAMES} -- a short drain here would \
             make the header assertion below measure the harness"
        );
        rec.stop().expect("finalize");

        let reader = hound::WavReader::open(&path).expect("readable");
        assert_eq!(reader.spec().channels, 6);
        assert_eq!(
            reader.len() as usize,
            FRAMES * CH,
            "500 FRAMES of 6 channels is 3000 samples — not 500, and not 18000"
        );
    }

    /// A live (starving) source records until stopped — the composability a
    /// generic `AudioIn` bound buys over a signature that opens a device itself.
    ///
    /// The fixture withholds frames without being exhausted, so a recorder that
    /// mistook a zero-frame poll for end-of-stream would stop early.
    #[test]
    fn a_non_mic_live_source_records_until_stopped() {
        struct Starving {
            polls: std::sync::atomic::AtomicUsize,
        }

        impl AudioIn for Starving {
            const ON_EMPTY: OnEmpty = OnEmpty::Starved;

            fn layout(&self) -> ChannelLayout {
                ChannelLayout::STEREO
            }

            fn poll_into(&mut self, out: &mut [f32]) -> Samples {
                let n = self.polls.fetch_add(1, Ordering::Relaxed);
                // Every other poll is empty — never exhausted.
                if n % 2 == 1 || out.len() < 2 {
                    return Samples::ZERO;
                }
                out[0] = 0.25;
                out[1] = -0.25;
                Samples(1)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generated.wav");
        let wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

        let (driver, pump) = ManualDriver::new();
        let rec = Recorder::start_with(
            Starving {
                polls: std::sync::atomic::AtomicUsize::new(0),
            },
            wav,
            driver,
        )
        .expect("matching stereo widths");

        // Six passes of a fixture that yields on every *other* poll: passes
        // 0, 2, 4 write a frame each and 1, 3, 5 report `Starved`. Nothing here
        // is a guess.
        //
        // The sequence below fails if a `Starved` pass is treated as end-of-stream
        // (writes stop at 1), and it fails if the parity of the fixture changes,
        // which is the only other way the count can move.
        let mut wrote = Samples::ZERO;
        for pass in 0..6 {
            let outcome = pump.pump_once().expect("the take is live");
            match (pass % 2, outcome) {
                (0, PumpPass::Wrote(n)) => wrote += n,
                (1, PumpPass::Starved) => {}
                (_, other) => panic!(
                    "pass {pass} of a source that yields every other poll reported {other:?} \
                     -- a `Starved` pass read as end-of-stream ends a live take silently"
                ),
            }
        }
        assert_eq!(wrote, Samples(3), "three yielding passes, one frame each");

        rec.stop().expect("finalize should succeed");

        let reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
        let frames = reader.len() as usize / 2; // two samples per stereo frame
        assert_eq!(
            frames, 3,
            "the header must report exactly the frames the counted passes wrote"
        );
    }

    /// A live source that never ends on its own, for the `Drop` tests.
    ///
    /// [`Starved`](OnEmpty::Starved) is the whole point: a finite source's pump
    /// loop breaks by itself, so a `Drop` bug is invisible with one. This never
    /// runs dry, so only `Drop` (or `stop`) can end the take.
    struct AlwaysLive;

    impl AudioIn for AlwaysLive {
        const ON_EMPTY: OnEmpty = OnEmpty::Starved;

        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }

        fn poll_into(&mut self, out: &mut [f32]) -> Samples {
            let frames = Samples::from_interleaved_len(out.len(), ChannelLayout::STEREO);
            for (i, s) in out[..frames.interleaved_len(ChannelLayout::STEREO)]
                .iter_mut()
                .enumerate()
            {
                *s = if i % 2 == 0 { 0.5 } else { -0.5 };
            }
            frames
        }
    }

    /// **The `Drop` guarantee.** A live recorder dropped without `stop` still
    /// finalizes, so the WAV is readable.
    ///
    /// Before the `Drop` impl this file was unreadable: the pump loop for a
    /// `Starved` source has no exit but the `running` flag, so `finalize` never
    /// ran and the RIFF/data sizes stayed at their placeholder values. The
    /// audio was on disk the whole time — only the header disagreed, which is
    /// what makes the failure silent.
    ///
    /// Mutation check: delete `impl Drop for Recorder` and this fails at
    /// `WavReader::open` (and the thread it leaks keeps running).
    #[test]
    fn dropping_a_live_recorder_still_finalizes_the_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dropped.wav");
        let wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

        let (driver, pump) = ManualDriver::new();
        let status = {
            let rec =
                Recorder::start_with(AlwaysLive, wav, driver).expect("matching stereo widths");
            let status = rec.finalize_status();
            // Land a known number of frames, then drop without calling `stop`.
            // Three passes is a count, not a sleep.
            for _ in 0..3 {
                assert!(
                    matches!(pump.pump_once(), Some(PumpPass::Wrote(n)) if !n.is_zero()),
                    "AlwaysLive fills every buffer it is handed"
                );
            }
            status
        };

        assert!(
            status.is_done(),
            "the drop path must run finalize and record that it did"
        );
        assert_eq!(
            status.error(),
            None,
            "finalize should have succeeded on a healthy sink"
        );
        // The take is over: the loop is out of the slot and no pass can run.
        // A direct observation of the shutdown, rather than inferring it from
        // the file below.
        assert_eq!(
            pump.pump_once(),
            None,
            "the dropped recorder must have taken the pump loop, not merely stopped polling it"
        );

        let reader = hound::WavReader::open(&path)
            .expect("a dropped recorder must still leave a readable WAV");
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(
            reader.len() as usize,
            (SCRATCH_FRAMES * 3).interleaved_len(ChannelLayout::STEREO),
            "the header must report exactly the frames the three counted passes wrote"
        );
    }

    /// **The other half of the `Drop` guarantee: a finalize that *fails* there
    /// still reaches the caller.**
    ///
    /// The sibling test above proves the drop path runs finalize and records
    /// success. Nothing proved it records a *failure* — `error()` returning
    /// `Some` had no coverage at all, so the entire reason
    /// [`FinalizeStatus`] carries an error rather than just a done flag was
    /// untested. `Drop` cannot return a `Result` and this workspace does not
    /// panic in `Drop`, so this handle is the only path a disk-full on the last
    /// block has to the caller; if it silently dropped the error, a truncated
    /// take would look like a clean one.
    ///
    /// The failure is provoked, not simulated: `narrow_header_for_test` opens
    /// an 8-bit header that the sink then quantizes into at `Int16`, so
    /// `AlwaysLive`'s ±0.5 (±16383 at `Int16`) is rejected by `hound` with
    /// `TooWide` on the write. See that constructor for why this shape rather
    /// than a permission or disk trick.
    ///
    /// Mutation-checked: deleting the `self.status.set(result)` line from
    /// `Drop` — or narrowing it to store only successes — fails this test while
    /// leaving every other test in the file green.
    #[test]
    fn a_finalize_that_fails_on_the_drop_path_is_recorded_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doomed.wav");
        let wav = WavOut::narrow_header_for_test(&path);

        let (driver, pump) = ManualDriver::new();
        let status = {
            let rec = Recorder::start_with(AlwaysLive, wav, driver)
                .expect("the fixture sink is stereo, as AlwaysLive is");
            let status = rec.finalize_status();
            // One pass is enough: the first sample is already too wide, and
            // `WavOut` latches `first_error` and stops writing from there.
            assert!(
                matches!(pump.pump_once(), Some(PumpPass::Wrote(n)) if !n.is_zero()),
                "the pump reports frames moved; the sink's failure is latched, not returned"
            );
            status
        };

        assert!(
            status.is_done(),
            "the drop path must run finalize even when it is going to fail"
        );
        let err = status
            .error()
            .expect("a failed finalize on the drop path must be recorded, not swallowed");
        // The specific cause survives — a caller can tell a too-wide sample
        // from a full disk. Flattening to "finalize failed" would make the two
        // indistinguishable, which is the information loss `WavOut::finalize`
        // already refuses to accept.
        assert!(
            err.contains("more bits than the destination type"),
            "the underlying hound error should be carried, got: {err}"
        );
    }

    /// `stop` and `Drop` cannot both join: `stop` consumes the recorder, so the
    /// `Drop` that immediately follows finds the handle already taken.
    ///
    /// The observable half is that `stop` still returns the finalize result and
    /// the drop path leaves `FinalizeStatus` untouched — a second join would
    /// panic on the already-consumed `JoinHandle` long before this assertion,
    /// so reaching it at all is most of the proof.
    #[test]
    fn stop_and_drop_do_not_both_join() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stopped.wav");
        let wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

        let (driver, pump) = ManualDriver::new();
        let rec = Recorder::start_with(AlwaysLive, wav, driver).expect("matching stereo widths");
        let status = rec.finalize_status();
        assert!(matches!(pump.pump_once(), Some(PumpPass::Wrote(_))));
        rec.stop().expect("stop returns the finalize result");

        // Same observation as the drop test, and it is what makes this one
        // non-vacuous: `stop` took the loop, so there is nothing left to join a
        // second time.
        assert_eq!(pump.pump_once(), None, "`stop` must take the pump loop");

        assert!(
            !status.is_done(),
            "`stop` returns the result itself; the drop path must not also \
             record one, or a caller cannot tell which shutdown ran"
        );
        assert!(hound::WavReader::open(&path).is_ok());
    }

    /// A sink that is not a WAV, over an element that is not `f32`: the pump
    /// is generic over both, and a test sink can be read back directly.
    #[derive(Clone)]
    struct Collect(Arc<std::sync::Mutex<Vec<i16>>>);

    impl AudioOut<i16> for Collect {
        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }
        fn write(&mut self, interleaved: &[i16]) {
            self.0.lock().unwrap().extend_from_slice(interleaved);
        }
        fn finalize(self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A finite stereo `i16` ramp: frame `k` is `(k, -k)`.
    struct RampI16 {
        frames: usize,
        pos: usize,
    }

    impl AudioIn<i16> for RampI16 {
        const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }

        fn poll_into(&mut self, out: &mut [i16]) -> Samples {
            let fits = Samples::from_interleaved_len(out.len(), ChannelLayout::STEREO).get();
            let n = (self.frames - self.pos).min(fits);
            for i in 0..n {
                let k = (self.pos + i) as i16;
                out[i * 2] = k;
                out[i * 2 + 1] = -k;
            }
            self.pos += n;
            Samples(n)
        }
    }

    /// **A named capacity bounds every pass**, over any sink and element.
    ///
    /// `bevy-tutti`'s `AudioPump` hands its capacity through here, so this is
    /// the property its `Samples(64)` relies on: 1000 frames at 64 per pass is
    /// fifteen full passes and a 40-frame tail, then `Ended`. The values are
    /// read back from the sink, so a pass that skipped or repeated frames fails
    /// on the content, not just the count.
    ///
    /// *Mutation:* size the scratch from `SCRATCH_FRAMES` instead of `frames`
    /// in `start_with_capacity`. The first pass then writes all 1000 frames and
    /// the `Wrote(64)` assertion fails.
    #[test]
    fn a_named_capacity_bounds_each_pass_over_any_sink() {
        let seen = Collect(Arc::new(std::sync::Mutex::new(Vec::new())));
        let (driver, pump) = ManualDriver::new();
        let rec = Recorder::start_with_capacity(
            RampI16 {
                frames: 1000,
                pos: 0,
            },
            seen.clone(),
            Samples(64),
            driver,
        )
        .expect("stereo into stereo");

        for pass in 0..15 {
            assert_eq!(
                pump.pump_once(),
                Some(PumpPass::Wrote(Samples(64))),
                "pass {pass} must move exactly the named capacity"
            );
        }
        assert_eq!(pump.pump_once(), Some(PumpPass::Wrote(Samples(40))));
        assert_eq!(pump.pump_once(), Some(PumpPass::Ended));
        rec.stop().expect("a collecting sink finalizes cleanly");

        let got = seen.0.lock().unwrap();
        assert_eq!(got.len(), 2000, "1000 stereo frames are 2000 samples");
        for (k, frame) in got.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(*frame, [k as i16, -(k as i16)], "frame {k}");
        }
    }

    /// **A zero capacity is one frame, not a take that never moves.**
    ///
    /// An empty scratch makes every poll return zero, and the source's
    /// `ON_EMPTY` then reads that zero as its own: a finite take ends before
    /// its first frame, a live one parks forever. `AudioPump` has always
    /// clamped to one frame; the clamp moved here with it.
    ///
    /// *Mutation:* drop the clamp (`let frames = capacity;`). The first pass
    /// then reports `Ended` with nothing written and this fails.
    #[test]
    fn a_zero_capacity_moves_one_frame_per_pass() {
        let seen = Collect(Arc::new(std::sync::Mutex::new(Vec::new())));
        let (driver, pump) = ManualDriver::new();
        let rec = Recorder::start_with_capacity(
            RampI16 { frames: 3, pos: 0 },
            seen.clone(),
            Samples::ZERO,
            driver,
        )
        .expect("stereo into stereo");

        assert_eq!(pump.pump_until_dry(16), Samples(3));
        assert_eq!(pump.pump_once(), Some(PumpPass::Ended));
        rec.stop().expect("finalize");
        assert_eq!(*seen.0.lock().unwrap(), [0, 0, 1, -1, 2, -2]);
    }

    /// **`shutdown` finalizes in place, once.**
    ///
    /// A component holding a take cannot move it into `stop`, so it calls
    /// `shutdown` from a `&mut`. The first call returns the finalize result;
    /// every later one returns `None`, and the `Drop` that follows records
    /// nothing, since the caller already holds the result.
    ///
    /// *Mutations:* make `join_pump` return `Some(Ok(()))` when the handle is
    /// gone (the second-call assertion fails); make `Drop` call
    /// `self.status.set(Ok(()))` unconditionally (the `is_done` assertion
    /// fails).
    #[test]
    fn shutdown_finalizes_in_place_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in_place.wav");
        let wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens");

        let (driver, pump) = ManualDriver::new();
        let mut rec =
            Recorder::start_with(AlwaysLive, wav, driver).expect("matching stereo widths");
        let status = rec.finalize_status();
        assert!(matches!(pump.pump_once(), Some(PumpPass::Wrote(_))));

        assert!(rec.is_running());
        assert!(
            matches!(rec.shutdown(), Some(Ok(()))),
            "the first shutdown returns the finalize result"
        );
        assert!(!rec.is_running(), "a shutdown clears the run flag");
        assert!(
            rec.shutdown().is_none(),
            "a second shutdown has nothing to join and must say so"
        );
        assert!(!rec.is_finished(), "no pump is left to report on");
        assert_eq!(pump.pump_once(), None, "the loop is finalized and gone");

        drop(rec);
        assert!(
            !status.is_done(),
            "the drop after a shutdown must not record a second finalize"
        );
        let reader = hound::WavReader::open(&path).expect("finalized in place");
        assert_eq!(
            reader.len() as usize,
            SCRATCH_FRAMES.interleaved_len(ChannelLayout::STEREO)
        );
    }
}
