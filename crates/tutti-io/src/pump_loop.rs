//! The pump loop and the drivers that run it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tutti_types::Samples;
use tutti_types::{AudioIn, AudioOut, OnEmpty, pump};

#[cfg(doc)]
use crate::Recorder;

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
    /// The source broke ([`AudioIn::is_failed`]) after its buffered frames were
    /// drained, or the sink failed ([`AudioOut::is_failed`]). Stop and
    /// finalize what was written.
    ///
    /// Outranks [`Ended`](Self::Ended): a finite source that failed and is
    /// drained is a failure, not a clean end.
    Failed,
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
    /// Set by the first [`PumpPass::Failed`]; the [`Recorder`] holds the other
    /// end.
    failed: Arc<AtomicBool>,
}

impl<I, O, S> PumpLoop<I, O, S> {
    /// A loop over `src` and `sink` moving at most `scratch.len()` samples per
    /// pass, raising `failed` when a pass fails.
    pub(crate) fn new(src: I, sink: O, scratch: Vec<S>, failed: Arc<AtomicBool>) -> Self {
        Self {
            src,
            sink,
            scratch,
            failed,
        }
    }
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
    S: Copy,
    I: AudioIn<S>,
    O: AudioOut<S>,
{
    /// Runs one pass: polls a block of frames from the source and writes them
    /// to the sink.
    ///
    /// A zero-frame poll is reported as [`Starved`](PumpPass::Starved) or
    /// [`Ended`](PumpPass::Ended) according to the source's own
    /// [`ON_EMPTY`](AudioIn::ON_EMPTY), unless the sink has failed or the
    /// source has failed and is drained, which is [`Failed`](PumpPass::Failed).
    pub fn pump_once(&mut self) -> PumpPass {
        let mut n = pump(&mut self.src, &mut self.sink, &mut self.scratch);
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

    /// Consumes the loop and closes the sink (for a WAV, back-patching its
    /// header).
    ///
    /// Taking `self` makes it once-only: a driver that has finalized cannot
    /// pump again.
    ///
    /// # Errors
    ///
    /// The sink's [`finalize`](AudioOut::finalize) error; for a WAV, the file
    /// is then unreadable. If the sink closed cleanly but the source had failed,
    /// [`Error::SourceFailed`](crate::Error::SourceFailed) as an `io::Error`:
    /// the file is valid and holds the frames delivered before the failure.
    pub fn finalize(self) -> std::io::Result<()> {
        let source_failed = self.src.is_failed();
        self.sink.finalize()?;
        if source_failed {
            return Err(crate::Error::SourceFailed.into());
        }
        Ok(())
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
                    // Broken source or sink: nothing more will come.
                    PumpPass::Failed => break,
                }
            }
            // Finalize exactly once, here, where the loop is owned. The result
            // rides the joined thread back to `finish`.
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
/// use tutti_metering::AudioTap;
/// use tutti_types::Samples;
/// use tutti_io::{Pcm, ManualDriver, PumpPass, Recorder, TapIn, WavOut};
///
/// # fn main() -> tutti_io::Result<()> {
/// let tap = AudioTap::new();
/// let src = TapIn::new(tap.open().expect("a fresh tap has no other reader"));
/// let dir = tempfile::tempdir()?;
/// let wav = WavOut::create(dir.path().join("take.wav"), 48_000.0, 2u16, Pcm::Float32)?;
///
/// let (driver, pump) = ManualDriver::new();
/// let rec = Recorder::start_with(src, wav, driver)?;
///
/// tap.push(&[0.5, -0.5, 0.25, -0.25]);
/// assert_eq!(pump.pump_once(), Some(PumpPass::Wrote(Samples(2))));
/// assert_eq!(pump.pump_once(), Some(PumpPass::Starved));
///
/// rec.finish()?;
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

impl std::fmt::Debug for ManualDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManualDriver").finish_non_exhaustive()
    }
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

impl std::fmt::Debug for ManualPump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManualPump").finish_non_exhaustive()
    }
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

impl std::fmt::Debug for ManualRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManualRunning").finish_non_exhaustive()
    }
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
