//! Restarting the output device — at whatever rate it now runs.
//!
//! [`TuttiDriver`] owns the stream and the graph's *audio* side only, so on
//! its own it can re-open a device and nothing more: a device that comes back
//! at another rate would play a graph built for the old one, every oscillator
//! off pitch, the beat clock off tempo, every figure in samples (PDC, a
//! lookahead) wrong — with no error anywhere. That is why a plain
//! `TuttiDriver::restart` now refuses a rate change, and why this module
//! exists: [`restart_device`] runs the driver's restart with a hook that, while
//! no callback runs, moves everything the adapter owns to the new
//! configuration:
//!
//! - the graph: `Net::set_sample_rate` (its clock re-seated on the live
//!   playhead), or `Editor::reprepare` on the native backend, whose second
//!   half lands through the ordinary per-frame `commit_graph` (a crossfade
//!   asked for meanwhile waits in [`PendingCrossfades`](crate::graph::PendingCrossfades));
//! - the engine follows by itself: it adopts the new rate on the first block
//!   that carries it, rescaling its frame clock and every scheduled
//!   `At::Frame` transport command to the same wall-clock time, and stepping
//!   the beat at the new rate — so the playhead continues without a jump;
//! - [`TransportRes`]'s rate, [`AudioConfig`], and the hardware MIDI input's
//!   timestamp rate;
//! - the compensation figures in samples ([`ChannelCompensation`],
//!   [`GraphLatency`]): on `Net` recomputed and published before the first
//!   block, with the delays resized in the same commit; on `Native` once the
//!   re-prepare resumes (see `commit_graph`).
//!
//! **On `Net`, a re-rate loses every unit's live state** (a limitation of
//! `Net`, not of the restart): `Net::set_sample_rate` marks every vertex
//! changed, so the commit swaps in the control side's copies, which have
//! never run. A voice mid-note, a reverb or delay tail, filter memory, an
//! LFO's phase all start from where the control-side copy was built. A
//! hosted plugin keeps its instance — a `PluginClient` clone shares the
//! bridge and the plugin process, and its `set_sample_rate` re-rates that
//! same process — but loses its batching scratch. The native backend keeps
//! every unit instance across a re-prepare and resets only time-based state
//! (`Editor::reprepare`'s rule).
//!
//! **Recovery after a failed hook:** the stream is left stopped, and the
//! driver keeps the old spec and graph rate (`TuttiDriver::graph_rate`). A
//! restart onto the old device at the old rate — `restart_device` with it,
//! or a plain `TuttiDriver::restart` — moves nothing and plays again.
//!
//! Not re-rated here, and recorded in design doc 013: the sampler's disk
//! streamer and the MIDI clock master each keep the rate they were built
//! with (neither has a way to change it yet), and the graph root is not
//! widened to a wider new device — so [`AudioConfig::channels`] keeps the
//! width the graph outputs, not the new device's (the engine folds to the
//! device's width either way).

use bevy_ecs::prelude::*;
use std::sync::Arc;

use tutti_core::Samples;
use tutti_cpal::{OutputSpec, StreamDriver, TuttiDriver};

use crate::engine::{Error, Result};
use crate::graph::latency::{ChannelCompensation, GraphLatency};
use crate::graph::{AudioConfig, AudioGraphRes, GraphBackend, GraphDirty, TransportRes};

/// What [`restart_device`] asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceRestart {
    /// The output device, as [`TuttiDriver::devices`] numbers it; `None` for
    /// the host's default.
    pub device: Option<usize>,
    /// The largest block the native graph is prepared for from the restart
    /// on (`Prepare`'s `MaxBlock`); `None` keeps the one it has. At most the
    /// engine's block capacity (`Engine::graph_block_capacity`): past it the
    /// restart is refused before the stream stops. Ignored on
    /// [`GraphBackend::Net`], which renders any block.
    ///
    /// A host's choice, not the device's: the device hands over whatever
    /// callback it likes, and the engine renders one longer than this as
    /// consecutive graph blocks.
    pub max_block: Option<Samples>,
}

/// Restart the output device and move the graph, the transport and every
/// rate-derived resource to the configuration it comes back with.
///
/// Call from an exclusive system or a queued command
/// (`commands.queue(|world: &mut World| { … })`): it takes the world because
/// the driver is `NonSend` and the hook writes several resources at once.
///
/// **On [`GraphBackend::Net`] a rate change resets every unit's live
/// state** — voices, tails, filter memory, LFO phase — because the commit
/// swaps in never-run control-side copies (see the module docs). Hosted
/// plugins keep their instance. `Native` keeps every unit and resets only
/// what is time-based.
///
/// # Errors
///
/// - [`Error::Reprepare`] **before the stream stops**, changing nothing,
///   when the native graph cannot take the request: `max_block` past the
///   engine's block capacity (`CommitError::BlockTooLong`), a re-prepare
///   already between its halves (`Repreparing`; retry on a later frame), or
///   a poisoned graph.
/// - What resolving or opening the device reports ([`Error::Device`]).
/// - [`Error::Reprepare`] from the re-prepare itself (a graph that no
///   longer compiles at the new block): the stream is then left **stopped**
///   (`TuttiDriver::restart_with`), since nothing may play at a rate the
///   graph was not moved to. Restart again with another `max_block`, or
///   onto the old device and rate, which moves nothing and plays again.
pub fn restart_device(world: &mut World, request: DeviceRestart) -> Result<()> {
    restart(world, request.max_block, |driver, world| {
        driver.restart_with(request.device, |spec| {
            rerate(world, spec, request.max_block)
        })
    })
}

/// [`restart_device`] on a driver of the caller's choosing, with `spec`
/// standing for the config the new device reports: the device-free restart
/// (`TuttiDriver::restart_on`), for a host's tests or a stream it drives
/// itself.
///
/// # Errors
///
/// As [`restart_device`], without the device's own.
pub fn restart_device_on<D: StreamDriver>(
    world: &mut World,
    spec: OutputSpec,
    driver: D,
    max_block: Option<Samples>,
) -> Result<()>
where
    D::Running: 'static,
{
    restart(world, max_block, |d, world| {
        d.restart_on(spec, driver, |spec| rerate(world, spec, max_block))
    })
}

/// The checks that must pass before the stream stops, then `run` with the
/// driver out of the world (it is `NonSend`, and the hook needs the world).
fn restart(
    world: &mut World,
    max_block: Option<Samples>,
    run: impl FnOnce(&mut TuttiDriver, &mut World) -> Result<()>,
) -> Result<()> {
    if max_block.is_some_and(|b| b.is_zero()) {
        return Err(Error::Device(tutti_cpal::Error::InvalidConfig(
            "a zero-frame max block".into(),
        )));
    }
    let Some(graph) = world.get_resource::<AudioGraphRes>() else {
        return Err(not_built());
    };
    graph.check_rerate(max_block)?;
    let Some(mut driver) = world.remove_non_send::<TuttiDriver>() else {
        return Err(not_built());
    };
    // The driver goes back whatever `run` does, a panic included: a world
    // left without it has no stream to restart ever again.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut driver, &mut *world)
    }));
    world.insert_non_send(driver);
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn not_built() -> Error {
    Error::Device(tutti_cpal::Error::InvalidDevice(
        "no audio engine was built, so there is no device to restart".into(),
    ))
}

/// The hook: move everything the adapter owns to `spec`, with no callback
/// running.
fn rerate(world: &mut World, spec: &OutputSpec, max_block: Option<Samples>) -> Result<()> {
    let rate = spec.sample_rate;
    let transport = world
        .get_resource::<TransportRes>()
        .map(|t| t.0.clone())
        .ok_or_else(not_built)?;
    let compensating = world.contains_resource::<GraphLatency>();
    let mut graph = world.resource_mut::<AudioGraphRes>();
    graph.rerate(rate, max_block, &transport)?;
    let backend = graph.backend();
    // `Net`: the re-rated net goes out now, so the first block at the new
    // rate renders it, with its compensation delays resized to the new
    // latencies in the same commit. `Native` sent its re-prepare above;
    // `commit_graph` finishes it, and republishes the figures once it
    // resumes, when the new shapes are known.
    let figures = if backend == GraphBackend::Net {
        let figures = compensating.then(|| graph.compensate()).flatten();
        graph.commit();
        figures
    } else {
        None
    };
    if let Some(figures) = figures {
        if let Some(published) = world.get_resource::<ChannelCompensation>() {
            published.0.publish(Arc::new(figures.channels));
        }
        world.resource_mut::<GraphLatency>().0 = figures.total;
    }
    if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
        dirty.0 = true;
    }

    transport.set_sample_rate(rate);
    // The port manager turns an event's wall-clock arrival into a frame
    // offset at this rate; its contract is to be told before the stream
    // starts, which is where the hook runs.
    #[cfg(feature = "midi-hardware")]
    if let Some(io) = world.get_resource::<crate::midi::MidiIoRes>() {
        io.ports().set_sample_rate(rate);
    }
    // The graph's width, not the new device's: the root is not widened on
    // a restart (doc 013), and the engine folds it to whatever the device is.
    let channels = world
        .get_resource::<AudioConfig>()
        .map_or(spec.channels, |c| c.channels);
    world.insert_resource(AudioConfig {
        sample_rate: rate,
        channels,
    });
    Ok(())
}

/// A device restart from 44.1 kHz to 48 kHz through the whole adapter —
/// `build_on` (the device-free `build_into`), the reconcile pipeline,
/// compensation — over `ManualStreamDriver`s standing for the two devices.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::build::build_on;
    use crate::graph::both_backends;
    use crate::graph::{GraphReconcilePlugin, MasterSources, PortSource, PortSources};
    use crate::graph::{PendingCrossfades, SpawnAudioNode};
    use crate::{AudioEngineState, LatencyCompensationPlugin, TuttiPlugin};
    use bevy_app::App;
    use tutti_core::{At, ChannelLayout, Db, Frame, Hz, MotionEvent, SampleRate};
    use tutti_cpal::{AudioEngine, ManualStream, ManualStreamDriver};
    use tutti_nodes::testing::Osc;
    use tutti_nodes::LimiterNode;

    const OLD: f64 = 44_100.0;
    const NEW: f64 = 48_000.0;

    fn spec_at(rate: f64) -> OutputSpec {
        OutputSpec::new(
            SampleRate(rate),
            ChannelLayout::STEREO,
            tutti_cpal::cpal::SampleFormat::F32,
        )
    }

    /// An engine built at 44.1 kHz on `backend`, its stream a manual one:
    /// a 1 kHz sine on output 0 and, through a lookahead limiter, on output
    /// 1 — so the dry channel pre-rolls by the limiter's latency.
    fn engine_app(backend: GraphBackend) -> (App, ManualStream, Entity, Entity) {
        let mut app = App::new();
        let plugin = TuttiPlugin {
            graph_backend: backend,
            ..Default::default()
        };
        let (driver, stream) = ManualStreamDriver::new();
        build_on(
            &plugin,
            &mut app,
            AudioEngine::from_spec(spec_at(OLD)),
            |engine, state| engine.start_with(state, driver),
        )
        .expect("builds with no device");
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, LatencyCompensationPlugin));
        let mut commands = app.world_mut().commands();
        let osc = commands.spawn_audio_node(Osc::sine(Hz(1_000.0))).id();
        let lim = commands
            .spawn_audio_node(LimiterNode::with_channels(
                ChannelLayout::MONO,
                Db(0.0),
                Db(0.0),
            ))
            .insert(PortSources::from(osc))
            .id();
        commands.insert_resource(
            MasterSources::default()
                .with(0, PortSource::node(osc))
                .with(1, PortSource::node(lim)),
        );
        app.world_mut().flush();
        app.update();
        (app, stream, osc, lim)
    }

    /// Render `blocks` device blocks of `frames` on `stream`, a frame of the
    /// app after each (as a host's frames interleave with its callbacks):
    /// the left channel.
    fn play(app: &mut App, stream: &ManualStream, frames: usize, blocks: usize) -> Vec<f32> {
        let mut left = Vec::new();
        for _ in 0..blocks {
            let block = stream.render_block(frames).expect("the stream is open");
            left.extend(block.iter().step_by(2));
            app.update();
        }
        left
    }

    /// Mean period, in frames, of the rising zero crossings in `x`
    /// (interpolated between the two frames that straddle each).
    fn period(x: &[f32]) -> f64 {
        let crossings: Vec<f64> = (1..x.len())
            .filter(|&i| x[i - 1] < 0.0 && x[i] >= 0.0)
            .map(|i| (i - 1) as f64 + f64::from(x[i - 1] / (x[i - 1] - x[i])))
            .collect();
        assert!(crossings.len() > 10, "a tone to measure");
        (crossings[crossings.len() - 1] - crossings[0]) / (crossings.len() - 1) as f64
    }

    fn transport(app: &App) -> tutti_core::Transport {
        app.world().resource::<TransportRes>().0.clone()
    }

    fn latency_of(app: &App, entity: Entity) -> Samples {
        let node = *app
            .world()
            .get::<tutti_core::AudioNode>(entity)
            .expect("a node");
        app.world().resource::<AudioGraphRes>().node_latency(node)
    }

    /// **A restart onto a 48 kHz device re-rates everything on both
    /// backends.** One second at 44.1 kHz, rolling at 120 BPM with a stop
    /// scheduled at `Frame(66 150)` (1.5 s at 44.1 kHz); then the restart,
    /// and one second on the new device:
    ///
    /// - the 1 kHz sine is 48 frames a cycle, not 44.1 (the graph runs at
    ///   the device's rate);
    /// - the beat carries on from 2.0 without a jump: 2.02 after the first
    ///   480-frame block (on `Native` that block is the re-prepare's silent
    ///   one, and the transport rolls through it at the new rate);
    /// - the stop keeps its wall-clock time: it lands 0.5 s after the
    ///   restart, on frame 72 000 (beat 3.0 to the frame);
    /// - a play scheduled *after* the restart, at 48 kHz frame 84 000, is
    ///   not rescaled a second time when the engine adopts the rate: it
    ///   lands on its frame (beat 3.5 half a beat later, to the frame);
    /// - `AudioConfig`, the transport's rate, and the PDC figures
    ///   (`GraphLatency`, the dry channel's pre-roll) are the new ones —
    ///   the limiter's lookahead is a time, so its latency in samples moved.
    ///
    /// Mutations (run):
    /// - `rerate` not calling `AudioGraphRes::rerate` → the beat steps at
    ///   44.1 kHz on both (and, past that check, the sine measures 44.1
    ///   frames a cycle);
    /// - `AudioGraphRes::rerate` not re-seating the `Net` clock → `Net`'s
    ///   beat restarts from 0 → fails;
    /// - `rerate` not republishing `AudioConfig` (or not setting the
    ///   transport's rate) → fails on each;
    /// - `Schedule::rescale` ignoring the `mark_rate_change` boundary → the
    ///   play at 84 000 moves to ~91 429 → beat ~3.19 → fails;
    /// - `rerate` not compensating on `Net` → the figures are the old ones
    ///   until the next frame's `compensate_graph` → `Net` fails the check
    ///   made before the first block; `commit_graph` clearing the flag on
    ///   the frame a re-prepare resumes → `Native`'s stay old for good.
    fn a_restart_at_a_new_rate_re_rates_the_graph_and_everything_on_it(backend: GraphBackend) {
        let (mut app, old, _, lim) = engine_app(backend);
        let t = transport(&app);
        t.motion.try_send(MotionEvent::Play).expect("room");
        t.motion
            .schedule(At::Frame(Frame(66_150)), MotionEvent::stop_now())
            .expect("room");
        let before = play(&mut app, &old, 441, 100);
        assert!(
            (period(&before[4_410..]) - 44.1).abs() < 0.01,
            "{backend:?}: 1 kHz at 44.1 kHz"
        );
        assert!((t.settings.beat().get() - 2.0).abs() < 1e-9, "one second");
        let old_latency = latency_of(&app, lim);
        assert_eq!(
            app.world().resource::<GraphLatency>().0,
            old_latency,
            "{backend:?}: the 44.1 kHz figure is published"
        );

        let (driver, new) = ManualStreamDriver::new();
        restart_device_on(app.world_mut(), spec_at(NEW), driver, None).expect("restarts");
        assert!(!old.is_open() && new.is_open(), "on the new device");
        assert_eq!(
            *app.world().resource::<AudioConfig>(),
            AudioConfig {
                sample_rate: SampleRate(NEW),
                channels: ChannelLayout::STEREO,
            }
        );
        assert_eq!(t.sample_rate(), SampleRate(NEW), "the transport's rate");
        if backend == GraphBackend::Net {
            // Before the first block: the delays that commit carried were
            // sized for the new latencies, and the figures say so.
            assert_eq!(
                app.world().resource::<GraphLatency>().0,
                latency_of(&app, lim),
                "Net compensates in the restart itself"
            );
        }

        // Sent after the restart, before the first block at the new rate:
        // already in 48 kHz frames (1.75 s of wall clock), so the rate's
        // adoption must leave it where it is.
        t.motion
            .schedule(At::Frame(Frame(84_000)), MotionEvent::Play)
            .expect("room");

        let mut after = play(&mut app, &new, 480, 1);
        assert!(
            (t.settings.beat().get() - 2.02).abs() < 1e-9,
            "{backend:?}: the beat carries on from 2.0 at the new rate, not from {}",
            t.settings.beat().get()
        );
        after.extend(play(&mut app, &new, 480, 54));
        // One frame of 48 kHz is 1/24 000 of a beat: 1e-9 pins the frame.
        assert!(t.motion.is_stopped(), "{backend:?}: the stop landed");
        assert!(
            (t.settings.beat().get() - 3.0).abs() < 1e-9,
            "{backend:?}: the stop lands on 1.5 s of wall clock, frame 72 000 \
             (beat 3), not at beat {}",
            t.settings.beat().get()
        );
        after.extend(play(&mut app, &new, 480, 45));
        assert!(t.motion.is_playing(), "{backend:?}: the later play landed");
        assert!(
            (t.settings.beat().get() - 3.5).abs() < 1e-9,
            "{backend:?}: rolling again from frame 84 000, 12 000 frames to go \
             (beat 3.5), not beat {}",
            t.settings.beat().get()
        );
        assert!(
            (period(&after[4_800..]) - 48.0).abs() < 0.01,
            "{backend:?}: 1 kHz at 48 kHz, measured {}",
            period(&after[4_800..])
        );

        let new_latency = latency_of(&app, lim);
        assert_ne!(new_latency, old_latency, "the lookahead moved in samples");
        assert_eq!(app.world().resource::<GraphLatency>().0, new_latency);
        let table = app.world().resource::<ChannelCompensation>().0.read();
        assert_eq!(
            table.first().copied(),
            Some(new_latency),
            "{backend:?}: the dry channel pre-rolls by the new latency"
        );
    }
    both_backends!(a_restart_at_a_new_rate_re_rates_the_graph_and_everything_on_it);

    /// **A block past the engine's capacity is refused before the stream
    /// stops** — the old device plays on, at the old configuration, and the
    /// host gets the graph's reason. The capacity is the engine's
    /// (`DEFAULT_GRAPH_BLOCK_CAPACITY`, 8 192 frames).
    ///
    /// Mutation (run): `NativeGraph::check_reprepare` not checking the
    /// block → the stream stops first and the refusal comes from the
    /// re-prepare in the hook, with the device stopped → fails.
    #[test]
    fn a_restart_past_the_block_capacity_is_refused_and_keeps_playing() {
        let (mut app, old, _, _) = engine_app(GraphBackend::Native);
        let (driver, new) = ManualStreamDriver::new();
        let err = restart_device_on(app.world_mut(), spec_at(NEW), driver, Some(Samples(16_384)))
            .expect_err("past the capacity");
        assert!(
            matches!(
                err,
                Error::Reprepare(tutti_graph::CommitError::BlockTooLong {
                    max_block: 16_384,
                    limit: 8_192
                })
            ),
            "{err}"
        );
        assert!(old.is_open() && !new.is_open(), "the old device plays on");
        assert_eq!(
            app.world().resource::<AudioConfig>().sample_rate,
            SampleRate(OLD)
        );
        assert!(play(&mut app, &old, 441, 10).iter().any(|&x| x != 0.0));

        // Within it, the same restart goes through — onto a 5.1 device,
        // whose width `AudioConfig` does not take: the root stays stereo
        // (it is not widened on a restart), and the engine folds.
        let (driver, new) = ManualStreamDriver::new();
        let surround = OutputSpec::new(
            SampleRate(NEW),
            ChannelLayout::from(6usize),
            tutti_cpal::cpal::SampleFormat::F32,
        );
        restart_device_on(app.world_mut(), surround, driver, Some(Samples(2_048)))
            .expect("at a block the engine holds");
        assert!(new.is_open());
        // Mutation (run): `rerate` publishing `spec.channels` → 6 → fails.
        assert_eq!(
            app.world().resource::<AudioConfig>().channels,
            ChannelLayout::STEREO
        );
    }

    /// **After a hook fails, a restart onto the old device and rate plays
    /// again.** The hook fails with the stream stopped (here: the
    /// transport resource is gone, so it cannot re-rate); the driver keeps
    /// the old spec and graph rate, and `AudioConfig` is untouched. Then a
    /// restart at 44.1 kHz moves nothing and renders the 1 kHz sine at 44.1
    /// frames a cycle.
    ///
    /// Mutation (run): `TuttiDriver::rerate_or_restore` adopting the new
    /// rate on failure → the driver claims 48 kHz → fails.
    fn a_restart_after_a_failed_hook_recovers_on_the_old_device(backend: GraphBackend) {
        let (mut app, old, _, _) = engine_app(backend);
        let t = app
            .world_mut()
            .remove_resource::<TransportRes>()
            .expect("built");
        let (driver, new) = ManualStreamDriver::new();
        restart_device_on(app.world_mut(), spec_at(NEW), driver, None)
            .expect_err("the hook cannot re-rate");
        assert!(!old.is_open() && !new.is_open(), "left stopped");
        let driver = app.world().non_send::<TuttiDriver>();
        assert_eq!(driver.graph_rate(), SampleRate(OLD), "{backend:?}");
        assert_eq!(driver.spec().sample_rate, SampleRate(OLD), "{backend:?}");
        assert_eq!(
            app.world().resource::<AudioConfig>().sample_rate,
            SampleRate(OLD)
        );

        app.world_mut().insert_resource(t);
        let (driver, back) = ManualStreamDriver::new();
        restart_device_on(app.world_mut(), spec_at(OLD), driver, None).expect("recovers");
        assert!(back.is_open());
        let out = play(&mut app, &back, 441, 20);
        assert!(
            (period(&out[4_410..]) - 44.1).abs() < 0.01,
            "{backend:?}: plays again at 44.1 kHz"
        );
    }
    both_backends!(a_restart_after_a_failed_hook_recovers_on_the_old_device);

    /// **The driver goes back into the world even if the restart panics**:
    /// a world left without it has no stream to restart again.
    ///
    /// Mutation (run): `restart` calling `run` without `catch_unwind` → the
    /// panic skips the reinsert → the driver is gone → fails.
    #[test]
    fn a_panicking_restart_puts_the_driver_back() {
        let (mut app, _old, _, _) = engine_app(GraphBackend::Net);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = restart(app.world_mut(), None, |_, _| panic!("mid-restart"));
        }));
        assert!(caught.is_err(), "the panic propagates");
        assert!(app.world().get_non_send::<TuttiDriver>().is_some());
    }

    /// **A crossfade asked for while the restart's re-prepare is between its
    /// halves waits, and lands once it resumes** (#32's `PendingCrossfades`):
    /// the native graph refuses a replace then, and consumes what it
    /// refuses. The incoming 2 kHz sine plays at the new rate: 24 frames a
    /// cycle.
    ///
    /// Mutation (run): `apply_crossfade` dropping a busy unit instead of
    /// parking it → nothing waits, and the 1 kHz sine would play on →
    /// fails.
    #[test]
    fn a_crossfade_during_the_restart_lands_after_it() {
        let (mut app, _old, osc, _) = engine_app(GraphBackend::Native);
        let (driver, new) = ManualStreamDriver::new();
        restart_device_on(app.world_mut(), spec_at(NEW), driver, None).expect("restarts");
        crate::graph::crossfade_audio_node(
            &mut app.world_mut().commands(),
            osc,
            Box::new(Osc::sine(Hz(2_000.0))),
        );
        app.world_mut().flush();
        assert_eq!(app.world().resource::<PendingCrossfades>().len(), 1);
        let out = play(&mut app, &new, 480, 50);
        assert!(app.world().resource::<PendingCrossfades>().is_empty());
        assert!(
            (period(&out[4_800..]) - 24.0).abs() < 0.01,
            "2 kHz at 48 kHz, measured {}",
            period(&out[4_800..])
        );
    }
}
