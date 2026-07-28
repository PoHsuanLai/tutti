//! Hardware MIDI device connect / disconnect + hot-plug detection.
//!
//! Only compiled under `midi-hardware`. [`ConnectMidiDevice`] /
//! [`DisconnectMidiDevice`] are fire-and-forget request messages;
//! [`midi_device_connect_system`] services them against the OS port manager,
//! while [`midi_device_poll_system`] diffs the engine's live device list and
//! emits [`MidiDeviceEvent`]s for devices that appeared or vanished — by request
//! or by hot-plug, indistinguishably, because the engine's list is the one
//! source either way.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader, MessageWriter};
use bevy_ecs::prelude::*;
use bevy_log::warn;

/// Hardware MIDI I/O (OS port management + virtual ports). Only present when
/// the `midi-hardware` feature is compiled; claimed into the world by
/// [`TuttiMidiPlugin`](super::plugin::TuttiMidiPlugin) from the engine handoff.
#[derive(Resource, Clone, Debug)]
pub struct MidiIoRes(pub tutti_midi_io::MidiIo);

impl std::ops::Deref for MidiIoRes {
    type Target = tutti_midi_io::MidiIo;
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

/// What the last poll saw, so a change can be spotted.
///
/// `seen` is **not** a record of what is connected — `MidiIo` already keeps
/// that, in an `ArcSwap` it updates itself, and a second copy here could only
/// disagree with it. It is the previous frame's snapshot, kept solely to diff
/// against the current one and turn "the set changed" into per-device messages.
///
/// It used to be that second copy: the connect system inserted into it by hand
/// and the 2-second poll existed partly to repair the divergence.
#[derive(Resource, Default, Debug)]
pub struct MidiDeviceState {
    pub(crate) seen: std::collections::HashSet<String>,
    pub(crate) last_check: Option<std::time::Instant>,
}

/// Service connect/disconnect requests against the OS port manager.
///
/// Emits no [`MidiDeviceEvent`] itself — [`midi_device_poll_system`] observes
/// what actually changed and reports it. A request that the driver silently
/// declines therefore produces no event, which is the honest outcome; announcing
/// a connection the engine does not have was the previous behaviour.
pub fn midi_device_connect_system(
    midi_io: Option<Res<super::device::MidiIoRes>>,
    mut connect_events: MessageReader<ConnectMidiDevice>,
    mut disconnect_events: MessageReader<DisconnectMidiDevice>,
    mut state: ResMut<MidiDeviceState>,
) {
    let Some(midi_io) = midi_io else {
        connect_events.clear();
        disconnect_events.clear();
        return;
    };

    let mut acted = false;
    for connect in connect_events.read() {
        match midi_io.0.connect_input_by_name(&connect.name) {
            Ok(()) => acted = true,
            Err(e) => warn!("Failed to connect MIDI device '{}': {}", connect.name, e),
        }
    }
    for disconnect in disconnect_events.read() {
        midi_io.0.disconnect_input(&disconnect.name);
        acted = true;
    }

    // Report the result now rather than up to two seconds from now.
    if acted {
        state.last_check = None;
    }
}

/// Emits a [`MidiDeviceEvent`] for every device that appeared or vanished since
/// the last look, whether through a request here or a hot-plug elsewhere.
///
/// Polls at most every 2s, except immediately after a connect/disconnect was
/// serviced.
pub fn midi_device_poll_system(
    midi_io: Option<Res<super::device::MidiIoRes>>,
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

    // The engine's own list is the truth; `seen` is only what we reported last.
    let live: std::collections::HashSet<String> =
        midi_io.0.connected_input_names().into_iter().collect();

    for name in state.seen.difference(&live).cloned() {
        device_events.write(MidiDeviceEvent::Disconnected { name });
    }
    for name in live.difference(&state.seen).cloned() {
        device_events.write(MidiDeviceEvent::Connected { name });
    }
    state.seen = live;
}

/// Hardware device connect/disconnect servicing + hot-plug polling.
pub struct MidiDevicePlugin;

impl Plugin for MidiDevicePlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<MidiDeviceEvent>();
        app.add_message::<ConnectMidiDevice>();
        app.add_message::<DisconnectMidiDevice>();
        app.init_resource::<MidiDeviceState>();
        // Chained so a serviced request is reported on the same frame: the
        // connect system clears the poll timer, and the poll system reads it.
        app.add_systems(
            Update,
            (midi_device_connect_system, midi_device_poll_system)
                .chain()
                .run_if(crate::graph::engine_ready),
        );
    }
}
