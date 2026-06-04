//! Bevy ECS integration: MIDI input observation, sequence playback,
//! hardware I/O, MPE, and time-delayed dispatch.
//!
//! Sub-modules use the role-axis split (components / events / systems)
//! because the duty is small + event-heavy.

use bevy_app::{App, Plugin, Startup, Update};
use bevy_ecs::prelude::*;


pub mod components;
pub mod events;
pub mod scheduled;
pub mod systems;

pub use components::{MidiReceiver, MidiSequence, MidiSequenceNote};
pub use events::MidiInputEvent;
pub use scheduled::{tick_scheduled_midi, MidiSynthMarker, ScheduledMidi};
pub use systems::{
    midi_input_event_system, midi_routing_sync_system, midi_sequence_setup_system,
    midi_sequence_tick_system, MidiInputObserver, MidiSequenceState,
};

#[cfg(feature = "midi-hardware")]
pub use components::{ConnectMidiDevice, DisconnectMidiDevice};
#[cfg(feature = "midi-hardware")]
pub use events::MidiDeviceEvent;
#[cfg(feature = "midi-hardware")]
pub use systems::{midi_device_connect_system, midi_device_poll_system, MidiDeviceState};

#[cfg(feature = "mpe")]
pub use components::MpeReceiver;
#[cfg(feature = "mpe")]
pub use systems::{MpeExpressionResource, MpeModeConfig};

/// MIDI fan-out bus — audio-thread event dispatch to per-unit inboxes.
///
/// Wraps a [`tutti_midi_runtime::MidiBus`] (the runtime fan-out bus is
/// owned by tutti-midi-runtime; this newtype only adds the Bevy
/// `Resource` derive).
#[derive(Resource, Clone)]
pub struct MidiBusRes(pub tutti_midi_runtime::MidiBus);

impl std::ops::Deref for MidiBusRes {
    type Target = tutti_midi_runtime::MidiBus;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Hardware MIDI I/O (OS port management + virtual ports). Only present
/// when `.midi()` was called on the builder.
#[cfg(feature = "midi-hardware")]
#[derive(Resource, Clone)]
pub struct MidiIoRes(pub crate::MidiIo);

#[cfg(feature = "midi-hardware")]
impl std::ops::Deref for MidiIoRes {
    type Target = crate::MidiIo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Transient handoff: the built MIDI handles. `build_into` inserts this; the
/// MIDI plugin's `build()` claims it (see [`TuttiMidiPlugin`]).
#[derive(Resource)]
pub struct PendingMidi {
    pub bus: Option<tutti_midi_runtime::MidiBus>,
    #[cfg(feature = "midi-hardware")]
    pub io: Option<crate::MidiIo>,
}

/// Bevy plugin: MIDI input + sequence playback + hardware I/O + time-delayed
/// dispatch.
pub struct TuttiMidiPlugin;

impl Plugin for TuttiMidiPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<components::MidiSequenceNote>();

        let (sender, receiver) = crossbeam_channel::unbounded();
        app.insert_resource(systems::MidiInputObserver { receiver });
        app.insert_resource(systems::MidiObserverSender {
            sender: Some(sender),
        });

        app.add_message::<events::MidiInputEvent>();

        app.add_systems(Startup, systems::midi_observer_setup_system);

        // Claim our handles out of the transient `build_into` inserted
        // (synchronous, during plugin build — present before frame 1).
        if let Some(mut pending) = app.world_mut().remove_resource::<PendingMidi>() {
            if let Some(bus) = pending.bus.take() {
                app.insert_resource(MidiBusRes(bus));
            }
            #[cfg(feature = "midi-hardware")]
            if let Some(io) = pending.io.take() {
                app.insert_resource(MidiIoRes(io));
            }
        }

        // `midi_routing_sync_system` stages route-table edits + sets
        // GraphDirty (instead of committing inline), so anchor the chain
        // before the Commit phase where `commit_graph` flushes it.
        app.add_systems(
            Update,
            (
                systems::midi_input_event_system,
                systems::midi_routing_sync_system,
                systems::midi_sequence_setup_system,
                systems::midi_sequence_tick_system,
            )
                .chain()
                .run_if(tutti_core::graph::engine_ready)
                .before(tutti_core::graph::GraphReconcileSystems::Commit),
        );

        // Time-delayed MIDI dispatch (relocated from bevy-tutti's
        // `GraphReconcilePlugin`, which used to schedule it inline).
        app.add_systems(
            Update,
            scheduled::tick_scheduled_midi.run_if(tutti_core::graph::engine_ready),
        );

        #[cfg(feature = "mpe")]
        app.add_systems(Startup, systems::mpe_setup_system);

        #[cfg(feature = "midi-hardware")]
        {
            app.add_message::<events::MidiDeviceEvent>();
            app.add_message::<components::ConnectMidiDevice>();
            app.add_message::<components::DisconnectMidiDevice>();
            app.init_resource::<systems::MidiDeviceState>();
            app.add_systems(
                Update,
                (
                    systems::midi_device_connect_system,
                    systems::midi_device_poll_system,
                ),
            );
        }
    }
}

