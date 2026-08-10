//! Hardware MIDI device connect / disconnect + hot-plug detection.
//!
//! Only compiled under `midi-hardware`. [`ConnectMidiDevice`] /
//! [`DisconnectMidiDevice`] are fire-and-forget request messages;
//! [`midi_device_connect_system`] services them against the OS port manager,
//! while [`midi_device_poll_system`] diffs the engine's live device list and
//! emits [`MidiDeviceEvent`]s for devices that appeared or vanished — by request
//! or by hot-plug, indistinguishably, because the engine's list is the one
//! source either way.
//!
//! **Both directions are covered, and the output half is what makes the
//! outbound path reachable at all.** Without a connected output,
//! [`hardware_out`](super::hardware_out), [`track_out`](super::track_out) and
//! [`clock_out`](super::clock_out) drain their mailboxes into `MidiIo::send`,
//! which pushes into a channel with no connected port and logs at `debug!` —
//! events leave the ring and reach nothing.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader, MessageWriter};
use bevy_ecs::prelude::*;
use bevy_log::warn;

/// The open MIDI connections and the endpoints available to open. Only present
/// when the `midi-hardware` feature is compiled; claimed into the world by
/// [`TuttiMidiPlugin`](crate::midi::plugin::TuttiMidiPlugin) from the engine handoff.
#[derive(Resource, Clone, Debug)]
pub struct MidiIoRes(
    /// The OS session. Shares its state behind an `Arc`, so this clone and the
    /// one the RT port manager holds are the same set of open ports.
    pub tutti_midi_hardware::MidiSession,
);

impl std::ops::Deref for MidiIoRes {
    type Target = tutti_midi_hardware::MidiSession;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Fire-and-forget request: connect to a MIDI input device by name (partial match).
#[derive(Message, Debug, Clone)]
pub struct ConnectMidiDevice {
    /// Matched case-insensitively as a substring of the OS device name; the
    /// first endpoint that contains it wins.
    pub name: String,
}

/// Fire-and-forget request: disconnect a specific MIDI input device by name.
///
/// Named, unlike [`DisconnectMidiOutput`], because inputs are many-at-once.
#[derive(Message, Debug, Clone)]
pub struct DisconnectMidiDevice {
    /// Matched the same way [`ConnectMidiDevice::name`] is.
    pub name: String,
}

/// Fire-and-forget request: connect a MIDI **output** device by name (partial
/// match). Replaces any currently connected output.
///
/// # Why not a direction field on [`ConnectMidiDevice`]
///
/// The engine's two sides are not symmetric, and folding them together would
/// have to hide that. Inputs are many-at-once and disconnect *by name*
/// (`MidiIo::disconnect_input(&name)`); there is at most one output, and
/// `MidiIo::disconnect_output()` takes no name at all. A shared
/// `DisconnectMidiDevice { name, direction }` would therefore have to accept a
/// name it silently ignores for one of the two — a new quiet failure in the
/// subsystem this layer exists to make loud.
#[derive(Message, Debug, Clone)]
pub struct ConnectMidiOutput {
    /// Matched the same way [`ConnectMidiDevice::name`] is.
    pub name: String,
}

/// Fire-and-forget request: disconnect the MIDI output device.
///
/// Carries no name: there is only ever one connected output, and the engine's
/// `disconnect_output` takes no argument.
#[derive(Message, Debug, Clone, Default)]
pub struct DisconnectMidiOutput;

/// Which half of the wire a device event is about.
///
/// A host almost always cares: a disappeared *input* means a dead controller, a
/// disappeared *output* means everything sent from now on is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MidiDirection {
    /// A device the app receives MIDI from.
    Input,
    /// The device the app sends MIDI to.
    Output,
}

/// A device appeared or vanished, by request or by hot-plug.
///
/// Emitted only by [`midi_device_poll_system`], which reports what the engine's
/// device list actually shows — so an event here means the change happened, not
/// that it was asked for.
#[derive(Event, Message, Clone, Debug)]
pub enum MidiDeviceEvent {
    /// A device is now open and carrying MIDI.
    Connected {
        /// The device's OS-reported name.
        name: String,
        /// Which half of the wire it is.
        direction: MidiDirection,
    },
    /// A device is gone. Anything sent to a departed output is dropped.
    Disconnected {
        /// The name the device was known by while connected.
        name: String,
        /// Which half of the wire it was.
        direction: MidiDirection,
    },
}

/// What the last poll saw, so a change can be spotted.
///
/// `seen` is **not** a record of what is connected — `MidiIo` already keeps
/// that, in an `ArcSwap` it updates itself, and a second copy here could only
/// disagree with it. It is the previous frame's snapshot, kept solely to diff
/// against the current one and turn "the set changed" into per-device messages.
/// Nothing but the poll writes it — a connect system inserting into it by hand
/// would make it that second copy, and the poll would then exist partly to
/// repair its own divergence.
#[derive(Resource, Default, Debug)]
pub struct MidiDeviceState {
    pub(crate) seen: std::collections::HashSet<String>,
    /// The output device the last poll saw, for the same reason as `seen`.
    ///
    /// A separate `Option` rather than an entry in the set above: there is at
    /// most one output, so a set would model a cardinality the engine does not
    /// have and would need a second lookup to answer "which one".
    pub(crate) seen_output: Option<String>,
    /// When the last poll ran. `None` forces the next one, which is how a
    /// serviced request gets reported on its own frame instead of up to two
    /// seconds later.
    pub(crate) last_check: Option<std::time::Instant>,
}

/// Service connect/disconnect requests against the OS port manager.
///
/// Emits no [`MidiDeviceEvent`] itself — [`midi_device_poll_system`] observes
/// what actually changed and reports it. A request the driver silently declines
/// therefore produces no event, which is the honest outcome — announcing a
/// connection the engine does not have would be worse than silence.
pub fn midi_device_connect_system(
    midi_io: Option<Res<super::device::MidiIoRes>>,
    mut connect_events: MessageReader<ConnectMidiDevice>,
    mut disconnect_events: MessageReader<DisconnectMidiDevice>,
    mut connect_out: MessageReader<ConnectMidiOutput>,
    mut disconnect_out: MessageReader<DisconnectMidiOutput>,
    mut state: ResMut<MidiDeviceState>,
) {
    let Some(midi_io) = midi_io else {
        connect_events.clear();
        disconnect_events.clear();
        connect_out.clear();
        disconnect_out.clear();
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
        midi_io.0.disconnect_input_by_name(&disconnect.name);
        acted = true;
    }
    for connect in connect_out.read() {
        match midi_io.0.connect_output_by_name(&connect.name) {
            Ok(()) => acted = true,
            Err(e) => warn!(
                "Failed to connect MIDI output device '{}': {}",
                connect.name, e
            ),
        }
    }
    // Read even when empty, so the reader does not accumulate.
    if disconnect_out.read().next().is_some() {
        midi_io.0.disconnect_output();
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

    // The engine's own list is the truth; `seen` is only what was reported last.
    let live: std::collections::HashSet<String> =
        midi_io.0.connected_input_names().into_iter().collect();

    for name in state.seen.difference(&live).cloned() {
        device_events.write(MidiDeviceEvent::Disconnected {
            name,
            direction: MidiDirection::Input,
        });
    }
    for name in live.difference(&state.seen).cloned() {
        device_events.write(MidiDeviceEvent::Connected {
            name,
            direction: MidiDirection::Input,
        });
    }
    state.seen = live;

    // The output is one device or none, so its diff is a compare rather than a
    // set difference. `is_output_connected` gates the name: the engine keeps the
    // last device name around, and reporting it while disconnected would
    // announce an output that cannot carry anything.
    let live_output = midi_io
        .0
        .is_output_connected()
        .then(|| midi_io.0.output_device_name())
        .flatten();
    if live_output != state.seen_output {
        if let Some(name) = state.seen_output.take() {
            device_events.write(MidiDeviceEvent::Disconnected {
                name,
                direction: MidiDirection::Output,
            });
        }
        if let Some(name) = live_output.clone() {
            device_events.write(MidiDeviceEvent::Connected {
                name,
                direction: MidiDirection::Output,
            });
        }
        state.seen_output = live_output;
    }
}

/// Hardware device connect/disconnect servicing + hot-plug polling.
pub struct MidiDevicePlugin;

impl Plugin for MidiDevicePlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<MidiDeviceEvent>();
        app.add_message::<ConnectMidiDevice>();
        app.add_message::<DisconnectMidiDevice>();
        app.add_message::<ConnectMidiOutput>();
        app.add_message::<DisconnectMidiOutput>();
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
