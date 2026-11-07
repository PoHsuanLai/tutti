//! A `Reflect`-able mirror of what the audio device is doing, for a UI to read.
//!
//! Everything here is derived from the [`Device`] and [`AudioConfig`]; nothing
//! writes back. The device is `NonSend` and neither it nor `AudioConfig` is
//! `Reflect`, so a status panel that wants an inspector-visible resource reads
//! this instead of the device layer directly.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;
use tutti_session::{DeviceRestarted, Phase, SessionEvent};

use crate::graph::{AudioConfig, AudioGraphRes, SessionPhaseSystems, engine_ready};
use crate::{AudioEngineState, Device};
use tutti_cpal::{DeviceId, Direction, StreamFault};
use tutti_types::ChannelLayout;

/// Audio device state, mirrored from the driver each frame.
///
/// Output-only: this is the read side, and writing a field changes nothing about
/// the device — it is overwritten by the next sync.
#[derive(Resource, Debug, Clone, Reflect)]
#[reflect(Resource, Default, Clone)]
pub struct AudioDeviceState {
    /// Every output device the host lists, by name (an order among
    /// same-named devices is appended: `USB Audio #1`).
    ///
    /// With [`TuttiPlugin::watch_devices`](crate::TuttiPlugin::watch_devices)
    /// on, kept current from the session's device events. Without it,
    /// enumerated once at startup and not refreshed.
    pub output_devices: Vec<String>,
    /// The host's default output, if it has one. Kept current only while
    /// devices are watched.
    pub default_output: Option<String>,
    /// The device the stream is open on, by name. Empty until enumeration runs,
    /// or if the driver declined to name it.
    pub current_device: String,
    /// Whether the CPAL stream is live. Mirrors [`Device::is_running`].
    pub is_running: bool,
    /// The device's channel layout, from [`AudioConfig`] — the width the graph
    /// root folds into, not the root's own width.
    #[reflect(ignore)]
    pub channels: ChannelLayout,
    /// Backend faults reported since the stream last started.
    ///
    /// Non-zero means the audio backend reported an error. A fault does not
    /// stop the stream by itself, so `is_running` can stay true on a device
    /// that has been disconnected; a status display checks this too.
    pub stream_faults: u64,
    /// The most recent fault's message, for a status line: the last
    /// [`AudioDeviceFault`] the session reported. Empty if none.
    pub last_fault: String,
}

impl Default for AudioDeviceState {
    fn default() -> Self {
        Self {
            output_devices: Vec::new(),
            default_output: None,
            current_device: String::new(),
            is_running: false,
            channels: ChannelLayout::STEREO,
            stream_faults: 0,
            last_fault: String::new(),
        }
    }
}

/// Refreshes the per-frame half of [`AudioDeviceState`]: run state and
/// channel layout. The fault half is [`device_fault_poll`]'s.
///
/// Device *names* are not refreshed here — enumeration is an OS call, and doing
/// it every frame to learn something that changes on hot-plug would be paying a
/// syscall for a constant. [`device_state_init_system`] does it once.
///
/// A no-op when no driver is present, which is the ordinary headless case.
pub fn device_state_sync_system(
    driver: Option<NonSend<Device>>,
    config: Option<Res<AudioConfig>>,
    mut state: ResMut<AudioDeviceState>,
) {
    let Some(driver) = driver else { return };
    state.is_running = driver.is_running();
    if let Some(cfg) = config {
        state.channels = cfg.channels;
    }
}

/// The audio backend reported a fault (a device unplugged mid-session): the
/// most recent one since the last frame, as the session's `Poll` phase
/// reports it (`tutti_session::SessionEvent::DeviceFault`). A frame with
/// several faults reports the last; [`AudioDeviceState::stream_faults`]
/// counts them all. A host that takes the fault off the device
/// (`Device::take_fault`) before the phase runs consumes it: it is counted,
/// not reported.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct AudioDeviceFault(pub StreamFault);

/// What the session's device events say, for an app to read as messages
/// (design 018). Written only while devices are watched
/// ([`TuttiPlugin::watch_devices`](crate::TuttiPlugin::watch_devices)).
#[derive(Message, Debug, Clone, PartialEq)]
pub enum AudioDeviceEvent {
    /// A device is listed that was not.
    Appeared {
        /// Which way it plays or captures.
        direction: Direction,
        /// The device.
        id: DeviceId,
    },
    /// A device is no longer listed.
    Lost {
        /// Which way it played or captured.
        direction: Direction,
        /// The device.
        id: DeviceId,
    },
    /// The host's default moved; `None`: there is none now.
    DefaultChanged {
        /// Which direction's default.
        direction: Direction,
        /// The new default.
        id: Option<DeviceId>,
    },
    /// The device policy restarted the stream (or the app did). A mic opened
    /// at the old rate is to be reopened when
    /// [`DeviceRestarted::rate_changed`].
    Restarted(DeviceRestarted),
    /// A restart failed; the message is the error's.
    RestartFailed(String),
}

/// The message for one session device event, `None` for an event that is
/// not about devices.
pub(crate) fn translate(event: &SessionEvent) -> Option<AudioDeviceEvent> {
    Some(match event {
        SessionEvent::DeviceAppeared { direction, id } => AudioDeviceEvent::Appeared {
            direction: *direction,
            id: id.clone(),
        },
        SessionEvent::DeviceLost { direction, id } => AudioDeviceEvent::Lost {
            direction: *direction,
            id: id.clone(),
        },
        SessionEvent::DefaultChanged { direction, id } => AudioDeviceEvent::DefaultChanged {
            direction: *direction,
            id: id.clone(),
        },
        SessionEvent::DeviceRestarted(r) => AudioDeviceEvent::Restarted(*r),
        SessionEvent::RestartFailed(why) => AudioDeviceEvent::RestartFailed(why.clone()),
        _ => return None,
    })
}

/// Copies the watch's current list into `state`, for display.
pub(crate) fn copy_devices(state: &mut AudioDeviceState, devices: &tutti_cpal::Snapshot) {
    state.output_devices = devices.outputs.iter().map(ToString::to_string).collect();
    state.default_output = devices.default_output.as_ref().map(ToString::to_string);
}

/// The session's `Poll` phase
/// ([`SessionPhaseSystems`]`(Phase::Poll)`, last in the frame), as
/// `Session::update` runs it last: each device fault it reports is written
/// as an [`AudioDeviceFault`] and becomes
/// [`AudioDeviceState::last_fault`]. This system owns both fault fields, so
/// the resource never shows a count without its fault.
pub(crate) fn device_fault_poll(
    driver: Option<NonSend<Device>>,
    graph: Option<ResMut<AudioGraphRes>>,
    mut state: Option<ResMut<AudioDeviceState>>,
    mut faults: MessageWriter<AudioDeviceFault>,
    mut changes: MessageWriter<AudioDeviceEvent>,
    mut commands: Commands,
) {
    // `engine_ready` reads a state a host may insert without an engine.
    let Some(mut graph) = graph else { return };
    // The count before the phase reads it: never ahead of what the phase
    // reports (a fault landing in between is counted next frame).
    let count = driver.as_ref().map(|d| d.faults().count());
    let events = graph
        .bypass_change_detection()
        .run_phase(Phase::Poll, std::time::Instant::now());
    for event in events {
        if let Some(change) = translate(&event) {
            changes.write(change);
        }
        match event {
            SessionEvent::DeviceFault(fault) => {
                if let Some(state) = state.as_mut() {
                    state.last_fault.clone_from(&fault.message);
                }
                faults.write(AudioDeviceFault(fault));
            }
            // The one call the session offers; its outcome is the next
            // frame's `Restarted` or `RestartFailed`.
            SessionEvent::RestartWanted(request) => commands.queue(move |world: &mut World| {
                if let Err(e) = crate::engine::restart::restart_wanted(world, request) {
                    bevy_log::warn!("device restart failed: {e}");
                }
            }),
            SessionEvent::DeviceRestarted(_) => {
                if let (Some(state), Some(driver)) = (state.as_mut(), driver.as_ref())
                    && let Ok(name) = driver.device_name()
                {
                    state.current_device = name;
                }
            }
            _ => {}
        }
    }
    if let (Some(devices), Some(state)) = (graph.session().watched_devices(), state.as_mut()) {
        copy_devices(state, devices);
    }
    if let (Some(count), Some(state)) = (count, state.as_mut())
        && count != state.stream_faults
    {
        // A restart clears the device's record, and the fault with it.
        if count < state.stream_faults {
            state.last_fault.clear();
        }
        state.stream_faults = count;
    }
}

/// Present once the session watches devices: the list is then the events',
/// and startup does not enumerate.
#[derive(Resource, Debug)]
pub(crate) struct DevicesWatched;

/// A start that failed, watched until its device is listed (design 018):
/// non-send, as the poller's source is not `Sync`.
pub(crate) struct StartupRetry {
    watch: tutti_session::DeviceWatch,
    plugin: crate::TuttiPlugin,
    /// How the engine is built for a selector: the real device, unless a
    /// test stands in.
    build: Box<
        dyn FnMut(
            tutti_session::SessionBuilder,
            tutti_cpal::DeviceSelector,
        ) -> crate::engine::Result<(tutti_session::Session, Device)>,
    >,
}

/// Starts watching for the device a failed start wanted, when the plugin's
/// policy retries a startup.
pub(crate) fn retry_startup(plugin: &crate::TuttiPlugin, app: &mut App) {
    let Some(policy) = plugin.watch_devices.filter(|p| p.retry_startup) else {
        return;
    };
    match policy.spawn_poller(tutti_cpal::AudioHost::Default) {
        Ok(poller) => {
            app.insert_non_send(StartupRetry {
                watch: tutti_session::DeviceWatch::without_stream(
                    poller,
                    policy,
                    plugin.output_device.clone(),
                ),
                plugin: plugin.clone(),
                build: Box::new(|builder, device| builder.build_device(device)),
            });
        }
        Err(e) => bevy_log::warn!("the startup retry could not watch devices: {e}"),
    }
}

/// While the engine failed to start and a retry is watching: polls it, keeps
/// the device list, and builds the engine when the policy says the device is
/// listed. A build that fails again waits out the backoff. On success the
/// state is `Running` and the retry ends.
pub(crate) fn startup_retry_system(world: &mut World) {
    use crate::engine::build::{Watching, build_with, watch_with};
    let Some(mut retry) = world.remove_non_send::<StartupRetry>() else {
        return;
    };
    let mut events = Vec::new();
    let mut plugin = retry.plugin.clone();
    let build = &mut retry.build;
    let outcome = retry.watch.retry_start(&mut events, |device| {
        plugin.output_device = device.clone();
        build_with(&plugin, world, |b| build(b, device), Watching::Skip)
    });
    for event in &events {
        if let Some(change) = translate(event) {
            world.write_message(change);
        }
    }
    if let (Some(devices), Some(mut state)) = (
        retry.watch.snapshot().cloned(),
        world.get_resource_mut::<AudioDeviceState>(),
    ) {
        copy_devices(&mut state, &devices);
    }
    match outcome {
        Some(Ok(())) => {
            world.insert_resource(AudioEngineState::Running);
            // The poller goes on into the session's watch: one poller.
            let source = retry.watch.into_source();
            if let Some(device) = world.remove_non_send::<Device>() {
                world.resource_scope(|world, mut graph: Mut<AudioGraphRes>| {
                    watch_with(world, &plugin, graph.session(), &device, source);
                });
                if let (Ok(name), Some(mut state)) = (
                    device.device_name(),
                    world.get_resource_mut::<AudioDeviceState>(),
                ) {
                    state.current_device = name;
                }
                world.insert_non_send(device);
            }
        }
        Some(Err(e)) => {
            bevy_log::error!("retrying the audio engine failed: {e}");
            world.insert_resource(AudioEngineState::Failed(e.to_string()));
            world.insert_non_send(retry);
        }
        None => world.insert_non_send(retry),
    }
}

/// [`AudioDeviceState`], kept in sync, and the session's `Poll` phase
/// ([`device_fault_poll`]). Added by [`TuttiPlugin`](crate::TuttiPlugin);
/// needs [`GraphReconcilePlugin`](crate::graph::GraphReconcilePlugin) for
/// the phase's set.
pub(crate) struct DeviceStatePlugin;

impl Plugin for DeviceStatePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AudioDeviceState>()
            .add_message::<AudioDeviceFault>()
            .add_message::<AudioDeviceEvent>()
            .register_type::<AudioDeviceState>();
        app.add_systems(Startup, device_state_init_system);
        app.add_systems(
            Update,
            startup_retry_system.run_if(|state: Option<Res<AudioEngineState>>| {
                state.is_some_and(|s| s.failure().is_some())
            }),
        );
        app.add_systems(
            Update,
            (
                device_state_sync_system,
                device_fault_poll
                    .run_if(engine_ready)
                    .in_set(SessionPhaseSystems(Phase::Poll)),
            ),
        );
    }
}

/// Enumerates output devices once, at startup (unless devices are watched,
/// which fills the list from events), and records which one is open.
///
/// Both lookups are allowed to fail quietly: a driver that cannot name its
/// device or list the others leaves those fields at their defaults rather than
/// failing the startup schedule over a status display.
pub fn device_state_init_system(
    driver: Option<NonSend<Device>>,
    watched: Option<Res<DevicesWatched>>,
    mut state: ResMut<AudioDeviceState>,
) {
    let Some(driver) = driver else { return };

    if let Ok(name) = driver.device_name() {
        state.current_device = name;
    }
    if watched.is_none()
        && let Ok(devices) = Device::devices()
    {
        state.output_devices = devices.map(|d| d.id.to_string()).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::build::build_on;
    use crate::graph::GraphReconcilePlugin;
    use crate::{AudioEngineState, TuttiPlugin};
    use tutti_cpal::cpal::{SampleFormat, StreamError};
    use tutti_cpal::{ManualStreamDriver, OutputSpec, StreamFaultKind};
    use tutti_types::SampleRate;

    /// A fault the backend reports reaches a Bevy app as the headless
    /// session reports it: the session's `Poll` phase runs in the frame,
    /// and its `DeviceFault` is written as an [`AudioDeviceFault`] (once)
    /// and lands in [`AudioDeviceState`].
    ///
    /// Mutation: `device_fault_poll` left out of `DeviceStatePlugin`'s
    /// systems (no system runs `Poll`, as before): no message, and
    /// `last_fault` stays empty, so the test fails.
    #[test]
    fn a_device_fault_reaches_the_app_through_the_sessions_poll_phase() {
        let mut app = App::new();
        let (driver, stream) = ManualStreamDriver::new();
        build_on(&TuttiPlugin::default(), &mut app, |builder| {
            builder.build_device_on(
                OutputSpec::new(
                    SampleRate(48_000.0),
                    ChannelLayout::STEREO,
                    SampleFormat::F32,
                ),
                driver,
            )
        })
        .expect("builds with no device");
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, DeviceStatePlugin));

        let read = |app: &mut App| -> Vec<AudioDeviceFault> {
            app.world_mut()
                .resource_mut::<Messages<AudioDeviceFault>>()
                .drain()
                .collect()
        };

        app.update();
        assert!(read(&mut app).is_empty(), "no fault yet");

        stream.fail(StreamError::DeviceNotAvailable);
        app.update();
        let faults = read(&mut app);
        assert_eq!(faults.len(), 1, "one fault, reported once: {faults:?}");
        assert_eq!(faults[0].0.kind, StreamFaultKind::Disconnected);
        let state = app.world().resource::<AudioDeviceState>();
        assert_eq!(state.stream_faults, 1);
        assert_eq!(state.last_fault, faults[0].0.message);

        app.update();
        assert!(read(&mut app).is_empty(), "a fault is reported once");
    }

    /// A source the test feeds by hand.
    #[derive(Clone, Default)]
    struct Fake(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<tutti_cpal::Snapshot>>>);

    impl Fake {
        fn show(&self, outputs: &[&str], default: Option<&str>) {
            self.0
                .lock()
                .unwrap()
                .push_back(tutti_cpal::Snapshot::from_names(
                    outputs.iter().copied(),
                    [],
                    default,
                    None,
                ));
        }
    }

    impl tutti_cpal::SnapshotSource for Fake {
        fn poll(&mut self) -> Option<tutti_cpal::Snapshot> {
            self.0.lock().unwrap().pop_front()
        }
    }

    fn spec() -> OutputSpec {
        OutputSpec::new(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            SampleFormat::F32,
        )
    }

    fn events(app: &mut App) -> Vec<AudioDeviceEvent> {
        app.world_mut()
            .resource_mut::<Messages<AudioDeviceEvent>>()
            .drain()
            .collect()
    }

    fn running_app() -> App {
        let mut app = App::new();
        let (driver, _stream) = ManualStreamDriver::new();
        build_on(&TuttiPlugin::default(), &mut app, |b| {
            b.build_device_on(spec(), driver)
        })
        .expect("builds with no device");
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, DeviceStatePlugin));
        app
    }

    /// The device list follows the session's events instead of being
    /// enumerated once, and each change is a message.
    ///
    /// Mutation: `translate` not editing `output_devices` → the list stays
    /// empty and this fails; the `Poll` system not writing the message →
    /// the message assertions fail.
    #[test]
    fn the_device_list_follows_the_sessions_events() {
        let mut app = running_app();
        app.insert_resource(DevicesWatched);
        let fake = Fake::default();
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .session()
            .watch_devices(
                fake.clone(),
                tutti_session::DevicePolicy::default(),
                tutti_cpal::DeviceSelector::Default,
                None,
            );
        fake.show(&["A", "USB", "USB"], Some("A"));
        app.update();
        let state = app.world().resource::<AudioDeviceState>();
        assert_eq!(state.output_devices, ["A", "USB", "USB #1"]);
        assert_eq!(state.default_output.as_deref(), Some("A"));
        assert_eq!(events(&mut app).len(), 4);

        fake.show(&["A", "USB"], Some("USB"));
        app.update();
        let state = app.world().resource::<AudioDeviceState>();
        assert_eq!(state.output_devices, ["A", "USB"]);
        assert_eq!(state.default_output.as_deref(), Some("USB"));
        assert!(
            events(&mut app)
                .iter()
                .any(|e| matches!(e, AudioDeviceEvent::Lost { .. }))
        );
    }

    /// A restart the policy wants is run by the adapter's one call, and its
    /// failure comes back as a message, once; the policy then waits out its
    /// backoff (this device-free engine has no card to reopen, so the
    /// restart fails after stopping the stream).
    ///
    /// Mutation: the `RestartWanted` arm left out of `device_fault_poll` →
    /// no `RestartFailed` and this fails.
    #[test]
    fn a_wanted_restart_is_run_and_its_failure_reported_once() {
        let mut app = running_app();
        let fake = Fake::default();
        let policy = tutti_session::DevicePolicy {
            follow_default: true,
            ..Default::default()
        };
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .session()
            .watch_devices(
                fake.clone(),
                policy,
                tutti_cpal::DeviceSelector::Default,
                Some(DeviceId::new("A")),
            );
        fake.show(&["A", "B"], Some("A"));
        app.update();
        events(&mut app);
        fake.show(&["A", "B"], Some("B"));
        for _ in 0..3 {
            app.update();
        }
        let failed = events(&mut app)
            .into_iter()
            .filter(|e| matches!(e, AudioDeviceEvent::RestartFailed(_)))
            .count();
        assert_eq!(failed, 1, "tried once, then held by the backoff");
    }

    /// A start that failed is retried when a device appears, if the policy
    /// asks: the state leaves `Failed` and the engine exists.
    ///
    /// Mutation: `retry_startup` false in the policy → the state stays
    /// `Failed` and this fails.
    #[test]
    fn a_failed_start_is_retried_when_a_device_appears() {
        let mut app = App::new();
        app.insert_resource(AudioEngineState::Failed("no card".into()));
        app.add_plugins((GraphReconcilePlugin, DeviceStatePlugin));
        let fake = Fake::default();
        let policy = tutti_session::DevicePolicy {
            retry_startup: true,
            ..Default::default()
        };
        let (driver, _stream) = ManualStreamDriver::new();
        let mut driver = Some(driver);
        app.insert_non_send(StartupRetry {
            watch: tutti_session::DeviceWatch::without_stream(
                fake.clone(),
                policy,
                tutti_cpal::DeviceSelector::Default,
            ),
            plugin: TuttiPlugin::default(),
            build: Box::new(move |b, _| b.build_device_on(spec(), driver.take().unwrap())),
        });
        fake.show(&[], None);
        app.update();
        assert!(
            app.world()
                .resource::<AudioEngineState>()
                .failure()
                .is_some(),
            "no card yet"
        );
        fake.show(&["A"], Some("A"));
        app.update();
        assert!(app.world().resource::<AudioEngineState>().is_running());
        assert!(app.world().contains_resource::<AudioGraphRes>());
        assert_eq!(
            app.world().resource::<AudioDeviceState>().output_devices,
            ["A"]
        );
    }
}
