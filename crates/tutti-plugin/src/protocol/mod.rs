//! Wire format for the plugin bridge. Every type here is `Serialize +
//! Deserialize` and flows between host and server process.
//!
//! Non-wire runtime types (`Sample`, `AudioBuffer<T>`, window handles)
//! live in [`crate::protocol::audio`] / [`crate::util::window`]. Host-local config lives
//! in [`crate::util::config`].

pub mod audio;
pub mod envelope;
pub mod midi;
pub mod process;
pub mod sample;
pub mod shm;

pub use envelope::{BridgeMessage, HostMessage};
pub use midi::{IpcMidiEvent, IpcMidiEventVec, MidiEventVec};
pub use process::ProcessAudioData;
pub use sample::SampleFormat;
pub use shm::SlabLayout;

pub use tutti_midi_types::ump::MidiEvent;

// Types that are owned elsewhere but speak on the wire, adopted here so
// `crate::protocol::{...}` is the single import point for the bridge.
//
// - Plugin load metadata: the catalog-identity `PluginDescriptor` (+ its
//   per-format `PluginClass`) lives in `crate::host::discovery::record`; the
//   runtime engine-wiring `LoadedPlugin` lives in the format-agnostic
//   `tutti-plugin-types`. They ride on `BridgeMessage::PluginLoaded`
//   (descriptor + loaded) / probe replies (descriptor only).
// - Parameters, transport snapshot, note-expression and harmony events:
//   cross-format vocabulary shared with the host crates
//   (`tutti-{vst2,vst3,clap,au}-host`) via `tutti-plugin-types`.
pub use crate::host::discovery::record::{
    AuComponentType, PluginClass, PluginDescriptor, Vst2Category,
};
pub use tutti_plugin_types::{
    BusChannels, ChordChanges, ChordValue, LoadedPlugin, NoteExpressionChanges,
    NoteExpressionIntChanges, NoteExpressionIntValue, NoteExpressionTextChanges,
    NoteExpressionTextValue, NoteExpressionType, NoteExpressionValue, ParameterChanges,
    ParameterFlags, ParameterInfo, ParameterPoint, ParameterQueue, ScaleChanges, ScaleValue,
    TransportInfo,
};

#[cfg(test)]
mod tests {
    use super::*;
    use audio_automation::ParameterScale;
    use std::path::PathBuf;

    #[test]
    fn test_message_serialization() {
        let msg = HostMessage::LoadPlugin {
            path: PathBuf::from("/test/plugin.vst3"),
            sample_rate: 44100.0,
            block_size: envelope::DEFAULT_BLOCK_SIZE,
            preferred_format: SampleFormat::Float32,
            shm_name: String::new(),
        };

        let encoded = bincode::serialize(&msg).unwrap();
        let decoded: HostMessage = bincode::deserialize(&encoded).unwrap();

        match decoded {
            HostMessage::LoadPlugin {
                path, sample_rate, ..
            } => {
                assert_eq!(path, PathBuf::from("/test/plugin.vst3"));
                assert_eq!(sample_rate, 44100.0);
            }
            _ => panic!("Wrong message type"),
        }
    }

    use tutti_midi_types::convert::{midi1_cc_to_midi2, midi1_velocity_to_midi2};

    fn is_note_on(ev: &MidiEvent) -> bool {
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2;
        use tutti_midi_types::midi2::UmpMessage;
        matches!(
            UmpMessage::try_from(ev.data_words()),
            Ok(UmpMessage::ChannelVoice2(ChannelVoice2::NoteOn(_)))
        )
    }

    fn note_number(ev: &MidiEvent) -> Option<u8> {
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2;
        use tutti_midi_types::midi2::UmpMessage;
        match UmpMessage::try_from(ev.data_words()).ok()? {
            UmpMessage::ChannelVoice2(ChannelVoice2::NoteOn(m)) => Some(u8::from(m.note_number())),
            UmpMessage::ChannelVoice2(ChannelVoice2::NoteOff(m)) => Some(u8::from(m.note_number())),
            _ => None,
        }
    }

    #[test]
    fn test_ipc_midi_event_roundtrip() {
        let event =
            MidiEvent::note_on(0, 0, 60, midi1_velocity_to_midi2(100)).with_frame_offset(128);
        let ipc_event = IpcMidiEvent::from(&event);
        assert_eq!(ipc_event.frame_offset, 128);

        let restored: MidiEvent = ipc_event.into();
        assert_eq!(restored.frame_offset, 128);
        assert!(is_note_on(&restored));
        assert_eq!(note_number(&restored), Some(60));
    }

    #[test]
    fn test_midi_message_serialization() {
        let midi_events: IpcMidiEventVec = [
            MidiEvent::note_on(0, 0, 60, midi1_velocity_to_midi2(100)).with_frame_offset(0),
            MidiEvent::note_on(0, 0, 64, midi1_velocity_to_midi2(100)).with_frame_offset(128),
            MidiEvent::cc(0, 0, 7, midi1_cc_to_midi2(64)).with_frame_offset(256),
        ]
        .iter()
        .map(IpcMidiEvent::from)
        .collect();

        let msg = HostMessage::ProcessAudio(Box::new(ProcessAudioData {
            buffer_id: 42,
            num_samples: 512,
            midi_events,
            ..Default::default()
        }));

        let encoded = bincode::serialize(&msg).unwrap();
        let decoded: HostMessage = bincode::deserialize(&encoded).unwrap();

        match decoded {
            HostMessage::ProcessAudio(data) => {
                assert_eq!(data.buffer_id, 42);
                assert_eq!(data.num_samples, 512);
                assert_eq!(data.midi_events.len(), 3);

                let events: Vec<MidiEvent> = data.midi_events.iter().map(|&e| e.into()).collect();
                assert_eq!(events.len(), 3);
                assert_eq!(events[0].frame_offset, 0);
                assert!(is_note_on(&events[0]));
                assert_eq!(note_number(&events[0]), Some(60));
                assert_eq!(events[1].frame_offset, 128);
                assert!(is_note_on(&events[1]));
                assert_eq!(note_number(&events[1]), Some(64));
                assert_eq!(events[2].frame_offset, 256);
            }
            _ => panic!("Wrong message type"),
        }
    }

    #[test]
    fn test_load_plugin_f64_serialization() {
        let msg = HostMessage::LoadPlugin {
            path: PathBuf::from("/test/reverb.vst3"),
            sample_rate: 96000.0,
            block_size: 1024,
            preferred_format: SampleFormat::Float64,
            shm_name: "test_shm".to_string(),
        };

        let encoded = bincode::serialize(&msg).unwrap();
        let decoded: HostMessage = bincode::deserialize(&encoded).unwrap();

        match decoded {
            HostMessage::LoadPlugin {
                path,
                sample_rate,
                block_size,
                preferred_format,
                ..
            } => {
                assert_eq!(path, PathBuf::from("/test/reverb.vst3"));
                assert_eq!(sample_rate, 96000.0);
                assert_eq!(block_size, 1024);
                assert_eq!(preferred_format, SampleFormat::Float64);
            }
            _ => panic!("Wrong message type"),
        }
    }

    #[test]
    fn test_to_range_toggle() {
        let mut info = ParameterInfo::new(1, "Bypass".to_string());
        info.step_count = 1;
        assert_eq!(info.to_range().scale, ParameterScale::Toggle);
    }

    #[test]
    fn test_to_range_integer() {
        let mut info = ParameterInfo::new(2, "Algorithm".to_string());
        info.step_count = 5;
        assert_eq!(info.to_range().scale, ParameterScale::Integer);
    }

    #[test]
    fn test_to_range_logarithmic_db() {
        let mut info = ParameterInfo::new(3, "Gain".to_string());
        info.unit = "dB".to_string();
        info.min_value = 0.001;
        info.max_value = 10.0;
        assert_eq!(info.to_range().scale, ParameterScale::Logarithmic);
    }

    #[test]
    fn test_to_range_logarithmic_hz() {
        let mut info = ParameterInfo::new(4, "Cutoff".to_string());
        info.unit = "Hz".to_string();
        info.min_value = 20.0;
        info.max_value = 20000.0;
        assert_eq!(info.to_range().scale, ParameterScale::Logarithmic);
    }

    #[test]
    fn test_to_range_log_fallback_non_positive_min() {
        let mut info = ParameterInfo::new(5, "Freq".to_string());
        info.unit = "Hz".to_string();
        info.min_value = 0.0;
        info.max_value = 20000.0;
        assert_eq!(info.to_range().scale, ParameterScale::Linear);
    }

    #[test]
    fn test_to_range_linear_default() {
        let info = ParameterInfo::new(6, "Mix".to_string());
        assert_eq!(info.to_range().scale, ParameterScale::Linear);
    }

    #[test]
    fn test_to_range_values_preserved() {
        let mut info = ParameterInfo::new(7, "Volume".to_string());
        info.min_value = -96.0;
        info.max_value = 6.0;
        info.default_value = -12.0;
        let range = info.to_range();
        assert_eq!(range.min, -96.0);
        assert_eq!(range.max, 6.0);
        assert_eq!(range.default, -12.0);
    }

    #[test]
    fn test_parameter_changes_add_queue() {
        let mut changes = ParameterChanges::new();
        assert!(changes.is_empty());

        let mut queue = ParameterQueue::new(42);
        queue.add_point(0, 0.5);
        queue.add_point(128, 0.8);
        changes.add_queue(queue);

        assert!(!changes.is_empty());
        assert_eq!(changes.queues.len(), 1);
        assert_eq!(changes.queues[0].param_id, 42);
        assert_eq!(changes.queues[0].points.len(), 2);
    }

    #[test]
    fn test_transport_info_default() {
        let info = TransportInfo::default();
        assert_eq!(info.timing.tempo, 120.0);
        assert_eq!(info.timing.time_sig_numerator, 4);
        assert_eq!(info.timing.time_sig_denominator, 4);
        assert!(!info.state.playing);
        assert!(!info.state.recording);
    }

    /// The harmony inputs (chord / scale / text / int) must survive a bincode
    /// round-trip inside `ProcessAudioData`, including the owned `String`
    /// names.
    #[test]
    fn process_audio_round_trips_harmony_fields() {
        let mut data = ProcessAudioData {
            num_samples: 256,
            ..Default::default()
        };
        data.chords.add_change(ChordValue {
            sample_offset: 0,
            root: 60,
            bass_note: 48,
            mask: 0b1001,
            text: "Cmaj7".to_string(),
        });
        data.scales.add_change(ScaleValue {
            sample_offset: 0,
            root: 62,
            mask: 0x5ab5,
            text: "D Dorian".to_string(),
        });
        data.expr_texts.add_change(NoteExpressionTextValue {
            sample_offset: 4,
            note_id: 7,
            type_id: 1,
            text: "staccato".to_string(),
        });
        data.expr_ints.add_change(NoteExpressionIntValue {
            sample_offset: 8,
            note_id: 7,
            type_id: 2,
            value: -42,
        });

        let bytes = bincode::serialize(&data).unwrap();
        let back: ProcessAudioData = bincode::deserialize(&bytes).unwrap();

        assert_eq!(back.num_samples, 256);
        assert_eq!(back.chords.changes[0].text, "Cmaj7");
        assert_eq!(back.chords.changes[0].root, 60);
        assert_eq!(back.scales.changes[0].text, "D Dorian");
        assert_eq!(back.expr_texts.changes[0].text, "staccato");
        assert_eq!(back.expr_ints.changes[0].value, -42);
    }
}
