//! Engine construction: the session builds the engine, and this publishes
//! every subsystem it holds as a Bevy resource.
//!
//! The ordered, fallible RT-wiring transaction (the device opened first, the
//! graph built at its rate, the click inserted, the callback made live once)
//! is [`SessionBuilder`]'s. What is left here is the glue: the session goes
//! into [`AudioGraphRes`], the `!Send` [`Device`] into a non-send resource,
//! and every handle the session shares (transport, click settings, meter,
//! tap, compensation, MIDI nodes, disk streamer) into its own resource, with
//! an entity for each engine-built node.
//!
//! There is no public `TuttiEngine` bundle: construction inserts directly.

use bevy_app::App;
use bevy_ecs::world::World;
use tutti_session::{Device, Session, SessionBuilder};

use crate::engine::Result;
use crate::graph::{
    AudioConfig, AudioGraphRes, AudioNode, EngineNodes, MeteringRes, MetronomeRes, TransportRes,
};

#[cfg(feature = "midi-hardware")]
use crate::midi::MidiIoRes;
#[cfg(feature = "midi")]
use crate::midi::{ClockMasterRes, MidiEngineNodes, MpeModeRes};

#[cfg(feature = "sampler")]
use crate::sampler::DiskStreamerRes;

/// Builds the engine from a [`TuttiPlugin`](crate::TuttiPlugin) config and inserts
/// every subsystem resource into `app`. The audio callback is live on return.
///
/// [`TuttiPlugin`](crate::TuttiPlugin) calls this during plugin build (unless
/// `disabled`) and records the outcome in
/// [`AudioEngineState`](crate::AudioEngineState). It opens the output device
/// (`plugin.output_device`, or the system default) through
/// [`SessionBuilder::build_device`], which builds the graph at the device's
/// rate, and inserts [`AudioGraphRes`], [`AudioConfig`], [`TransportRes`],
/// [`MetronomeRes`], [`MeteringRes`] (enabled),
/// [`EngineNodes`] and the [`Device`] (as a non-send resource), plus the MIDI
/// and sampler resources when those features are on.
///
/// # Errors
///
/// Returns an error when the device cannot be opened or started, or the graph
/// cannot be built over it. On `Err`, nothing is inserted; the
/// [`engine_ready`](crate::graph::engine_ready) run condition gates every
/// engine-dependent system, so the app proceeds without audio.
pub fn build_into(plugin: &crate::TuttiPlugin, app: &mut App) -> Result<()> {
    let device = plugin.output_device.clone();
    build_on(plugin, app, |builder| builder.build_device(device))
}

/// [`build_into`], the session built by `build` from the plugin's options:
/// the device-free build, when `build` is
/// [`SessionBuilder::build_device_on`] over a `ManualStreamDriver`, every
/// subsystem built and published as a device would have them, with no sound
/// card.
pub(crate) fn build_on(
    plugin: &crate::TuttiPlugin,
    app: &mut App,
    build: impl FnOnce(SessionBuilder) -> Result<(Session, Device)>,
) -> Result<()> {
    build_in(plugin, app.world_mut(), build)
}

/// Starts the session's device watch on `source`, with the plugin's policy
/// and selector, and marks the list as the watch's.
pub(crate) fn watch_with(
    world: &mut World,
    plugin: &crate::TuttiPlugin,
    session: &mut Session,
    device: &Device,
    source: impl tutti_cpal::SnapshotSource + Send + 'static,
) {
    let Some(policy) = plugin.watch_devices else {
        return;
    };
    session.watch_devices(
        source,
        policy,
        plugin.output_device.clone(),
        device.opened_id(&plugin.output_device),
    );
    world.insert_resource(super::device_state::DevicesWatched);
}

/// [`build_on`] into a world: the engine built after the app started (a
/// start that failed and was retried, design 018).
pub(crate) fn build_in(
    plugin: &crate::TuttiPlugin,
    world: &mut World,
    build: impl FnOnce(SessionBuilder) -> Result<(Session, Device)>,
) -> Result<()> {
    build_with(plugin, world, build, Watching::Spawn)
}

/// Where a built engine's device watch gets its snapshots.
pub(crate) enum Watching {
    /// A new poller, if the plugin watches devices.
    Spawn,
    /// None: the caller installs the watch.
    Skip,
}

/// [`build_in`], with the device watch as `source` says.
pub(crate) fn build_with(
    plugin: &crate::TuttiPlugin,
    world: &mut World,
    build: impl FnOnce(SessionBuilder) -> Result<(Session, Device)>,
    source: Watching,
) -> Result<()> {
    #[cfg_attr(not(feature = "midi"), allow(unused_mut))]
    let mut builder = Session::builder()
        .inputs(plugin.inputs)
        .outputs_floor(plugin.outputs)
        .render_workers(plugin.render_workers);
    if let Some(start) = plugin.render_worker_start.clone() {
        builder = builder.render_worker_start(move |i| start.call(i));
    }
    #[cfg(feature = "plugin")]
    {
        builder = builder.plugin_recovery(plugin.plugin_recovery);
    }
    #[cfg(feature = "midi-hardware")]
    {
        builder = builder.midi_hardware();
    }
    // The hardware input ingests MPE in the app's `MpeModeConfig`, inserted
    // before the engine builds (default `Disabled`).
    #[cfg(feature = "midi")]
    if let Some(mode) = world.get_resource::<crate::midi::MpeModeConfig>() {
        builder = builder.mpe_mode(mode.0);
    }
    #[cfg_attr(not(feature = "sampler"), allow(unused_mut))]
    let (mut session, device) = build(builder)?;
    if let (Watching::Spawn, Some(policy)) = (source, plugin.watch_devices) {
        match policy.spawn_poller(tutti_cpal::AudioHost::Default) {
            Ok(poller) => watch_with(world, plugin, &mut session, &device, poller),
            Err(e) => bevy_log::warn!("the device poller did not start: {e}"),
        }
    }

    // --- Publish every subsystem. On `Err` above none of this runs, so
    // `engine_ready` stays an exact proxy for "the callback is live". ---
    let config = AudioConfig {
        sample_rate: device.spec().sample_rate,
        channels: device.spec().channels,
    };
    // The session's own table: the disk streamer pre-rolls by it.
    // `GraphReconcilePlugin` (and `LatencyCompensationPlugin`) use
    // `init_resource`, which leaves it.
    let compensation =
        crate::graph::latency::ChannelCompensation(std::sync::Arc::clone(session.compensation()));
    let transport = session.transport().clone();
    let metronome = std::sync::Arc::clone(session.click_settings());
    let meter = session.take_master_meter();
    let click = session.click_node();
    #[cfg(feature = "sampler")]
    let disk_streamer = session.take_disk_streamer();
    #[cfg(feature = "midi")]
    let midi_nodes = session.midi_nodes().cloned();
    #[cfg(feature = "midi-hardware")]
    let midi_io = session.midi_io().cloned();

    world.insert_resource(AudioGraphRes::from_session(session));
    world.insert_resource(config);
    world.insert_resource(compensation);
    world.insert_non_send(device);
    world.insert_resource(TransportRes(transport));
    world.insert_resource(MetronomeRes(metronome));
    // The engine-built node gets an entity like everything else in the graph,
    // and `EngineNodes` publishes it. Spawned here, before any host system
    // runs, it is otherwise unreachable — and `PortSources` names sources by
    // `Entity`, so an unnameable node is an unwirable one. The click's output
    // is left unwired so that the host declares where it lands.
    if let Some(click) = click {
        let click_entity = world.spawn(AudioNode(click)).id();
        world.insert_resource(EngineNodes {
            click: click_entity,
        });
    }
    // Consumers read `MeteringRes` directly; the graph enabled it.
    // A session always declares one.
    if let Some(meter) = meter {
        world.insert_resource(MeteringRes(meter));
    }

    #[cfg(feature = "midi")]
    if let Some(nodes) = midi_nodes {
        // The MIDI nodes get entities like every node, so a host can wire to
        // (and from) them by entity: the route rules feed from the input's
        // ports, and anything declared on the hardware out reaches the wire
        // beside the clock (the session's `"clock"` feed).
        let input_entity = world.spawn(AudioNode(nodes.input)).id();
        let clock_entity = world.spawn(AudioNode(nodes.clock)).id();
        let out_entity = world.spawn(AudioNode(nodes.hardware_out)).id();
        world.insert_resource(MidiEngineNodes {
            input: input_entity,
            input_node: AudioNode(nodes.input),
            clock: clock_entity,
            hardware_out: out_entity,
        });
        world.insert_resource(MpeModeRes(nodes.input_controls));
        world.insert_resource(ClockMasterRes::new(nodes.clock_master));
    }
    #[cfg(feature = "midi-hardware")]
    if let Some(io) = midi_io {
        world.insert_resource(MidiIoRes(io));
    }

    #[cfg(feature = "sampler")]
    if let Some(streamer) = disk_streamer {
        world.insert_resource(DiskStreamerRes(streamer));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use tutti_types::ChannelLayout;

    /// **The engine publishes the meter its callback feeds**, not a fresh
    /// disconnected one: `MeteringRes` reads the level the device renders, and
    /// a tap opened through `AudioGraphRes` sees the frames the callback
    /// renders. Built device-free (`build_on` over a `ManualStreamDriver`), so
    /// it runs in CI. The adapter half: the session's own (`tutti-session`'s
    /// `the_session_publishes_the_meter_and_taps_its_callback_feeds`) pins that
    /// the meter and taps are the callback's.
    ///
    /// Mutation (run, recorded here): `build_into` inserting a
    /// `MeteringRes` over a reader of a meter nothing feeds (a detached
    /// session's) instead of the session's own -> the level reads 0.0.
    #[test]
    fn the_engine_publishes_the_meter_and_taps_its_callback_feeds() {
        use crate::graph::{AudioGraphRes, MeteringRes};
        use tutti_cpal::{ManualStreamDriver, OutputSpec};
        use tutti_types::SampleRate;

        let mut app = bevy_app::App::new();
        let (driver, stream) = ManualStreamDriver::new();
        super::build_on(&crate::TuttiPlugin::default(), &mut app, |builder| {
            builder.build_device_on(
                OutputSpec::new(
                    SampleRate(48_000.0),
                    ChannelLayout::STEREO,
                    tutti_cpal::cpal::SampleFormat::F32,
                ),
                driver,
            )
        })
        .expect("builds with no device");
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let (dc, _) = graph.graph().insert(tutti_nodes::testing::Const::mono(0.5));
        graph
            .graph()
            .set_output_source(0, crate::graph::GraphSource::Node(dc, 0));
        let mut tap = graph
            .graph()
            .open_output_tap(tutti_types::Samples(1_024))
            .expect("the sources exist");
        graph.session().update();
        stream.render_block(64).expect("the stream is open");
        assert_eq!(
            tap.available(),
            tutti_types::Samples(64),
            "the callback's block reached the tap"
        );
        let mut got = [f32::NAN; 128];
        assert_eq!(tap.pop_into(&mut got), tutti_types::Samples(64));
        assert!(got.as_chunks::<2>().0.iter().all(|f| *f == [0.5, 0.0]));
        let level = app.world_mut().resource_mut::<MeteringRes>().read()[0]
            .peak
            .get();
        assert!(
            (level - 0.5).abs() < 1e-6,
            "the callback's block was measured: {level}"
        );
    }

    /// **The resource reads the load its callback's engine measures**: two
    /// device blocks are two blocks on `AudioGraphRes::load_meter`.
    ///
    /// Mutation (run): `AudioGraphRes::load_meter` returning `None` →
    /// fails.
    #[test]
    fn the_resource_reads_the_load_its_callback_measures() {
        use crate::graph::AudioGraphRes;
        use tutti_cpal::{ManualStreamDriver, OutputSpec};
        use tutti_types::SampleRate;

        let mut app = bevy_app::App::new();
        let (driver, stream) = ManualStreamDriver::new();
        super::build_on(&crate::TuttiPlugin::default(), &mut app, |builder| {
            builder.build_device_on(
                OutputSpec::new(
                    SampleRate(48_000.0),
                    ChannelLayout::STEREO,
                    tutti_cpal::cpal::SampleFormat::F32,
                ),
                driver,
            )
        })
        .expect("builds with no device");
        let meter = app
            .world()
            .resource::<AudioGraphRes>()
            .load_meter()
            .expect("a device's engine");
        stream.render_block(64).expect("the stream is open");
        stream.render_block(64).expect("the stream is open");
        assert_eq!(meter.take().blocks, 2);
    }

    /// **The plugin's `render_worker_start` reaches the session's pool**: the
    /// hook runs once for each of three helpers, indices 1..=3.
    ///
    /// Mutation (run): `build_with` not passing the field to the builder: no
    /// index is seen, so it fails.
    #[test]
    fn the_plugins_render_worker_start_reaches_the_helpers() {
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};
        use tutti_cpal::{ManualStreamDriver, OutputSpec};
        use tutti_types::SampleRate;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let hook = Arc::clone(&seen);
        let plugin = crate::TuttiPlugin {
            render_workers: 4,
            render_worker_start: Some(tutti_session::RenderWorkerStart::new(move |i| {
                hook.lock().unwrap().push(i);
            })),
            ..Default::default()
        };
        let mut app = bevy_app::App::new();
        let (driver, _stream) = ManualStreamDriver::new();
        super::build_on(&plugin, &mut app, |builder| {
            builder.build_device_on(
                OutputSpec::new(
                    SampleRate(48_000.0),
                    ChannelLayout::STEREO,
                    tutti_cpal::cpal::SampleFormat::F32,
                ),
                driver,
            )
        })
        .expect("builds with no device");
        let t = Instant::now();
        while seen.lock().unwrap().len() < 3 && t.elapsed() < Duration::from_secs(5) {
            std::thread::yield_now();
        }
        let mut v = seen.lock().unwrap().clone();
        v.sort_unstable();
        assert_eq!(v, [1, 2, 3]);
    }
}
