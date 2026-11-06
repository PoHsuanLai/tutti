//! [`Recorder`]: a background pump from any [`AudioIn`] into any [`AudioOut`].
//!
//! The loop and its drivers are in [`PumpLoop`] and [`PumpDriver`]; this type
//! owns the stop flag, the once-only shutdown and the `Drop` finalize.
//!
//! - [`start`](Recorder::start) refuses a source and sink whose channel widths
//!   disagree, before any frame moves. The same check is a `debug_assert` in
//!   [`tutti_core::io::pump`].
//! - The scratch buffer is allocated once, `capacity.interleaved_len(layout)`
//!   samples, so the loop never allocates.
//! - A zero-frame poll means "back off" or "finished" according to the
//!   source's [`ON_EMPTY`](AudioIn::ON_EMPTY). `Starved` sources (every
//!   microphone) never end on their own; [`finish`](Recorder::finish) stops
//!   them, [`wait`](Recorder::wait) would block forever.
//! - Dropping a live recorder stops, joins and finalizes it, so the WAV header
//!   is always patched. `Drop` cannot return the result; it lands in the
//!   [`FinalizeStatus`] taken beforehand.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use tutti_core::Samples;
use tutti_core::io::{AudioIn, AudioOut};

use crate::error::{Error, Result};
use crate::pump::{PumpDriver, PumpLoop, RunningPump, ThreadDriver};

/// Frames moved per pump pass unless the caller names a capacity.
const SCRATCH_FRAMES: Samples = Samples(1024);

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
/// - [`finish`](Self::finish) — for a live source (a microphone, [`TapIn`](crate::TapIn)):
///   stops the thread wherever it is, finalizes the sink and returns the result.
/// - [`wait`](Self::wait) — for a finite source (a decoded file): lets the
///   source reach its end, then finalizes.
/// - [`finish_in_place`](Self::finish_in_place) — `finish` without consuming the recorder, for
///   a host that stores the take in place (a Bevy component). Pair it with
///   [`request_stop`](Self::request_stop) and
///   [`is_finished`](Self::is_finished) to end a take without blocking.
/// - dropping the recorder — does what `finish` does and stores the finalize
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
/// rec.finish()
/// # }
/// ```
pub struct Recorder {
    /// Cleared by [`request_stop`](Self::request_stop) (and so by every
    /// shutdown) to break the pump loop.
    running: Arc<AtomicBool>,
    /// The running pump, joinable for the `finalize` result. `Option` because
    /// taking it is what makes the shutdown once-only: `finish`, `wait`,
    /// `finish_in_place` and `drop` all join through it, and whichever runs first
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

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("running", &self.running)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// The finalize outcome of a [`Recorder`] that was dropped rather than stopped.
///
/// Take it from [`Recorder::finalize_status`] before the drop and read it
/// afterwards. [`Recorder::finish`] returns the result directly and leaves
/// this untouched.
#[derive(Default, Debug)]
pub struct FinalizeStatus {
    /// Unset until the drop-path finalize has run; then its error text, if any.
    outcome: OnceLock<Option<String>>,
}

impl FinalizeStatus {
    /// Returns whether a drop-path finalize has completed.
    pub fn is_done(&self) -> bool {
        self.outcome.get().is_some()
    }

    /// Returns the drop-path finalize error's message. `None` if it succeeded
    /// or has not run; [`is_done`](Self::is_done) tells which.
    pub fn error(&self) -> Option<String> {
        self.outcome.get().cloned().flatten()
    }

    fn set(&self, result: std::io::Result<()>) {
        let _ = self.outcome.set(result.err().map(|e| e.to_string()));
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
        let pump_loop = PumpLoop::new(src, sink, vec![S::default(); scratch_samples]);

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
    /// as `finish` but can only store the result in
    /// [`finalize_status`](Self::finalize_status). After
    /// [`finish_in_place`](Self::finish_in_place) has already returned the result, this
    /// returns `Ok(())`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if finalizing failed (for a WAV, the header was left
    /// unpatched and the file is unreadable) or the pump thread panicked.
    pub fn finish(mut self) -> Result<()> {
        // `self` is consumed, so the `Drop` that follows finds `handle` already
        // taken and does nothing — the join happens exactly once.
        Ok(self.finish_in_place().unwrap_or(Ok(()))?)
    }

    /// Waits for a **finite** take to reach its own end, then finalizes.
    ///
    /// [`finish`](Self::finish) cuts a source off wherever the pump happens to be,
    /// which silently truncates a finite one. This instead lets the loop run
    /// until the source reports
    /// [`OnEmpty::EndOfStream`](tutti_core::io::OnEmpty::EndOfStream), then
    /// returns the finalize result as `finish` does.
    ///
    /// **On a [`Starved`](tutti_core::io::OnEmpty::Starved) source this blocks
    /// forever**, and every microphone capture is one — the pump parks and
    /// re-polls rather than ending. Choose the method that matches your
    /// source's `ON_EMPTY`. After [`finish_in_place`](Self::finish_in_place) has already
    /// returned the result, this returns `Ok(())` without blocking.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if finalizing failed or the pump thread panicked.
    pub fn wait(mut self) -> Result<()> {
        // Deliberately not `finish_in_place()`: the one line that differs is the one
        // that would truncate the take.
        Ok(self.join_pump().unwrap_or(Ok(()))?)
    }

    /// Signals the pump to stop, without waiting for it. Idempotent.
    ///
    /// The driver notices on its next pass, finalizes, and stops;
    /// [`is_finished`](Self::is_finished) then reads `true`, and
    /// [`finish_in_place`](Self::finish_in_place) collects the result without blocking.
    pub fn request_stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Whether the pump is still meant to run: `false` once a stop was
    /// requested.
    ///
    /// This reports the *flag*, not the driver: a finite source that ran out
    /// has stopped while this still reads `true`. Ask
    /// [`is_finished`](Self::is_finished) for that.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Whether the pump has stopped running passes, so
    /// [`finish_in_place`](Self::finish_in_place) will not block.
    ///
    /// Never blocks. `true` once a finite source has ended its own take, or
    /// once a requested stop has been noticed. `false` after a shutdown, since
    /// there is no pump left to report on; a [`ManualDriver`] take is never
    /// finished on its own (its caller runs the passes).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| h.is_finished())
    }

    /// Stops the pump, joins it and returns the sink's finalize result, leaving
    /// the recorder in place.
    ///
    /// [`finish`](Self::finish) for a host that stores the take where it cannot be
    /// moved out (a component, a field). `None` once the result was already collected,
    /// which is what makes a second finalize unrepresentable rather than a
    /// panic. Blocks for at most one pass (or one park); after
    /// [`is_finished`](Self::is_finished) it does not block at all.
    ///
    /// Clearing the stop flag is the half that matters for a **live** source.
    /// The pump loop exits on its own only for a finite one
    /// ([`OnEmpty::EndOfStream`](tutti_core::io::OnEmpty::EndOfStream) breaks); a starving source — every microphone
    /// capture — parks and re-polls forever, so nothing else ever reaches the
    /// `finalize` below it.
    pub fn finish_in_place(&mut self) -> Option<std::io::Result<()>> {
        self.request_stop();
        self.join_pump()
    }

    /// Returns the handle where a drop-path finalize leaves its outcome.
    ///
    /// Take it *before* dropping the recorder; it outlives the recorder. After
    /// [`finish`](Self::finish), [`wait`](Self::wait) or
    /// [`finish_in_place`](Self::finish_in_place) it stays untouched, since those return the
    /// result directly.
    pub fn finalize_status(&self) -> Arc<FinalizeStatus> {
        Arc::clone(&self.status)
    }

    /// Join the pump and surface its finalize result, leaving `running`
    /// alone.
    ///
    /// The half [`finish_in_place`](Self::finish_in_place) and [`wait`](Self::wait) share,
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
/// [`Starved`](tutti_core::io::OnEmpty::Starved) source parks and re-polls forever,
/// `finalize` never runs, and the WAV header is never back-patched, which
/// leaves the file unreadable.
///
/// The finalize `Result` has nowhere to go from here, and this workspace does
/// not panic in `Drop`. It is stored in [`FinalizeStatus`] instead, which a
/// caller reads through a handle taken before the drop.
impl Drop for Recorder {
    fn drop(&mut self) {
        // `finish`, `wait` and `finish_in_place` already took `handle`, so this is a
        // no-op on those paths and must not overwrite the status they
        // deliberately left clear.
        if let Some(result) = self.finish_in_place() {
            self.status.set(result);
        }
    }
}

#[cfg(test)]
mod tests;
