//! [`Recorder`] — the background pump that drives any live source into a WAV.
//!
//! Recording is `pump(src, wav)`: poll a block of frames from an
//! [`AudioIn`], write it to the
//! [`AudioOut`] WAV sink, repeat. This is the caller
//! side of [`tutti_core::io::pump`] — the loop and the stop policy that the pump
//! function itself deliberately leaves out.
//!
//! # It takes a source, not a device
//!
//! [`start`](Recorder::start) is generic over the [`AudioIn`], so a microphone
//! is one option among several: a socket, a decoded file, or a generated signal
//! record identically. That is what keeps this crate device-free and lets it sit
//! *below* `tutti-cpal` rather than inside it — a host opens its own `MicIn` and
//! hands it over.
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
//! wall clock. The four tests here slept 20–50 ms and one of them asserted
//! `frames >= 3` with a comment saying the floor had been tuned against a loaded
//! machine — which is an assertion about the test host, not about the engine. A
//! driven loop makes the same claims by *counting passes*: "three pumps of a
//! source that yields on every other poll wrote two frames" is exact, and it
//! fails for the reason it names.
//!
//! # Dropping is a shutdown, not a leak
//!
//! Ending a take is the *only* way `finalize` runs, so it cannot be left to a
//! caller remembering to ask. [`Drop`] performs the same stop-and-join
//! [`stop`](Recorder::stop) does — the two share one `shutdown`, and the taken
//! `JoinHandle` is what stops them joining twice.
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
//! [`FinalizeStatus`] handle taken beforehand. [`stop`](Recorder::stop) remains
//! the path that hands the result straight back.
//!
//! The scratch buffer is allocated once before the loop, sized
//! `SCRATCH_FRAMES * channels` samples at the source's own width; the loop body
//! never allocates.
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
//! disagree. That check is not incidental bookkeeping: it is the **replacement**
//! for a compile-time guarantee deliberately given up. `AudioIn` and `AudioOut`
//! carry the frame width as a runtime
//! [`ChannelLayout`](tutti_core::ChannelLayout), not a `const CH`, because only
//! a runtime width can express one that comes from *data* — a decoded file, a
//! surround capture device. The cost is that "a stereo mic cannot feed a
//! 6-channel WAV" stopped being a type error. Per the project rule that an
//! omitted guarantee ships with its replacement, it is restored as two runtime
//! checks: a `debug_assert` on the two layouts inside [`pump`], and **the error
//! `start` returns — which lives in this crate.**
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

use crate::error::{Error, Result};
use crate::wav_out::WavOut;

/// Frames moved per pump pass. One bufferful, allocated once before the loop so
/// the pump body stays allocation-free. ~21ms at 48kHz — small enough to bound
/// how far the sink can lag the source, large enough to amortize per-pass cost.
const SCRATCH_FRAMES: usize = 1024;

/// How long [`ThreadDriver`] parks when a [`Starved`](OnEmpty::Starved) source
/// yields nothing. A live mic has nothing ready between callback pushes;
/// sleeping avoids busy-spinning a core while still draining the ring far
/// faster than it can overrun.
///
/// This belongs to the *driver*, not to the pump: it is a pacing decision about
/// a thread, and [`PumpLoop`] has neither.
const IDLE_PARK: Duration = Duration::from_millis(5);

/// What one [`PumpLoop::pump_once`] pass achieved.
///
/// The two "nothing moved" cases are separated because
/// [`ON_EMPTY`](AudioIn::ON_EMPTY) is what distinguishes them and a driver must
/// act differently on each: park and re-poll, or stop. Collapsing them into a
/// frame count would put that decision back at every call site, which is the
/// mistake the const exists to prevent — reading a live source as finite ends a
/// take milliseconds in, with no error anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpPass {
    /// Frames moved from the source into the sink.
    Wrote(usize),
    /// The source had nothing ready *yet* ([`OnEmpty::Starved`]). Park and come
    /// back; there is more coming.
    Starved,
    /// The source has nothing more, ever ([`OnEmpty::EndOfStream`]). Stop.
    Ended,
}

/// One source→sink pump, with no thread and no clock.
///
/// Owns both endpoints and the scratch buffer, which is what lets
/// [`finalize`](AudioOut::finalize) — a once-only consuming call — have an
/// unambiguous home. [`pump_once`](Self::pump_once) moves at most
/// `SCRATCH_FRAMES` frames and allocates nothing.
///
/// Built by [`Recorder::start`] after it has checked the two widths agree, and
/// handed to a [`PumpDriver`].
pub struct PumpLoop<I: AudioIn> {
    src: I,
    sink: WavOut,
    /// Allocated once at construction; `pump_once` never grows it.
    scratch: Vec<f32>,
}

impl<I: AudioIn> PumpLoop<I> {
    /// One pass: poll a block of frames and write them.
    ///
    /// A zero-frame poll is reported as [`Starved`](PumpPass::Starved) or
    /// [`Ended`](PumpPass::Ended) according to the source's own
    /// [`ON_EMPTY`](AudioIn::ON_EMPTY) — the one place that const is spent.
    pub fn pump_once(&mut self) -> PumpPass {
        match pump(&mut self.src, &mut self.sink, &mut self.scratch) {
            0 => match I::ON_EMPTY {
                OnEmpty::Starved => PumpPass::Starved,
                OnEmpty::EndOfStream => PumpPass::Ended,
            },
            n => PumpPass::Wrote(n),
        }
    }

    /// Consume the loop and close the WAV, back-patching its header.
    ///
    /// Once-only by construction: it takes `self`, so a driver that has
    /// finalized cannot pump again, and one that has not cannot have finalized
    /// twice.
    pub fn finalize(self) -> std::io::Result<()> {
        self.sink.finalize()
    }
}

/// How a [`PumpLoop`] is *run*.
///
/// [`Recorder`] owns the take — the stop flag, the once-only shutdown, the
/// finalize outcome — and delegates only the execution. That is the whole
/// division: a driver decides where the loop runs and how it waits, and decides
/// nothing about when a take ends.
///
/// Two implementations ship. [`ThreadDriver`] is what a host gets; [`ManualDriver`]
/// exists so a test can run passes and count frames instead of sleeping and
/// hoping. Both must finalize exactly once when the loop stops, which is why
/// [`PumpLoop::finalize`] consumes the loop rather than borrowing it.
pub trait PumpDriver {
    /// A running take, joinable for its finalize result.
    type Running: RunningPump;

    /// Start running `loop_`, stopping when `running` is cleared.
    ///
    /// `running` is the [`Recorder`]'s stop flag. An implementation must check
    /// it between passes — a [`Starved`](PumpPass::Starved) source never ends on
    /// its own, so this flag is the only thing that ends a live take.
    fn drive<I: AudioIn + Send + 'static>(
        self,
        loop_: PumpLoop<I>,
        running: Arc<AtomicBool>,
    ) -> Self::Running;
}

/// The handle a [`PumpDriver`] hands back: something that can be waited on for
/// the finalize result.
pub trait RunningPump {
    /// Wait for the pump to stop and return what
    /// [`finalize`](PumpLoop::finalize) reported.
    ///
    /// Called exactly once, by [`Recorder`]'s shutdown, which takes the handle
    /// out of an `Option` to guarantee it.
    ///
    /// `Box<Self>` rather than `self`: [`Recorder`] stores the handle as a
    /// `dyn RunningPump` so its own type does not carry the driver, and a
    /// by-value `self` on a trait object is not something the compiler can size.
    /// Consuming the box is the same once-only guarantee with a shape that
    /// survives erasure.
    fn join(self: Box<Self>) -> std::io::Result<()>;
}

/// The production driver: one dedicated thread per take.
///
/// Parks `IDLE_PARK` on a starving source rather than spinning a core, and
/// breaks on [`Ended`](PumpPass::Ended) or on the cleared stop flag. This is
/// what [`Recorder::start`] uses.
#[derive(Debug, Default, Clone, Copy)]
pub struct ThreadDriver;

impl PumpDriver for ThreadDriver {
    type Running = std::thread::JoinHandle<std::io::Result<()>>;

    fn drive<I: AudioIn + Send + 'static>(
        self,
        mut loop_: PumpLoop<I>,
        running: Arc<AtomicBool>,
    ) -> Self::Running {
        std::thread::spawn(move || {
            while running.load(Ordering::Acquire) {
                match loop_.pump_once() {
                    PumpPass::Wrote(_) => {}
                    // Live source: the producer has not caught up.
                    PumpPass::Starved => std::thread::sleep(IDLE_PARK),
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
}

/// A driver that runs no passes of its own: the caller runs them.
///
/// Hand one to [`Recorder::start_with`] and keep the [`ManualPump`] it was built
/// from. The recorder then behaves exactly as it does over a thread — same stop
/// flag, same once-only shutdown, same `Drop` finalization — while every pass is
/// a call the caller made and can count.
///
/// This is what lets a recorder test state its claim exactly. "A starving source
/// keeps being polled across its empty passes" was previously asserted as
/// `frames >= 3` after a 40 ms sleep, with a comment recording that the floor
/// had been tuned on a loaded machine; driven, the same claim is
/// `assert_eq!(frames, 2)` after four passes of a fixture that yields on every
/// other poll, and it fails for exactly the reason it names.
///
/// # It is not a second implementation of the loop
///
/// Both drivers run [`PumpLoop::pump_once`] and finalize through
/// [`PumpLoop::finalize`]; only the pacing differs. What a test drives and what a
/// host runs cannot diverge, which is the property that makes the seam worth the
/// trait.
pub struct ManualDriver {
    /// Shared with the [`ManualPump`] the caller kept. `None` once the take has
    /// been finalized, which is what makes finalize once-only across the two
    /// handles.
    slot: Arc<std::sync::Mutex<Option<Box<dyn ErasedPump>>>>,
}

/// The caller's half of a [`ManualDriver`]: run passes, and see what they did.
///
/// Held across the [`Recorder`]'s life. After the recorder is stopped or dropped
/// the loop is finalized and gone, so [`pump_once`](Self::pump_once) returns
/// `None` — which is itself the assertion that a shutdown ran.
pub struct ManualPump {
    slot: Arc<std::sync::Mutex<Option<Box<dyn ErasedPump>>>>,
}

/// [`PumpLoop`] with its source type erased.
///
/// The driver is handed a `PumpLoop<I>` for a caller-chosen `I`, but
/// [`ManualPump`] is built *before* the recorder exists and so cannot name it.
/// Two methods, both already on `PumpLoop`; `finalize` takes `Box<Self>` because
/// it consumes the loop.
trait ErasedPump: Send {
    fn pump_once(&mut self) -> PumpPass;
    fn finalize(self: Box<Self>) -> std::io::Result<()>;
}

impl<I: AudioIn + Send> ErasedPump for PumpLoop<I> {
    fn pump_once(&mut self) -> PumpPass {
        PumpLoop::pump_once(self)
    }
    fn finalize(self: Box<Self>) -> std::io::Result<()> {
        PumpLoop::finalize(*self)
    }
}

impl ManualDriver {
    /// A driver and the handle that runs it, sharing one slot.
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
    /// Run one pump pass, or `None` if the take has been finalized.
    ///
    /// `None` is not an error condition to skip past: it is the observable that
    /// a shutdown ran, and a test asserting a `Drop` finalized can check it
    /// directly instead of inferring it from a readable file.
    pub fn pump_once(&self) -> Option<PumpPass> {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_mut().map(|l| l.pump_once())
    }

    /// Run passes until one does not write, or `budget` passes have run.
    /// Returns the total frames written.
    ///
    /// The budget is a liveness bound. A source that writes forever is not what
    /// any fixture here models, and exhausting it means the test is measuring
    /// something other than what it meant to.
    #[must_use]
    pub fn pump_until_dry(&self, budget: usize) -> usize {
        let mut total = 0;
        for _ in 0..budget {
            match self.pump_once() {
                Some(PumpPass::Wrote(n)) => total += n,
                _ => break,
            }
        }
        total
    }
}

/// A [`ManualDriver`]'s running take: the slot, waiting to be finalized.
///
/// `join` is where the once-only finalize happens on this path — it takes the
/// loop out of the shared slot, which is the same act that makes every later
/// [`ManualPump::pump_once`] return `None`.
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
}

impl PumpDriver for ManualDriver {
    type Running = ManualRunning;

    fn drive<I: AudioIn + Send + 'static>(
        self,
        loop_: PumpLoop<I>,
        _running: Arc<AtomicBool>,
    ) -> Self::Running {
        // The stop flag is unused here on purpose. Nothing runs between the
        // caller's own `pump_once` calls, so there is no loop to break; the
        // recorder's shutdown ends the take by taking the loop out of the slot,
        // and that is what `join` does below.
        *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(loop_));
        ManualRunning { slot: self.slot }
    }
}

/// A live source→WAV recorder.
///
/// [`start`](Self::start) spawns a background thread pumping `src` into a
/// [`WavOut`]. [`stop`](Self::stop) signals that thread to finish, joins it, and
/// returns the finalize result — the once-only close is guaranteed because the
/// sink is owned by the pump thread and `stop` takes `self` by value.
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
    /// Cleared by [`stop`](Self::stop) to break the pump loop.
    running: Arc<AtomicBool>,
    /// The running pump, joinable for the `finalize` result. `Option` because
    /// taking it is what makes the shutdown once-only: both
    /// [`stop`](Self::stop) and [`drop`](Self::drop) go through
    /// [`shutdown`](Self::shutdown), and whichever runs first leaves `None`
    /// behind for the other.
    ///
    /// Boxed rather than generic on the driver: `Recorder` is one type a caller
    /// stores in a field, and making it `Recorder<D>` would push the choice of
    /// driver into every signature that holds a take.
    handle: Option<Box<dyn RunningPump>>,
    /// Where a shutdown that cannot return its result leaves it.
    ///
    /// Only [`drop`](Self::drop) writes here — [`stop`](Self::stop) returns the
    /// same value to its caller instead. It is a *shared* cell rather than a
    /// plain field so a caller can hold [`finalize_status`](Self::finalize_status)
    /// across the drop and read the outcome afterwards, which is the only
    /// moment the answer exists on that path.
    ///
    /// `None` means no drop-path finalize has completed: either `stop` was
    /// called, or the recorder is still alive.
    status: Arc<FinalizeStatus>,
}

/// The finalize outcome of a [`Recorder`] shut down by its [`Drop`] impl.
///
/// Obtained from [`Recorder::finalize_status`] *before* the recorder is
/// dropped; reading it afterwards is the point.
///
/// # Why this exists at all
///
/// [`Recorder::stop`] returns the finalize `Result`, and that is the path a
/// caller should take. `Drop` has no return value and this workspace does not
/// panic in `Drop`, so a dropped recorder's finalize error would otherwise
/// vanish — and a failed finalize means an unpatched WAV header, i.e. an
/// unreadable file. This is where that error goes instead of nowhere.
///
/// The crate has no logger and takes no logging dependency, so the outcome is
/// *stored* rather than printed: a caller that cares reads it, one that does
/// not pays an `AtomicBool` and a `Mutex` it never locks.
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
    /// Whether a drop-path finalize has completed.
    ///
    /// `false` after [`Recorder::stop`]: that path returns the result directly
    /// and deliberately leaves nothing here.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// The drop-path finalize error, if there was one.
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
    /// Spawn a background thread pumping `src` into `sink`. Returns once
    /// recording is live.
    ///
    /// **Errors if `src` and `sink` disagree on channel width**
    /// ([`Error::ChannelWidthMismatch`], carrying both layouts) — the module
    /// header explains why this check is here and what it replaces.
    ///
    /// The **sample rate** is still the caller's to match: `AudioIn` carries no
    /// rate at all (the trait deliberately has none — a caller that needs one
    /// holds the concrete type), so nothing here can compare them. A caller
    /// opening a mic reads `MicIn::sample_rate()` and builds the `WavOut` from
    /// it, or uses `MicIn::matching_sink`, which pairs both halves at the one
    /// place they are both in scope.
    pub fn start<I>(src: I, sink: WavOut) -> Result<Self>
    where
        I: AudioIn + Send + 'static,
    {
        Self::start_with(src, sink, ThreadDriver)
    }

    /// [`start`](Self::start), with the caller choosing how the pump runs.
    ///
    /// The width check, the stop flag, the once-only shutdown and the `Drop`
    /// finalization are all identical — a driver decides only *where the loop
    /// runs and how it waits*, never when the take ends. So a recorder over a
    /// [`ManualDriver`] is the same recorder, and a test over one is testing the
    /// shipped shutdown path rather than a stand-in for it.
    ///
    /// # Errors
    ///
    /// [`Error::ChannelWidthMismatch`] when `src` and `sink` disagree on width,
    /// checked before the driver is handed anything — so no frame is ever
    /// written to a file whose channels would rotate.
    pub fn start_with<I, D>(src: I, sink: WavOut, driver: D) -> Result<Self>
    where
        I: AudioIn + Send + 'static,
        D: PumpDriver,
        D::Running: 'static,
    {
        let layout = src.layout();
        if layout != AudioOut::layout(&sink) {
            return Err(Error::ChannelWidthMismatch {
                src: layout,
                sink: AudioOut::layout(&sink),
            });
        }
        // Sized once, here, from the width both endpoints agreed on above.
        let scratch_samples = SCRATCH_FRAMES * layout.count().max(1) as usize;

        let running = Arc::new(AtomicBool::new(true));
        let pump_loop = PumpLoop {
            src,
            sink,
            // Allocated once; `pump_once` never allocates.
            scratch: vec![0.0f32; scratch_samples],
        };

        let handle = driver.drive(pump_loop, Arc::clone(&running));

        Ok(Self {
            running,
            handle: Some(Box::new(handle)),
            status: Arc::new(FinalizeStatus::default()),
        })
    }

    /// Signal the pump thread to stop, join it, and finalize the WAV. Returns
    /// [`finalize`](tutti_core::io::AudioOut::finalize)'s result — an error here
    /// means the WAV header was left unpatched and the file is unreadable.
    ///
    /// This is the path that hands the result back. [`Drop`] runs the same
    /// shutdown and has nowhere to return it, so it stores it in
    /// [`finalize_status`](Self::finalize_status) instead.
    pub fn stop(mut self) -> Result<()> {
        // `self` is consumed, so the `Drop` that follows finds `handle` already
        // taken and does nothing — the join happens exactly once.
        Ok(self.shutdown()?)
    }

    /// Wait for a **finite** take to reach its own end, then finalize.
    ///
    /// [`stop`](Self::stop) clears the run flag *before* joining. That is
    /// exactly right for a live source and exactly wrong for a finite one: a
    /// source that has not reached its end yet is cut off wherever the pump
    /// happened to be, and the take is silently truncated — a short file, no
    /// error anywhere. Until this existed there was no way to record a finite
    /// source to completion without racing the pump thread and guessing.
    ///
    /// This joins without touching the flag, so the loop breaks where it was
    /// always going to: on the source's own
    /// [`OnEmpty::EndOfStream`](tutti_core::io::OnEmpty::EndOfStream). The
    /// finalize result rides the join home exactly as it does for `stop`.
    ///
    /// **On a [`Starved`](tutti_core::io::OnEmpty::Starved) source this blocks
    /// forever**, and every microphone capture is one — the pump parks and
    /// re-polls rather than ending, so nothing but `stop` will ever break the
    /// loop. That is not a wart to guard against with a timeout: the two
    /// verdicts mean different things, and a recorder that gave up after some
    /// interval would be reporting a complete take when it had no idea.
    /// Choose the method that matches your source's `ON_EMPTY`.
    ///
    /// # Errors
    ///
    /// The sink's finalize error, if back-patching the WAV header failed.
    pub fn wait(mut self) -> Result<()> {
        // Deliberately not `shutdown()`: the one line that differs is the one
        // that would truncate the take.
        Ok(self.join_pump()?)
    }

    /// Where a [`Drop`]-path finalize leaves its outcome.
    ///
    /// Take this handle *before* dropping the recorder; it outlives the
    /// recorder, which is what makes the answer readable at all on that path.
    /// A caller using [`stop`](Self::stop) has no use for it — that returns the
    /// same result directly, and leaves this untouched.
    pub fn finalize_status(&self) -> Arc<FinalizeStatus> {
        Arc::clone(&self.status)
    }

    /// Break the pump loop, join the thread, and surface the finalize result.
    ///
    /// The shared body of [`stop`](Self::stop) and [`Drop`]. Taking `handle`
    /// is what makes it once-only: a second call finds `None` and returns
    /// `Ok(())` without joining anything.
    ///
    /// Clearing `running` is the half that matters for a **live** source. The
    /// pump loop exits on its own only for a finite one
    /// ([`OnEmpty::EndOfStream`] breaks); a starving source — every microphone
    /// capture — parks and re-polls forever, so nothing else ever reaches the
    /// `finalize` below it.
    fn shutdown(&mut self) -> std::io::Result<()> {
        self.running.store(false, Ordering::Release);
        self.join_pump()
    }

    /// Join the pump and surface its finalize result, leaving `running`
    /// alone.
    ///
    /// The half [`shutdown`](Self::shutdown) and [`wait`](Self::wait) share,
    /// and the whole difference between them is the line above this call.
    /// Taking `handle` is what makes either once-only.
    fn join_pump(&mut self) -> std::io::Result<()> {
        match self.handle.take() {
            // The driver finalizes the sink and returns that io::Result.
            Some(handle) => handle.join(),
            None => Ok(()),
        }
    }
}

/// Stop, join and finalize a recorder nobody called [`stop`](Recorder::stop)
/// on.
///
/// Without this, dropping a live recorder leaks the pump thread *and* the
/// take: the loop only breaks on `running`, so a
/// [`Starved`](OnEmpty::Starved) source parks and re-polls forever,
/// `finalize` never runs, and the WAV header is never back-patched — the
/// module header explains why that leaves an unreadable file. The data is on
/// disk; only the header says how much of it there is.
///
/// The finalize `Result` has nowhere to go from here, and this workspace does
/// not panic in `Drop`. It is stored in [`FinalizeStatus`] instead, which a
/// caller reads through a handle taken before the drop.
impl Drop for Recorder {
    fn drop(&mut self) {
        // `stop` consumed `self` and already took `handle`, so this is a no-op
        // on that path and must not overwrite the status it deliberately left
        // clear.
        if self.handle.is_some() {
            let result = self.shutdown();
            self.status.set(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::io::{AudioIn, OnEmpty};
    use tutti_core::pcm::BitDepth;
    use tutti_core::ChannelLayout;

    /// A finite in-memory [`AudioIn`] standing in for a live mic: hands out its
    /// frames in bounded chunks, returning a short-then-zero count at
    /// end-of-stream. Lets the pump→`WavOut`→readback path be proven without
    /// touching real hardware. Mirrors the `SliceSource` fixture in
    /// `tutti_types::io`'s own tests, including its runtime width.
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

        fn poll_into(&mut self, out: &mut [f32]) -> usize {
            let ch = self.layout.count() as usize;
            let total = self.samples.len() / ch;
            let n = (total - self.pos).min(out.len() / ch);
            out[..n * ch].copy_from_slice(&self.samples[self.pos * ch..(self.pos + n) * ch]);
            self.pos += n;
            n
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
        let mut scratch = vec![0.0f32; SCRATCH_FRAMES * 2];
        while pump(&mut src, &mut wav, &mut scratch) != 0 {}
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

        // Drain by counting, not by sleeping. The old version slept 50 ms
        // because `stop` clears the run flag immediately and a threaded pump can
        // exit before its first pass — a race whose outcome was the machine's to
        // decide. Here the passes are the caller's, so "drained" is a fact.
        let written = pump.pump_until_dry(16);
        assert_eq!(
            written, FRAMES,
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

            fn poll_into(&mut self, out: &mut [f32]) -> usize {
                let n = self.polls.fetch_add(1, Ordering::Relaxed);
                // Every other poll is empty — never exhausted.
                if n % 2 == 1 || out.len() < 2 {
                    return 0;
                }
                out[0] = 0.25;
                out[1] = -0.25;
                1
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
        // This is what the seam bought. The old version slept 40 ms and asserted
        // `frames >= 3`, with a comment recording that the floor had been tuned
        // against a loaded machine — an assertion about the test host. The
        // sequence below fails if a `Starved` pass is treated as end-of-stream
        // (writes stop at 1), and it fails if the parity of the fixture changes,
        // which is the only other way the count can move.
        let mut wrote = 0;
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
        assert_eq!(wrote, 3, "three yielding passes, one frame each");

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

        fn poll_into(&mut self, out: &mut [f32]) -> usize {
            let frames = out.len() / 2;
            for (i, s) in out[..frames * 2].iter_mut().enumerate() {
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
            // The old version slept 30 ms and hoped; three passes is a count.
            for _ in 0..3 {
                assert!(
                    matches!(pump.pump_once(), Some(PumpPass::Wrote(n)) if n > 0),
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
            3 * SCRATCH_FRAMES * 2,
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
                matches!(pump.pump_once(), Some(PumpPass::Wrote(n)) if n > 0),
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
}
