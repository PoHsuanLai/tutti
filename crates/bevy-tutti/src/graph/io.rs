//! Moving frames between the engine's I/O edges, with the ECS owning the
//! lifetime.
//!
//! [`tutti_core::io::pump`] is the whole of "recording" minus the loop and the
//! stop condition, which its own docs call the caller's policy. [`AudioPump`] is
//! that policy for a Bevy host: a background thread running the loop, a
//! component holding its handle, and a drain system that joins it.
//!
//! ```rust,ignore
//! // Any AudioIn into any AudioOut of the same frame type.
//! commands.spawn(AudioPump::start(mic, wav, 1024));
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
//! - The source ends on its own (see [`OnEmpty`]); the same drain joins.
//! - The entity despawns mid-pump; [`finalize_removed_pumps`] joins.
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
//! shared pool. `tutti_cpal::Recorder` already runs exactly this loop on a
//! dedicated thread; this wraps that shape rather than reimplementing it.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use bevy_app::{App, Update};
use bevy_ecs::message::{Message, MessageWriter};
use bevy_ecs::prelude::*;

use tutti_core::io::{pump, AudioIn, AudioOut, OnEmpty};

/// How long the pump parks when a [`Starved`](OnEmpty::Starved) source yields
/// nothing. Matches `tutti_cpal::Recorder`: long enough not to spin a core,
/// far shorter than the ring it drains can overrun.
pub const IDLE_PARK: Duration = Duration::from_millis(5);

/// A running `AudioIn → AudioOut` pump: one background thread moving frames
/// from a source into a sink until it stops or is stopped.
///
/// `S`/`CH` mirror [`pump`]'s frame `[S; CH]`. The **endpoints are deliberately
/// not type parameters** — `pump` is monomorphized over the concrete source and
/// sink (never `dyn`, by design), so making them generic here would mint a
/// distinct component type per pairing, each needing its own registered drain
/// system. They are erased into the thread instead, leaving one component to
/// query per frame type.
///
/// Register the drain for each frame type a host uses with
/// [`add_audio_pump`](AudioPumpAppExt::add_audio_pump).
#[derive(Component)]
pub struct AudioPump<S = f32, const CH: usize = 2> {
    /// `Option` so a join can happen exactly once: whoever `take`s it owns the
    /// finalize result, and a second attempt finds `None` rather than panicking
    /// on a joined handle.
    handle: Option<JoinHandle<std::io::Result<()>>>,
    running: Arc<AtomicBool>,
    /// `fn() -> [S; CH]` rather than `[S; CH]`, so the component is `Send +
    /// Sync` whatever `S` is and carries no drop obligation for a type it never
    /// holds.
    _frame: PhantomData<fn() -> [S; CH]>,
}

impl<S, const CH: usize> AudioPump<S, CH> {
    /// Start pumping `src` into `dst` on a background thread.
    ///
    /// `capacity` is the scratch buffer in frames, allocated once before the
    /// loop so the loop body never allocates.
    ///
    /// There is no policy argument: whether an empty poll means "retry" or
    /// "done" is [`AudioIn::ON_EMPTY`], a property of the source type. A caller
    /// cannot pair it wrongly because a caller does not state it.
    pub fn start<I, O>(src: I, dst: O, capacity: usize) -> Self
    where
        I: AudioIn<S, CH> + Send + 'static,
        O: AudioOut<S, CH> + Send + 'static,
        S: Default + Copy + Send + 'static,
    {
        Self::start_with_park(src, dst, capacity, IDLE_PARK)
    }

    /// [`start`](Self::start) with the back-off named.
    ///
    /// Only reached for a [`Starved`](OnEmpty::Starved) source — an
    /// [`EndOfStream`](OnEmpty::EndOfStream) one never parks, it exits. How long
    /// to wait is the consumer's tuning; *whether* to wait is the source's
    /// nature, which is why only this half is an argument.
    pub fn start_with_park<I, O>(mut src: I, mut dst: O, capacity: usize, park: Duration) -> Self
    where
        I: AudioIn<S, CH> + Send + 'static,
        O: AudioOut<S, CH> + Send + 'static,
        S: Default + Copy + Send + 'static,
    {
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);

        let handle = std::thread::spawn(move || {
            // Allocated once; the loop body below never allocates.
            let mut scratch = vec![[S::default(); CH]; capacity.max(1)];
            while flag.load(Ordering::Acquire) {
                if pump(&mut src, &mut dst, &mut scratch) == 0 {
                    match I::ON_EMPTY {
                        // The producer has not caught up — back off, don't spin.
                        OnEmpty::Starved => std::thread::sleep(park),
                        // Nothing more is coming; stop and finalize below.
                        OnEmpty::EndOfStream => break,
                    }
                }
            }
            // Exactly once, here, where the sink is owned. The result rides the
            // joined thread back to whoever joins.
            dst.finalize()
        });

        Self {
            handle: Some(handle),
            running,
            _frame: PhantomData,
        }
    }

    /// Ask the pump to finish. Idempotent.
    ///
    /// Returns immediately — the thread notices the flag on its next pass, then
    /// finalizes. [`drain_audio_pumps`] reports the outcome once it has.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Whether the pump thread is still supposed to be running.
    ///
    /// This reports the *flag*, not the thread: an
    /// [`EndOfStream`](OnEmpty::EndOfStream) source that ran out has exited
    /// while this still reads `true`, until the drain observes it.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Stop, join, and return the sink's finalize result.
    ///
    /// `None` when the handle was already taken, which is the case that makes
    /// double-finalize unrepresentable rather than a panic.
    fn join(&mut self) -> Option<std::io::Result<()>> {
        self.running.store(false, Ordering::Release);
        let handle = self.handle.take()?;
        Some(
            handle
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("audio pump thread panicked"))),
        )
    }

    /// Whether the thread has finished, without blocking on it.
    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_some_and(JoinHandle::is_finished)
    }
}

/// A pump finished and its sink was finalized.
///
/// Carries the [`AudioOut::finalize`] result, which is the only place a sink's
/// closing error can surface — for a WAV, an `Err` here means the header was
/// never patched and the file will not open.
#[derive(Message, Debug)]
pub struct PumpFinished {
    /// The entity that carried the pump. Already had its [`AudioPump`] removed.
    pub entity: Entity,
    pub result: std::io::Result<()>,
}

/// Joins pumps whose thread has finished, and reports what finalizing did.
///
/// Polls rather than blocks: a pump that is still moving frames is left alone,
/// so this costs one atomic load per live pump per frame.
pub fn drain_audio_pumps<S: Send + Sync + 'static, const CH: usize>(
    mut commands: Commands,
    mut pumps: Query<(Entity, &mut AudioPump<S, CH>)>,
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
            bevy_log::error!("audio pump sink failed to finalize: {error}");
        }
        commands.entity(entity).remove::<AudioPump<S, CH>>();
        finished.write(PumpFinished { entity, result });
    }
}

/// Finalize a pump whose component is being removed.
///
/// `On<Remove, AudioPump<S, CH>>` fires at command-flush with the value still
/// readable, mirroring [`unwire_removed_sources`](super::unwire_removed_sources).
/// Without it, despawning an entity mid-recording would drop the `JoinHandle`
/// and detach the thread — the sink is owned *by that thread*, so its
/// `finalize` would never run and the WAV would be left unreadable. Nothing
/// else can reach it, because the entity has already left the drain's query.
///
/// This blocks the frame until the thread notices its flag (one poll, so
/// bounded by the park). That is the cost of the guarantee, and it is only paid
/// on teardown.
pub fn finalize_removed_pumps<S: Send + Sync + 'static, const CH: usize>(
    remove: On<Remove, AudioPump<S, CH>>,
    mut pumps: Query<&mut AudioPump<S, CH>>,
) {
    let entity = remove.event_target();
    let Ok(mut pump) = pumps.get_mut(entity) else {
        return;
    };
    if let Some(Err(error)) = pump.join() {
        bevy_log::error!("audio pump sink failed to finalize on removal: {error}");
    }
}

/// Which `AudioPump<S, CH>` drains are already scheduled.
///
/// `add_systems` does not deduplicate, so without this a frame type registered
/// by both a host and a library plugin would drain twice per frame. The second
/// pass finds `handle` already taken and does nothing, but it doubles the
/// per-frame query cost and makes the schedule depend on how many callers asked.
#[derive(Resource, Default)]
struct RegisteredAudioPumps(std::collections::HashSet<(core::any::TypeId, usize)>);

/// Registers the drain and removal observer for one [`AudioPump`] frame type.
pub trait AudioPumpAppExt {
    /// Drive `AudioPump<S, CH>` — join finished pumps, emit [`PumpFinished`],
    /// and finalize on removal.
    ///
    /// Idempotent, so a host and a library plugin can both declare the frame
    /// type they share.
    ///
    /// ```rust,ignore
    /// app.add_audio_pump::<f32, 2>();   // stereo — mic, WAV
    /// app.add_audio_pump::<f32, 6>();   // 5.1 render
    /// ```
    fn add_audio_pump<S: Send + Sync + 'static, const CH: usize>(&mut self) -> &mut Self;
}

impl AudioPumpAppExt for App {
    fn add_audio_pump<S: Send + Sync + 'static, const CH: usize>(&mut self) -> &mut Self {
        let key = (core::any::TypeId::of::<S>(), CH);
        if !self
            .world_mut()
            .get_resource_or_init::<RegisteredAudioPumps>()
            .0
            .insert(key)
        {
            return self;
        }
        self.add_message::<PumpFinished>();
        self.add_observer(finalize_removed_pumps::<S, CH>);
        // Deliberately not in a `GraphReconcileSystems` phase and not gated on
        // `engine_ready`: a pump touches no graph topology and sets no
        // `GraphDirty`, and a file→WAV pump is valid with no audio device at
        // all. Gating it would tie offline work to a live callback.
        self.add_systems(Update, drain_audio_pumps::<S, CH>)
    }
}
