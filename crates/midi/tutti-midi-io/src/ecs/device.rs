//! Hardware MIDI device connect / disconnect + hot-plug detection.
//!
//! Only compiled under `midi-hardware`. [`ConnectMidiDevice`] /
//! [`DisconnectMidiDevice`] are fire-and-forget request messages;
//! [`midi_device_connect_system`] services them against the OS port manager,
//! while [`midi_device_poll_system`] reconciles the live device list every 2s
//! and emits [`MidiDeviceEvent`]s for devices that appear or vanish.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader, MessageWriter};
use bevy_ecs::prelude::*;
use bevy_log::warn;

/// Hardware MIDI I/O (OS port management + virtual ports). Only present when
/// the `midi-hardware` feature is compiled; claimed into the world by
/// [`TuttiMidiPlugin`](crate::TuttiMidiPlugin) from the engine handoff.
#[derive(Resource, Clone)]
pub struct MidiIoRes(pub crate::MidiIo);

impl std::ops::Deref for MidiIoRes {
    type Target = crate::MidiIo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Fire-and-forget request: connect to a MIDI input device by name (partial match).
#[derive(Message, Debug, Clone)]
pub struct ConnectMidiDevice {
    pub name: String,
}

/// Fire-and-forget request: disconnect a specific MIDI input device by name.
#[derive(Message, Debug, Clone)]
pub struct DisconnectMidiDevice {
    pub name: String,
}

#[derive(Event, Message, Clone, Debug)]
pub enum MidiDeviceEvent {
    Connected { name: String },
    Disconnected { name: String },
}

#[derive(Resource, Default)]
pub struct MidiDeviceState {
    pub(crate) connected: std::collections::HashSet<String>,
    pub(crate) last_check: Option<std::time::Instant>,
}

pub fn midi_device_connect_system(
    midi_io: Option<Res<crate::MidiIoRes>>,
    mut connect_events: MessageReader<ConnectMidiDevice>,
    mut disconnect_events: MessageReader<DisconnectMidiDevice>,
    mut device_events: MessageWriter<MidiDeviceEvent>,
    mut state: ResMut<MidiDeviceState>,
) {
    let Some(midi_io) = midi_io else { return };

    for connect in connect_events.read() {
        match midi_io.0.connect_input_by_name(&connect.name) {
            Ok(()) => {
                if state.connected.insert(connect.name.clone()) {
                    device_events.write(MidiDeviceEvent::Connected {
                        name: connect.name.clone(),
                    });
                }
            }
            Err(e) => {
                warn!("Failed to connect MIDI device '{}': {}", connect.name, e);
            }
        }
    }

    for disconnect in disconnect_events.read() {
        midi_io.0.disconnect_input(&disconnect.name);
        if state.connected.remove(&disconnect.name) {
            device_events.write(MidiDeviceEvent::Disconnected {
                name: disconnect.name.clone(),
            });
        }
    }
}

/// Polls every 2s; emits `MidiDeviceEvent::Disconnected` for each device that
/// vanished and `Connected` for any new device that appeared (e.g., a hot-plug
/// or an external connection through another part of the app).
pub fn midi_device_poll_system(
    midi_io: Option<Res<crate::MidiIoRes>>,
    mut state: ResMut<MidiDeviceState>,
    mut device_events: MessageWriter<MidiDeviceEvent>,
) {
    let Some(midi_io) = midi_io else { return };
    let now = std::time::Instant::now();

    if let Some(last) = state.last_check {
        if now.duration_since(last).as_secs() < 2 {
            return;
        }
    }
    state.last_check = Some(now);

    let live: std::collections::HashSet<String> =
        midi_io.0.connected_input_names().into_iter().collect();

    for name in state.connected.difference(&live).cloned().collect::<Vec<_>>() {
        state.connected.remove(&name);
        device_events.write(MidiDeviceEvent::Disconnected { name });
    }
    for name in live.difference(&state.connected).cloned().collect::<Vec<_>>() {
        state.connected.insert(name.clone());
        device_events.write(MidiDeviceEvent::Connected { name });
    }
}

/// Hardware device connect/disconnect servicing + hot-plug polling.
pub struct MidiDevicePlugin;

impl Plugin for MidiDevicePlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<MidiDeviceEvent>();
        app.add_message::<ConnectMidiDevice>();
        app.add_message::<DisconnectMidiDevice>();
        app.init_resource::<MidiDeviceState>();
        app.add_systems(
            Update,
            (midi_device_connect_system, midi_device_poll_system)
                .run_if(tutti_core::graph::engine_ready),
        );
    }
}
