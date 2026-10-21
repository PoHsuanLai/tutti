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
use tutti_session::{Device, Session, SessionBuilder};

use crate::engine::Result;
use crate::graph::{
    AudioConfig, AudioGraphRes, AudioTapRes, EngineNodes, MeteringRes, MetronomeRes, TransportRes,
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
/// [`MetronomeRes`], [`MeteringRes`] (enabled), [`AudioTapRes`] (closed),
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
    let device = tutti_cpal::DeviceSelector::from(plugin.output_device);
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
    #[cfg_attr(not(feature = "midi"), allow(unused_mut))]
    let mut builder = Session::builder()
        .inputs(plugin.inputs)
        .outputs_floor(plugin.outputs)
        .render_workers(plugin.render_workers);
    #[cfg(feature = "midi-hardware")]
    {
        builder = builder.midi_hardware(true);
    }
    // The hardware input ingests MPE in the app's `MpeModeConfig`, inserted
    // before the engine builds (default `Disabled`).
    #[cfg(feature = "midi")]
    if let Some(mode) = app.world().get_resource::<crate::midi::MpeModeConfig>() {
        builder = builder.mpe_mode(mode.0);
    }
    #[cfg_attr(not(feature = "sampler"), allow(unused_mut))]
    let (mut session, device) = build(builder)?;

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
    let meter = session.meter().clone();
    let tap = session.tap().clone();
    let click = session.click_node();
    #[cfg(feature = "sampler")]
    let disk_streamer = session.take_disk_streamer();
    #[cfg(feature = "midi")]
    let midi_nodes = session.midi_nodes().cloned();
    #[cfg(feature = "midi-hardware")]
    let midi_io = session.midi_io().cloned();

    app.insert_resource(AudioGraphRes::from_session(session));
    app.insert_resource(config);
    app.insert_resource(compensation);
    app.insert_non_send(device);
    app.insert_resource(TransportRes(transport));
    app.insert_resource(MetronomeRes(metronome));
    // The engine-built node gets an entity like everything else in the graph,
    // and `EngineNodes` publishes it. Spawned here, before any host system
    // runs, it is otherwise unreachable — and `PortSources` names sources by
    // `Entity`, so an unnameable node is an unwirable one. The click's output
    // is left unwired so that the host declares where it lands.
    if let Some(click) = click {
        let click_entity = app.world_mut().spawn(click).id();
        app.insert_resource(EngineNodes {
            click: click_entity,
        });
    }
    // Consumers read `MeteringRes::get()` directly; the session enabled it.
    app.insert_resource(MeteringRes(meter));
    // Deliberately NOT opened: while closed, `AudioTap::push` on the audio
    // thread is one atomic load and a return, so a host that never analyses
    // pays nothing. `AudioTapRes::open()` is the switch, and it hands back a
    // consumer the caller owns.
    app.insert_resource(AudioTapRes(tap));

    #[cfg(feature = "midi")]
    if let Some(nodes) = midi_nodes {
        // The MIDI nodes get entities like every node, so a host can wire to
        // (and from) them by entity: the route rules feed from the input's
        // ports, and anything declared on the hardware out reaches the wire
        // beside the clock (the session's `"clock"` feed).
        let input_entity = app.world_mut().spawn(nodes.input).id();
        let clock_entity = app.world_mut().spawn(nodes.clock).id();
        let out_entity = app.world_mut().spawn(nodes.hardware_out).id();
        app.insert_resource(MidiEngineNodes {
            input: input_entity,
            input_node: nodes.input,
            clock: clock_entity,
            hardware_out: out_entity,
        });
        app.insert_resource(MpeModeRes(nodes.input_controls));
        app.insert_resource(ClockMasterRes::new(nodes.clock_master));
    }
    #[cfg(feature = "midi-hardware")]
    if let Some(io) = midi_io {
        app.insert_resource(MidiIoRes(io));
    }

    #[cfg(feature = "sampler")]
    if let Some(streamer) = disk_streamer {
        app.insert_resource(DiskStreamerRes(streamer));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use tutti_core::ChannelLayout;

    /// **The engine publishes the tap its callback feeds**, not a fresh
    /// disconnected one: a host opens `AudioTapRes` and sees the frames the
    /// callback renders. Built device-free (`build_on` over a
    /// `ManualStreamDriver`), so it runs in CI. The adapter half: the
    /// session's own (`tutti-session`'s
    /// `the_session_publishes_the_tap_its_callback_feeds`) pins that
    /// `Session::tap` is the callback's.
    ///
    /// Mutation (run): `build_on` inserting `AudioTapRes(AudioTap::new())`
    /// instead of the session's `tap` → the opened consumer sees nothing.
    #[test]
    fn the_engine_publishes_the_tap_its_callback_feeds() {
        use crate::graph::AudioTapRes;
        use tutti_core::SampleRate;
        use tutti_cpal::{ManualStreamDriver, OutputSpec};

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
        let mut consumer = app
            .world()
            .resource::<AudioTapRes>()
            .open()
            .expect("a fresh tap opens");
        stream.render_block(64).expect("the stream is open");
        let mut frames = 0;
        while consumer.try_pop().is_some() {
            frames += 1;
        }
        assert_eq!(frames, 64, "the callback's block reached the published tap");
    }
}
