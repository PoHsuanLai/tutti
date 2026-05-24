//! MIDI: input observation, sequence playback, hardware I/O, MPE.
//!
//! Sub-modules use the role-axis split (components / events / systems)
//! because the duty is small + event-heavy.

use bevy_app::{App, Plugin, Startup, Update};
use bevy_ecs::prelude::*;

pub mod components;
pub mod events;
pub mod systems;

/// Bevy plugin: MIDI input + sequence playback + hardware I/O.
pub struct TuttiMidiPlugin;

impl Plugin for TuttiMidiPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<components::MidiSequenceNote>();

        #[cfg(feature = "midi-hardware")]
        app.register_type::<components::ConnectMidiDevice>()
            .register_type::<components::DisconnectMidiDevice>();

        let (sender, receiver) = crossbeam_channel::unbounded();
        app.insert_resource(systems::MidiInputObserver { receiver });
        app.insert_resource(systems::MidiObserverSender {
            sender: Some(sender),
        });

        app.add_message::<events::MidiInputEvent>();

        app.add_systems(Startup, systems::midi_observer_setup_system);

        app.add_systems(
            Update,
            (
                systems::midi_input_event_system,
                systems::midi_routing_sync_system,
                systems::midi_sequence_setup_system,
                systems::midi_sequence_tick_system,
            )
                .chain(),
        );

        #[cfg(feature = "mpe")]
        app.add_systems(Startup, systems::mpe_setup_system);

        #[cfg(feature = "midi-hardware")]
        {
            app.add_message::<events::MidiDeviceEvent>();
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
