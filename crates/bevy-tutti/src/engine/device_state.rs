//! A `Reflect`-able mirror of what the audio device is doing, for a UI to read.
//!
//! Everything here is derived from the [`Device`] and [`AudioConfig`]; nothing
//! writes back. The device is `NonSend` and neither it nor `AudioConfig` is
//! `Reflect`, so a status panel that wants an inspector-visible resource reads
//! this instead of the device layer directly.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;
use tutti_session::{Phase, SessionEvent};

use crate::graph::{engine_ready, AudioConfig, AudioGraphRes, SessionPhaseSystems};
use crate::Device;
use tutti_core::ChannelLayout;
use tutti_cpal::StreamFault;

/// Audio device state, mirrored from the driver each frame.
///
/// Output-only: this is the read side, and writing a field changes nothing about
/// the device — it is overwritten by the next sync.
#[derive(Resource, Debug, Clone, Reflect)]
#[reflect(Resource, Default, Clone)]
pub struct AudioDeviceState {
    /// Every output device the host OS offered at startup, by name.
    ///
    /// Enumerated once at startup and not refreshed, so a device plugged in
    /// later is absent until enumeration runs again.
    pub output_devices: Vec<String>,
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
) {
    // `engine_ready` reads a state a host may insert without an engine.
    let Some(mut graph) = graph else { return };
    // The count before the phase reads it: never ahead of what the phase
    // reports (a fault landing in between is counted next frame).
    let count = driver.map(|d| d.faults().count());
    let events = graph
        .bypass_change_detection()
        .run_phase(Phase::Poll, std::time::Instant::now());
    for event in events {
        if let SessionEvent::DeviceFault(fault) = event {
            if let Some(state) = state.as_mut() {
                state.last_fault.clone_from(&fault.message);
            }
            faults.write(AudioDeviceFault(fault));
        }
    }
    if let (Some(count), Some(state)) = (count, state.as_mut()) {
        if count != state.stream_faults {
            // A restart clears the device's record, and the fault with it.
            if count < state.stream_faults {
                state.last_fault.clear();
            }
            state.stream_faults = count;
        }
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
            .register_type::<AudioDeviceState>();
        app.add_systems(Startup, device_state_init_system);
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

/// Enumerates output devices once, at startup, and records which one is open.
///
/// Both lookups are allowed to fail quietly: a driver that cannot name its
/// device or list the others leaves those fields at their defaults rather than
/// failing the startup schedule over a status display.
pub fn device_state_init_system(
    driver: Option<NonSend<Device>>,
    mut state: ResMut<AudioDeviceState>,
) {
    let Some(driver) = driver else { return };

    if let Ok(name) = driver.device_name() {
        state.current_device = name;
    }
    if let Ok(devices) = Device::devices() {
        state.output_devices = devices.map(|d| d.name).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::build::build_on;
    use crate::graph::GraphReconcilePlugin;
    use crate::{AudioEngineState, TuttiPlugin};
    use tutti_core::SampleRate;
    use tutti_cpal::cpal::{SampleFormat, StreamError};
    use tutti_cpal::{ManualStreamDriver, OutputSpec, StreamFaultKind};

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
}
