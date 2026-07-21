//! Hardware MIDI input → ECS event bridge.
//!
//! A crossbeam channel funnels [`MidiInputRecord`](crate::MidiInputRecord)s
//! from the hardware port observer into the ECS world; the per-frame
//! [`midi_input_event_system`] drains it into [`MidiInputEvent`] messages that
//! any consumer can read. Without `midi-hardware` there's no port to observe,
//! so the setup is a no-op and events only arrive via
//! [`MidiInputEvent::synthetic`].

use std::collections::HashMap;

use bevy_app::{App, Plugin, Startup, Update};
use bevy_ecs::message::MessageWriter;
use bevy_ecs::prelude::*;

use crate::{normalize, MidiEvent, MidiInputRecord};
use tutti_midi_types::Midi1ToMidi2Translator;

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
    ///
    /// This is *stateless*. Use [`MidiInputEvent::translated`] with a
    /// [`MidiInputTranslators`] when a source sends multi-message (N)RPN runs
    /// that must be reassembled into single MIDI-2 controller messages.
    #[inline]
    pub fn normalized(&self) -> MidiEvent {
        normalize(&self.event)
    }

    /// Translate to MIDI 2.0 through the per-device stateful translator, so an
    /// (N)RPN CC run collapses into one Registered/Assignable Controller. Returns
    /// `None` when this event is *absorbed* mid-run (a parameter-select or partial
    /// Data Entry); otherwise the promoted/translated [`MidiEvent`]. Non-(N)RPN
    /// events pass through exactly as [`normalized`](Self::normalized) would.
    #[inline]
    pub fn translated(&self, translators: &mut MidiInputTranslators) -> Option<MidiEvent> {
        translators.of(self.device_id).translate(&self.event)
    }
}

/// Per-device [`Midi1ToMidi2Translator`] state for the inbound hardware path.
///
/// (N)RPN reassembly is stateful and per-endpoint, so each device keeps its own
/// accumulator. Insert this as a resource and drive inbound events through
/// [`MidiInputEvent::translated`] instead of [`MidiInputEvent::normalized`] to
/// get spec-faithful MIDI-1→2 translation, including multi-CC (N)RPN runs.
#[derive(Resource, Default)]
pub struct MidiInputTranslators {
    per_device: HashMap<u32, Midi1ToMidi2Translator>,
}

impl MidiInputTranslators {
    /// The translator for `device_id`, created on first use.
    #[inline]
    pub fn of(&mut self, device_id: u32) -> &mut Midi1ToMidi2Translator {
        self.per_device.entry(device_id).or_default()
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
        app.init_resource::<MidiInputTranslators>();

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

    #[test]
    fn translated_reassembles_rpn_run_per_device() {
        // A MIDI-1 RPN run (CC101/100 select + CC6 data) must collapse into one
        // MIDI-2 Registered Controller when routed through the stateful translator,
        // and each device keeps independent state.
        let mut translators = MidiInputTranslators::default();
        let cc = |control: u8, value: u8| {
            let mut ev = MidiInputEvent::synthetic(
                MidiEvent::from_midi1_bytes(0, &[0xB0 | 3, control, value]).expect("CV1 CC"),
            );
            ev.device_id = 42;
            ev
        };

        // Select RPN 0x00/0x06 then Data Entry — the first two are absorbed.
        assert!(cc(101, 0x00).translated(&mut translators).is_none());
        assert!(cc(100, 0x06).translated(&mut translators).is_none());
        let out = cc(6, 10)
            .translated(&mut translators)
            .expect("data entry emits a controller");
        match UmpMessage::try_from(out.data_words()).expect("UMP") {
            UmpMessage::ChannelVoice2(Cv2::RegisteredController(m)) => {
                assert_eq!(u8::from(m.channel()), 3);
                assert_eq!(u8::from(m.bank()), 0x00);
                assert_eq!(u8::from(m.index()), 0x06);
            }
            other => panic!("expected RegisteredController, got {other:?}"),
        }

        // A plain note on a *different* device promotes straight through.
        let mut note = MidiInputEvent::synthetic(MidiEvent::note_on(0, 0, 60, 0x8000));
        note.device_id = 7;
        assert!(matches!(
            UmpMessage::try_from(note.translated(&mut translators).unwrap().data_words()).unwrap(),
            UmpMessage::ChannelVoice2(Cv2::NoteOn(_))
        ));
    }
}
