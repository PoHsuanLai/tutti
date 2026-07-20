//! Hardware MIDI input → ECS event bridge.
//!
//! A crossbeam channel funnels [`MidiInputRecord`](crate::MidiInputRecord)s
//! from the hardware port observer into the ECS world; the per-frame
//! [`midi_input_event_system`] drains it into [`MidiInputEvent`] messages that
//! any consumer can read. Without `midi-hardware` there's no port to observe,
//! so the setup is a no-op and events only arrive via
//! [`MidiInputEvent::synthetic`].

use bevy_app::{App, Plugin, Startup, Update};
use bevy_ecs::message::MessageWriter;
use bevy_ecs::prelude::*;

use crate::{normalize, MidiEvent, MidiInputRecord};

/// Fired every frame for each MIDI event received from hardware input.
///
/// Tagged with the originating device — consumers that don't care
/// (live-input synth routing) can ignore `device_id` / `device_name`,
/// while those that do (external clock chase, MIDI-learn-per-device)
/// filter on them.
#[derive(Event, Message, Clone, Debug)]
pub struct MidiInputEvent {
    pub event: MidiEvent,
    pub device_id: u32,
    pub device_name: String,
    /// Microseconds since the originating connection opened
    /// (midir-provided). Monotonic per device.
    pub timestamp_us: u64,
}

impl From<MidiInputRecord> for MidiInputEvent {
    fn from(r: MidiInputRecord) -> Self {
        Self {
            event: r.event,
            device_id: r.device_id,
            device_name: r.device_name,
            timestamp_us: r.timestamp_us,
        }
    }
}

impl MidiInputEvent {
    /// Constructor for tests / synthetic events not originating from
    /// hardware. Real hardware events arrive via `From<MidiInputRecord>`.
    pub fn synthetic(event: MidiEvent) -> Self {
        Self {
            event,
            device_id: 0,
            device_name: String::new(),
            timestamp_us: 0,
        }
    }

    #[inline]
    pub fn is_note_on(&self) -> bool {
        self.event.is_note_on()
    }

    #[inline]
    pub fn is_note_off(&self) -> bool {
        self.event.is_note_off()
    }

    #[inline]
    pub fn note(&self) -> Option<u8> {
        self.event.note()
    }

    /// Velocity as a 7-bit MIDI 1 value, downconverted from the internal
    /// 16-bit MIDI 2 form if the event is MIDI 2.
    #[inline]
    pub fn velocity(&self) -> Option<u8> {
        self.event.velocity_u7()
    }

    #[inline]
    pub fn event(&self) -> &MidiEvent {
        &self.event
    }

    /// Normalise to a MIDI 2.0 Channel Voice [`MidiEvent`]: velocity-0 NoteOn
    /// folds to NoteOff and inbound MIDI 1.0 channel voice is promoted to Channel
    /// Voice 2, so consumers match one vocabulary. Decode the result with
    /// `midi2::UmpMessage::try_from(ev.data_words())`.
    #[inline]
    pub fn normalized(&self) -> MidiEvent {
        normalize(&self.event)
    }
}

/// Bound on the hardware-input observer channel. This is a best-effort UI
/// notification tap (the audible path is the lock-free port ring, separate),
/// so the channel is *bounded*: if a frame stall stops the drain while a fast
/// controller spews events, the midir callback's `try_send` drops the overflow
/// rather than growing the channel without limit. Sized for several frames of
/// dense CC traffic.
const OBSERVER_CHANNEL_CAPACITY: usize = 1024;

/// Receiving end of the hardware-input observer channel; drained each frame by
/// [`midi_input_event_system`].
#[derive(Resource)]
pub struct MidiInputObserver {
    pub(crate) receiver: crossbeam_channel::Receiver<MidiInputRecord>,
}

/// Sending end, handed to the hardware port at [`Startup`]; taken once by
/// [`midi_observer_setup_system`].
#[derive(Resource)]
pub(crate) struct MidiObserverSender {
    pub(crate) sender: Option<crossbeam_channel::Sender<MidiInputRecord>>,
}

/// Sets up the UI observer on the hardware MIDI input port, funneling events
/// into [`MidiInputObserver`]'s channel. No-op when `midi-hardware` is disabled
/// (there's no hardware port to observe).
pub(crate) fn midi_observer_setup_system(
    #[cfg(feature = "midi-hardware")] midi_io: Option<Res<crate::MidiIoRes>>,
    mut sender_res: ResMut<MidiObserverSender>,
) {
    let Some(sender) = sender_res.sender.take() else {
        return;
    };

    #[cfg(feature = "midi-hardware")]
    {
        let Some(midi_io) = midi_io else { return };
        midi_io.0.set_input_observer(sender);
    }

    #[cfg(not(feature = "midi-hardware"))]
    {
        let _ = sender;
    }
}

pub fn midi_input_event_system(
    observer: Option<Res<MidiInputObserver>>,
    mut writer: MessageWriter<MidiInputEvent>,
) {
    let Some(observer) = observer else { return };
    while let Ok(record) = observer.receiver.try_recv() {
        writer.write(MidiInputEvent::from(record));
    }
}

/// Hardware MIDI input observation: the observer channel + per-frame event pump.
pub struct MidiInputPlugin;

impl Plugin for MidiInputPlugin {
    fn build(&self, app: &mut App) {
        let (sender, receiver) = crossbeam_channel::bounded(OBSERVER_CHANNEL_CAPACITY);
        app.insert_resource(MidiInputObserver { receiver });
        app.insert_resource(MidiObserverSender {
            sender: Some(sender),
        });

        app.add_message::<MidiInputEvent>();
        app.add_systems(Startup, midi_observer_setup_system);
        app.add_systems(
            Update,
            midi_input_event_system.run_if(tutti_core::graph::engine_ready),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
    use tutti_midi_types::midi2::{Channeled, UmpMessage};

    #[test]
    fn normalized_decodes_cc() {
        let ev = MidiInputEvent::synthetic(MidiEvent::cc(0, 3, 7, u32::MAX / 2));
        let norm = ev.normalized();
        match UmpMessage::try_from(norm.data_words()).expect("UMP") {
            UmpMessage::ChannelVoice2(Cv2::ControlChange(m)) => {
                assert_eq!(u8::from(m.channel()), 3);
                assert_eq!(u8::from(m.control()), 7);
                assert_eq!(m.control_change_data(), u32::MAX / 2);
            }
            other => panic!("expected CV2 ControlChange, got {other:?}"),
        }
    }

    #[test]
    fn normalized_decodes_pitch_bend() {
        let ev = MidiInputEvent::synthetic(MidiEvent::pitch_bend(0, 0, u32::MAX));
        let norm = ev.normalized();
        match UmpMessage::try_from(norm.data_words()).expect("UMP") {
            UmpMessage::ChannelVoice2(Cv2::ChannelPitchBend(m)) => {
                assert_eq!(m.pitch_bend_data(), u32::MAX);
            }
            other => panic!("expected CV2 ChannelPitchBend, got {other:?}"),
        }
    }

    #[test]
    fn normalized_decodes_note_on() {
        let ev = MidiInputEvent::synthetic(MidiEvent::note_on(0, 0, 60, 0x8000));
        let norm = ev.normalized();
        match UmpMessage::try_from(norm.data_words()).expect("UMP") {
            UmpMessage::ChannelVoice2(Cv2::NoteOn(m)) => {
                assert_eq!(u8::from(m.note_number()), 60);
                assert_eq!(m.velocity(), 0x8000);
            }
            other => panic!("expected CV2 NoteOn, got {other:?}"),
        }
    }
}
