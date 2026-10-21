//! Moving frames between the engine's I/O edges, with the ECS owning the
//! lifetime.
//!
//! [`tutti_core::io::pump`] is the whole of "recording" minus the loop and the
//! stop condition, which its own docs call the caller's policy. [`AudioPump`] is
//! that policy for a Bevy host: a [`Recorder`] running the loop on its own
//! thread, a component holding it, and a drain system that collects it.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::graph::{AudioPump, AudioPumpAppExt, PumpFinished};
//! use tutti_core::io::{AudioIn, AudioOut, OnEmpty};
//! use tutti_core::{ChannelLayout, Samples};
//! use std::sync::{Arc, Mutex};
//!
//! /// A finite stereo source. `MicIn` (`audio-io`) is the live counterpart; the
//! /// only difference that reaches this layer is `ON_EMPTY`.
//! struct Tone { frames_left: usize }
//! impl AudioIn<f32> for Tone {
//!     // Finite: a 0-frame poll means "never again", so the pump exits rather
//!     // than parking. A live source says `Starved` and the pump waits.
//!     const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;
//!     fn layout(&self) -> ChannelLayout { ChannelLayout::STEREO }
//!     fn poll_into(&mut self, out: &mut [f32]) -> Samples {
//!         // `out` is flat interleaved, so it holds len/2 FRAMES — the return
//!         // is a frame count, never a sample count, and the type says so.
//!         let fits = Samples::from_interleaved_len(out.len(), self.layout());
//!         let frames = fits.min(Samples(self.frames_left));
//!         out[..frames.interleaved_len(self.layout())].fill(0.25);
//!         self.frames_left -= frames.get();
//!         frames
//!     }
//! }
//!
//! /// Counts what arrived, so the assertion can be about frames moved.
//! #[derive(Clone, Default)]
//! struct Counter(Arc<Mutex<usize>>);
//! impl AudioOut<f32> for Counter {
//!     fn layout(&self) -> ChannelLayout { ChannelLayout::STEREO }
//!     fn write(&mut self, frames: &[f32]) { *self.0.lock().unwrap() += frames.len() / 2; }
//!     fn finalize(self) -> std::io::Result<()> { Ok(()) }
//! }
//!
//! let sink = Counter::default();
//! let seen = sink.0.clone();
//!
//! let mut app = App::new();
//! // One registration per element type — it covers every channel width, since
//! // the width rides on the value rather than in the type.
//! app.add_audio_pump::<f32>();
//! // Any AudioIn into any AudioOut of the same frame type.
//! app.world_mut().spawn(AudioPump::start(Tone { frames_left: 4096 }, sink, Samples(1024)));
//!
//! // The source ends on its own; the drain system joins the thread and reports.
//! let mut finalized = None;
//! while finalized.is_none() {
//!     app.update();
//!     finalized = app
//!         .world()
//!         .resource::<bevy_ecs::message::Messages<PumpFinished>>()
//!         .iter_current_update_messages()
//!         .next()
//!         .map(|done| done.result.is_ok());
//! }
//!
//! assert_eq!(finalized, Some(true), "the sink must close cleanly");
//! assert_eq!(*seen.lock().unwrap(), 4096);
//! ```
//!
//! # What this layer adds, and what it does not
//!
//! It adds **lifetime**, not behaviour. [`AudioOut::finalize`] consumes `self`
//! and can fail — for a WAV that means an unpatched header and an unreadable
//! file — so it must happen exactly once, on every path out. Three paths reach
//! it and all converge on one join:
//!
//! - [`AudioPump::stop`] clears the flag; [`drain_audio_pumps`] joins.
//! - The source ends on its own (see [`OnEmpty`](tutti_core::io::OnEmpty));
//!   the same drain joins.
//! - The entity despawns mid-pump; [`finalize_removed_pumps`] joins.
//!
//! A component dropped with no `Remove` hook running (the world itself torn
//! down) is still finalized: the [`Recorder`] inside does it in its `Drop`.
//!
//! The loop itself is the engine's, and so is the decision of what an empty
//! poll means — that is [`AudioIn::ON_EMPTY`], read off the source type rather
//! than passed in here.
//!
//! # Not on a task pool
//!
//! A pump does not run to completion, so it cannot share Bevy's task pools:
//! `AsyncComputeTaskPool` defaults to at most 4 threads, and 4 live pumps would
//! occupy all of them permanently — soundfont decodes and plugin scans would
//! stop running with no error anywhere. `IoTaskPool` has the same cap. A
//! starving source also needs a real sleep between polls, which is illegal on a
//! shared pool.
//!
//! # The loop is `tutti_io::Recorder`'s
//!
//! This module used to run its own copy of the poll / park / break / finalize
//! loop. It was the same loop as [`Recorder`]'s, so design 015 kept that
//! one: it already had the width check, the `Drop` finalization and the
//! driver seam its tests count passes through, and it gained the three
//! non-blocking methods a frame loop needs ([`Recorder::request_stop`],
//! [`Recorder::is_finished`], [`Recorder::shutdown`]). An [`AudioPump`] is a
//! `Recorder` plus the element type that keys its drain system.

use std::marker::PhantomData;
use std::time::Duration;

use bevy_app::{App, Update};
use bevy_ecs::message::{Message, MessageWriter};
use bevy_ecs::prelude::*;

use tutti_core::io::{AudioIn, AudioOut};
use tutti_core::Samples;
use tutti_io::{Recorder, ThreadDriver};

/// How long the pump parks when a [`Starved`](tutti_core::io::OnEmpty::Starved)
/// source yields nothing: `tutti_io`'s default, long enough not to spin a
/// core, far shorter than the ring it drains can overrun.
pub use tutti_io::IDLE_PARK;

/// A running `AudioIn → AudioOut` pump: one background thread moving frames
/// from a source into a sink until it stops or is stopped.
///
/// `S` is the sample element both endpoints speak. The **channel width is not
/// a type parameter**: the I/O traits carry it as a runtime
/// [`ChannelLayout`](tutti_core::ChannelLayout), read off the source at
/// [`start`](Self::start), so one `AudioPump<f32>` covers stereo, 5.1, and a
/// width that only exists at runtime. A const width would put the width in the
/// schedule instead: a host recording 5.1 would register a distinct drain system
/// per width, and could register none at all for a width it learns from the
/// device.
///
/// The **endpoints are deliberately not type parameters** — the loop is
/// monomorphized over the concrete source and sink (never `dyn`, by design), so
/// making them generic here would mint a distinct component type per pairing,
/// each needing its own registered drain system. They are erased into the
/// [`Recorder`]'s thread instead, leaving one component to query per sample
/// element.
///
/// Register the drain for each element type a host uses with
/// [`add_audio_pump`](AudioPumpAppExt::add_audio_pump).
#[derive(Component)]
pub struct AudioPump<S = f32> {
    take: Take,
    /// `fn() -> S` rather than `S`, so the component is `Send + Sync` whatever
    /// `S` is and carries no drop obligation for a type it never holds.
    _frame: PhantomData<fn() -> S>,
}

/// What [`AudioPump::start`] got back from [`Recorder`].
enum Take {
    /// The pump is live (or has ended and awaits collection).
    Running(Recorder),
    /// The source and sink disagreed on width, so no thread was started. The
    /// error waits here for the drain to report it as [`PumpFinished`];
    /// `Option` so it is reported once.
    Refused(Option<std::io::Error>),
}

impl<S> AudioPump<S> {
    /// Starts pumping `src` into `dst` on a background thread.
    ///
    /// `capacity` is the scratch buffer in **frames** — a [`Samples`], so a
    /// caller cannot hand over a sample count by mistake and get a buffer
    /// `width` times too large. It is allocated once, sized
    /// [`capacity.interleaved_len(layout)`](Samples::interleaved_len) from the
    /// source's own layout, so the loop body never allocates. Zero is taken as
    /// one frame.
    ///
    /// There is no policy argument: whether an empty poll means "retry" or
    /// "done" is [`AudioIn::ON_EMPTY`], a property of the source type. A caller
    /// cannot pair it wrongly because a caller does not state it.
    ///
    /// **Width agreement is checked here**, by [`Recorder`]'s start. A
    /// component has no error to return, so a mismatch starts no thread and
    /// the drain reports it as a [`PumpFinished`] whose `result` is an
    /// [`InvalidInput`](std::io::ErrorKind::InvalidInput) error carrying the
    /// [`tutti_io::Error::ChannelWidthMismatch`]. The sink is dropped
    /// unfinalized, as `Recorder::start` drops it: not one frame reached it.
    pub fn start<I, O>(src: I, dst: O, capacity: Samples) -> Self
    where
        I: AudioIn<S> + Send + 'static,
        O: AudioOut<S> + Send + 'static,
        S: Default + Copy + Send + 'static,
    {
        Self::start_with_park(src, dst, capacity, IDLE_PARK)
    }

    /// [`start`](Self::start) with the back-off named.
    ///
    /// Only reached for a [`Starved`](tutti_core::io::OnEmpty::Starved) source —
    /// an [`EndOfStream`](tutti_core::io::OnEmpty::EndOfStream) one never parks,
    /// it exits. How long to wait is the consumer's tuning; *whether* to wait is
    /// the source's nature, which is why only this half is an argument.
    pub fn start_with_park<I, O>(src: I, dst: O, capacity: Samples, park: Duration) -> Self
    where
        I: AudioIn<S> + Send + 'static,
        O: AudioOut<S> + Send + 'static,
        S: Default + Copy + Send + 'static,
    {
        let take = match Recorder::start_with_capacity(
            src,
            dst,
            capacity,
            ThreadDriver::with_park(park),
        ) {
            Ok(recorder) => Take::Running(recorder),
            Err(refused) => Take::Refused(Some(refused.into())),
        };
        Self {
            take,
            _frame: PhantomData,
        }
    }

    /// Asks the pump to finish. Idempotent.
    ///
    /// Returns immediately — the thread notices the flag on its next pass, then
    /// finalizes. [`drain_audio_pumps`] reports the outcome once it has.
    pub fn stop(&self) {
        if let Take::Running(recorder) = &self.take {
            recorder.request_stop();
        }
    }

    /// Whether the pump thread is still supposed to be running.
    ///
    /// This reports the *flag*, not the thread: an
    /// [`EndOfStream`](tutti_core::io::OnEmpty::EndOfStream) source that ran out
    /// has exited while this still reads `true`, until the drain observes it. A
    /// pump refused at start was never running.
    pub fn is_running(&self) -> bool {
        match &self.take {
            Take::Running(recorder) => recorder.is_running(),
            Take::Refused(_) => false,
        }
    }

    /// Stop, join, and return the sink's finalize result (or the start's
    /// refusal).
    ///
    /// `None` when it was already collected, which is the case that makes
    /// double-finalize unrepresentable rather than a panic.
    fn join(&mut self) -> Option<std::io::Result<()>> {
        match &mut self.take {
            Take::Running(recorder) => recorder.shutdown(),
            Take::Refused(error) => error.take().map(Err),
        }
    }

    /// Whether there is an outcome to collect, without blocking on it.
    fn is_finished(&self) -> bool {
        match &self.take {
            Take::Running(recorder) => recorder.is_finished(),
            Take::Refused(error) => error.is_some(),
        }
    }
}

/// A pump ended: its sink was finalized, or the pump was refused at start.
///
/// Carries the [`AudioOut::finalize`] result, which is the only place a sink's
/// closing error can surface — for a WAV, an `Err` here means the header was
/// never patched and the file will not open. A pump refused at start (source
/// and sink widths disagree) reports an
/// [`InvalidInput`](std::io::ErrorKind::InvalidInput) error here instead.
#[derive(Message, Debug)]
pub struct PumpFinished {
    /// The entity that carried the pump. Already had its [`AudioPump`] removed.
    pub entity: Entity,
    /// What [`AudioOut::finalize`] returned, or the start's refusal. An `Err`
    /// means the sink never closed cleanly — for a WAV, a header that was
    /// never patched.
    pub result: std::io::Result<()>,
}

/// Joins pumps whose thread has finished, and reports what finalizing did.
///
/// Polls rather than blocks: a pump that is still moving frames is left alone,
/// so this costs one atomic load per live pump per frame.
pub fn drain_audio_pumps<S: Send + Sync + 'static>(
    mut commands: Commands,
    mut pumps: Query<(Entity, &mut AudioPump<S>)>,
    mut finished: MessageWriter<PumpFinished>,
) {
    for (entity, mut pump) in pumps.iter_mut() {
        // Two ways to be done: the thread exited on its own (a finite source
        // ran out), or `stop()` was called and it has since noticed.
        if !pump.is_finished() && pump.is_running() {
            continue;
        }
        let Some(result) = pump.join() else { continue };

        if let Err(error) = &result {
            bevy_log::error!("audio pump ended with error: {error}");
        }
        commands.entity(entity).remove::<AudioPump<S>>();
        finished.write(PumpFinished { entity, result });
    }
}

/// Finalizes a pump whose component is being removed, and logs a failure.
///
/// `On<Remove, AudioPump<S>>` fires at command-flush with the value still
/// readable, mirroring
/// [`unwire_removed_sources`](super::wire::unwire_removed_sources).
/// The sink would be finalized without it: the [`Recorder`] inside stops and
/// joins in its `Drop`. But `Drop` can only store the finalize error in a
/// [`FinalizeStatus`](tutti_io::FinalizeStatus) that nothing here reads, so a
/// sink that failed to close on a despawn (an unreadable WAV) would go
/// unreported. This collects the result while the component is still
/// reachable, and logs it; the entity has already left the drain's query, so
/// no [`PumpFinished`] is sent.
///
/// This blocks the frame until the thread notices its flag (one poll, so
/// bounded by the park). That is the cost of the guarantee, and it is only paid
/// on teardown.
pub fn finalize_removed_pumps<S: Send + Sync + 'static>(
    remove: On<Remove, AudioPump<S>>,
    mut pumps: Query<&mut AudioPump<S>>,
) {
    let entity = remove.event_target();
    let Ok(mut pump) = pumps.get_mut(entity) else {
        return;
    };
    if let Some(Err(error)) = pump.join() {
        bevy_log::error!("audio pump ended with error on removal: {error}");
    }
}

/// Which `AudioPump<S>` drains are already scheduled.
///
/// `add_systems` does not deduplicate, so without this an element type
/// registered by both a host and a library plugin would drain twice per frame.
/// The second pass finds the take already collected and does nothing, but it doubles
/// the per-frame query cost and makes the schedule depend on how many callers
/// asked.
#[derive(Resource, Default)]
struct RegisteredAudioPumps(std::collections::HashSet<core::any::TypeId>);

/// Registers the drain and removal observer for one [`AudioPump`] element type.
pub trait AudioPumpAppExt {
    /// Schedules the systems that drive `AudioPump<S>`: joining finished
    /// pumps, emitting [`PumpFinished`], and finalizing on removal.
    ///
    /// Idempotent, so a host and a library plugin can both declare the element
    /// type they share.
    ///
    /// **One registration covers every channel width.** The width rides on the
    /// value as a `ChannelLayout` rather than in the type, so the schedule does
    /// not depend on it and a host need not name a width it only learns from a
    /// device at runtime.
    ///
    /// ```rust
    /// use bevy_app::prelude::*;
    /// use bevy_ecs::message::Messages;
    /// use bevy_tutti::graph::{AudioPumpAppExt, PumpFinished};
    ///
    /// let mut app = App::new();
    /// app.add_audio_pump::<f32>(); // covers stereo, 5.1, whatever the mic is
    /// // Idempotent, so a host and a library plugin may both declare it.
    /// app.add_audio_pump::<f32>();
    ///
    /// // The message the drain reports through is registered either way.
    /// assert!(app.world().get_resource::<Messages<PumpFinished>>().is_some());
    /// ```
    fn add_audio_pump<S: Send + Sync + 'static>(&mut self) -> &mut Self;
}

impl AudioPumpAppExt for App {
    fn add_audio_pump<S: Send + Sync + 'static>(&mut self) -> &mut Self {
        let key = core::any::TypeId::of::<S>();
        if !self
            .world_mut()
            .get_resource_or_init::<RegisteredAudioPumps>()
            .0
            .insert(key)
        {
            return self;
        }
        self.add_message::<PumpFinished>();
        self.add_observer(finalize_removed_pumps::<S>);
        // Deliberately not in a `GraphReconcileSystems` phase and not gated on
        // `engine_ready`: a pump touches no graph topology and edits no
        // session, and a file→WAV pump is valid with no audio device at
        // all. Gating it would tie offline work to a live callback.
        self.add_systems(Update, drain_audio_pumps::<S>)
    }
}
