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

/// Wire protocol version, exchanged in the [`BridgeMessage::Ready`] handshake.
///
/// Bump on ANY change to the host↔server wire shape (a new or reordered field on
/// a serialized type), since bincode is not self-describing and a skew mis-parses
/// silently. Host and subprocess refuse to handshake on a mismatch.
///
/// # Why an appended field is still a bump
///
/// Two kinds of change are easy to mistake for compatible, and neither is:
///
/// - A **struct** is positional: fields are written in declaration order with no
///   tags, so a peer one version behind stops reading before the new field, and
///   a payload from the newer side runs the decoder off the end or into the next
///   field's bytes. `serde(default)` does not rescue this — it covers the JSON
///   and struct-update paths, not this wire.
/// - An **enum variant** is a varint discriminant over declaration order, so a
///   peer receiving a tag it has no arm for fails the decode mid-stream rather
///   than at a message boundary.
pub const PROTOCOL_VERSION: u32 = 19;

/// Largest control-socket frame body either end will allocate for, in bytes.
///
/// The wire is `[u32 big-endian length][bincode payload]`, so the length is
/// **attacker-controlled**: a corrupt or hostile peer can advertise `u32::MAX`,
/// and an unbounded reader would answer with `vec![0u8; len]` — a 4 GiB zeroed
/// allocation made before a single byte of body had been seen, let alone
/// validated. Under Linux overcommit that mapping is lazy and effectively free
/// (measured at 60 ns), which hides the hazard; with `overcommit_memory=2`,
/// a cgroup limit, or a 32-bit host, the allocator fails and Rust's OOM handler
/// **aborts the process** — the whole DAW, not the bridge.
///
/// **This bounds size, and only size.** It does *not* bound how long a read
/// takes, and must not be read as closing the dribble hazard: `SO_RCVTIMEO`
/// restarts on every syscall, so any frame — including a perfectly legitimate
/// under-cap one — can be paced out indefinitely one byte at a time. The
/// **total-elapsed deadline** in `util::transport::control::read_exact_by` is
/// what answers that, and the two are independent. Removing either re-opens a
/// hazard the other does not cover.
///
/// 64 MiB is far above any honest *control* frame and far below a denial of
/// service. Every message except plugin state is bounded by a fixed struct or
/// by `ParameterList`, and `midi_out` is capped at `MIDI_STACK_CAPACITY`
/// server-side.
///
/// **Plugin state is the exception, and it is not carried in one frame.**
/// A Kontakt- or Serum-class instrument embedding samples or wavetables
/// routinely exceeds 64 MiB, and a plugin storing an impulse response or a
/// recorded buffer trivially does — so capping state at this number would
/// silently lose a user's preset. State travels chunked instead
/// (`StateChunk`), bounded on reassembly by [`MAX_STATE_BYTES`]. Each wire
/// frame stays under this cap, so the allocation and deadline guarantees hold
/// unchanged for state as well. Raising *this* constant to cover the state tail
/// would re-widen the allocation surface for all traffic and still leave a wall
/// somewhere; a larger number just moves the cliff.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Largest **reassembled** plugin-state blob either end will accept, in bytes.
///
/// Bounds the total across a `StateChunk` sequence, which is a different
/// question from [`MAX_FRAME_BYTES`]: that one bounds a single allocation off a
/// single unvalidated length, this one bounds an accumulation across many
/// individually-valid frames. Without it, chunking would reintroduce the
/// unbounded allocation the frame cap exists to prevent — a peer would simply
/// send 64 MiB chunks forever.
///
/// 1 GiB, chosen to sit above the real tail and below anything that threatens
/// the host. The largest plugin states in practice are sample-embedding
/// instruments (Kontakt libraries, Serum wavetables) and convolution reverbs
/// carrying impulse responses; these reach high hundreds of MiB but not
/// gigabytes, because the formats themselves stream rather than inline beyond
/// that. A blob past 1 GiB is a plugin misbehaving or a corrupt project file,
/// and failing it with `StateError::TooLarge` is both honest and survivable —
/// unlike an allocation the host cannot satisfy.
///
/// Deliberately *not* enforced by aborting the connection: an over-limit state
/// is a per-command refusal, and the socket stays synchronised because the
/// receiver stops accumulating and drains the rest of the sequence.
pub const MAX_STATE_BYTES: usize = 1024 * 1024 * 1024;

/// Payload size of one `StateChunk` on the wire.
///
/// Comfortably under [`MAX_FRAME_BYTES`] so the framed message — chunk bytes
/// plus the `seq`/`last` header and bincode's own overhead — cannot approach
/// the cap however the envelope grows. 4 MiB keeps a 1 GiB state to 256 round
/// trips, which at local-socket speeds is not the bottleneck; the plugin's own
/// `get_state`/`set_state` is.
pub const STATE_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Validate a subprocess-reported protocol version against [`PROTOCOL_VERSION`].
/// Called at each handshake consumer so a version skew fails loudly instead of
/// mis-parsing later messages.
pub fn check_protocol_version(got: u32) -> crate::error::Result<()> {
    if got == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(crate::error::BridgeError::ProtocolMismatch {
            expected: PROTOCOL_VERSION,
            got,
        })
    }
}

pub use envelope::{BridgeMessage, HostMessage};
pub use midi::{IpcMidiEvent, IpcMidiEventVec, MidiEventVec, MIDI_STACK_CAPACITY};
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
    AuComponentType, ClapFeature, PluginClass, PluginDescriptor, Vst2Category, Vst3PlugType,
    Vst3SubCategories,
};
pub use tutti_plugin_types::{
    AutomationMode, BusChannels, ChannelLayout, ChannelTopology, ChordChanges, ChordValue,
    EditorPresence, FeatureReport, Features, LayoutSupport, LoadedPlugin, Normalized,
    NoteExpressionChanges, NoteExpressionIntChanges, NoteExpressionIntValue,
    NoteExpressionTextChanges, NoteExpressionTextValue, NoteExpressionType, NoteExpressionValue,
    ParamAddress, ParamFlags, ParamId, ParamRange, ParamSteps, ParameterChanges, ParameterInfo,
    ParameterPoint, ParameterQueue, PluginTail, Preset, PresetId, PresetSupport, RenderMode,
    Samples, ScaleChanges, ScaleValue, TimeSignature, TransportInfo,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tutti_midi_types::{CCNumber, MidiChannel, MidiGroup};

    /// Every preset frame survives the wire in both directions.
    ///
    /// These are the frames that make presets reachable out-of-process: an id
    /// produced by `PresetList` in the subprocess is handed back through
    /// `LoadPreset`, so a variant that does not round-trip is a preset that can
    /// be listed and never loaded.
    #[test]
    fn every_preset_frame_round_trips() {
        let requests = [
            HostMessage::GetPresetList,
            HostMessage::LoadPreset {
                id: PresetId::Number(9000),
            },
            HostMessage::LoadPreset {
                id: PresetId::Program {
                    list_id: 3,
                    index: 7,
                },
            },
            HostMessage::LoadPreset {
                id: PresetId::Location(PathBuf::from("/p/lead.clap-preset")),
            },
            HostMessage::GetCurrentPreset,
        ];
        for msg in requests {
            let bytes = bincode::serialize(&msg).expect("serialize");
            let back: HostMessage = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(
                format!("{back:?}"),
                format!("{msg:?}"),
                "{msg:?} did not survive the wire"
            );
        }

        let responses = [
            BridgeMessage::PresetList {
                presets: vec![
                    Preset::new(PresetId::Number(0), "Init"),
                    Preset::in_bank(
                        PresetId::Program {
                            list_id: 1,
                            index: 2,
                        },
                        "Warm",
                        "Bank 1",
                    ),
                ],
            },
            BridgeMessage::PresetLoaded { ok: true },
            BridgeMessage::PresetLoaded { ok: false },
            BridgeMessage::CurrentPreset {
                id: Some(PresetId::Number(4)),
            },
            BridgeMessage::CurrentPreset { id: None },
        ];
        for msg in responses {
            let bytes = bincode::serialize(&msg).expect("serialize");
            let back: BridgeMessage = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(
                format!("{back:?}"),
                format!("{msg:?}"),
                "{msg:?} did not survive the wire"
            );
        }
    }

    /// The preset frames were **appended**, leaving every older tag untouched.
    ///
    /// bincode encodes an enum as a leading `u32` discriminant over declaration
    /// order, so inserting a variant mid-enum silently renumbers every later
    /// one — and a symmetric round-trip cannot catch it, because both ends move
    /// together. The two ends here are not one build: a host and its subprocess
    /// negotiate `PROTOCOL_VERSION` and then trust each other's bytes.
    ///
    /// Pinning the *first* variant's tag is what makes this an append check
    /// rather than a restatement: tag 0 stays 0 only if nothing was inserted
    /// ahead of it, and the new variants land past every v13 tag.
    #[test]
    fn the_preset_frames_are_appended_not_inserted() {
        /// `SetRenderMode`'s tag — the last variant v13 shipped. Pinned as a
        /// literal so a shift is a failure here rather than an agreement
        /// between two expressions that move together.
        const V13_LAST_TAG: u32 = 17;

        let tag = |bytes: &[u8]| u32::from_le_bytes(bytes[..4].try_into().unwrap());

        let first = bincode::serialize(&HostMessage::ProbePlugin {
            path: PathBuf::new(),
        })
        .expect("serialize");
        assert_eq!(tag(&first), 0, "ProbePlugin must stay tag 0");

        // Absolute tags, not a relative comparison. A variant inserted
        // *between* two existing ones keeps both the tag-0 anchor and any
        // "later than" ordering intact while renumbering everything after the
        // insertion point — so only pinned numbers catch it. Verified by
        // mutation: inserting ahead of `SetRenderMode` survives a relative
        // check and fails these.
        assert_eq!(
            tag(&bincode::serialize(&HostMessage::SetRenderMode {
                mode: RenderMode::Offline,
            })
            .expect("serialize")),
            V13_LAST_TAG,
            "v13's last request variant moved; a v13 peer would mis-decode it"
        );
        assert_eq!(
            tag(&bincode::serialize(&HostMessage::GetPresetList).expect("serialize")),
            V13_LAST_TAG + 1,
            "GetPresetList must be the first v14 request frame"
        );
        assert_eq!(
            tag(&bincode::serialize(&HostMessage::LoadPreset {
                id: PresetId::Number(0),
            })
            .expect("serialize")),
            V13_LAST_TAG + 2,
        );
        assert_eq!(
            tag(&bincode::serialize(&HostMessage::GetCurrentPreset).expect("serialize")),
            V13_LAST_TAG + 3,
        );
    }

    /// Every parameter-display frame survives the wire in both directions.
    ///
    /// These are what make a plugin's own value text reachable out-of-process.
    /// `None` is carried explicitly in both replies: it is the answer for a
    /// plugin that declined, and a caller renders the raw number on it — so a
    /// variant that dropped the `Option` would turn "did not answer" into an
    /// empty label.
    #[test]
    fn every_parameter_display_frame_round_trips() {
        let requests = [
            HostMessage::GetParameterText {
                param_id: ParamAddress::Opaque(ParamId::new(0x8000_0001)),
                value: Normalized::new(0.375),
            },
            // The other addressing model, which VST2 uses.
            HostMessage::GetParameterText {
                param_id: ParamAddress::Index(3),
                value: Normalized::new(1.0),
            },
            HostMessage::GetParameterValueFromText {
                param_id: ParamAddress::Opaque(ParamId::new(7)),
                text: "800 Hz".to_string(),
            },
            HostMessage::GetParameterValueFromText {
                param_id: ParamAddress::Index(0),
                text: String::new(),
            },
        ];
        for msg in requests {
            let bytes = bincode::serialize(&msg).expect("serialize");
            let back: HostMessage = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(
                format!("{back:?}"),
                format!("{msg:?}"),
                "{msg:?} did not survive the wire"
            );
        }

        let responses = [
            BridgeMessage::ParameterText {
                text: Some("Bandpass".to_string()),
            },
            BridgeMessage::ParameterText { text: None },
            BridgeMessage::ParameterValueFromText {
                value: Some(Normalized::new(0.375)),
            },
            BridgeMessage::ParameterValueFromText { value: None },
        ];
        for msg in responses {
            let bytes = bincode::serialize(&msg).expect("serialize");
            let back: BridgeMessage = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(
                format!("{back:?}"),
                format!("{msg:?}"),
                "{msg:?} did not survive the wire"
            );
        }
    }

    /// The v17 frames were **appended**, leaving every older tag untouched.
    ///
    /// Same argument as [`the_preset_frames_are_appended_not_inserted`], and the
    /// same absolute pinning: a variant inserted between two existing ones keeps
    /// both the tag-0 anchor and any "later than" ordering intact while
    /// renumbering everything after it, so only literal numbers catch it.
    #[test]
    fn the_parameter_display_frames_are_appended_not_inserted() {
        /// `GetCurrentPreset`'s tag — the last request variant v14 shipped.
        const V16_LAST_REQUEST_TAG: u32 = 20;

        let tag = |bytes: &[u8]| u32::from_le_bytes(bytes[..4].try_into().unwrap());

        assert_eq!(
            tag(&bincode::serialize(&HostMessage::GetCurrentPreset).expect("serialize")),
            V16_LAST_REQUEST_TAG,
            "v16's last request variant moved; a v16 peer would mis-decode it"
        );
        assert_eq!(
            tag(&bincode::serialize(&HostMessage::GetParameterText {
                param_id: ParamAddress::Index(0),
                value: Normalized::new(0.0),
            })
            .expect("serialize")),
            V16_LAST_REQUEST_TAG + 1,
            "GetParameterText must be the first v17 request frame"
        );
        assert_eq!(
            tag(
                &bincode::serialize(&HostMessage::GetParameterValueFromText {
                    param_id: ParamAddress::Index(0),
                    text: String::new(),
                })
                .expect("serialize")
            ),
            V16_LAST_REQUEST_TAG + 2,
        );

        // The reply direction is a separate enum with its own numbering, so it
        // needs its own anchor rather than an offset from the request side.
        let shutdown = tag(&bincode::serialize(&BridgeMessage::Shutdown).expect("serialize"));
        assert_eq!(
            tag(
                &bincode::serialize(&BridgeMessage::ParameterText { text: None })
                    .expect("serialize")
            ),
            shutdown + 1,
            "ParameterText must follow v16's last reply variant"
        );
        assert_eq!(
            tag(
                &bincode::serialize(&BridgeMessage::ParameterValueFromText { value: None })
                    .expect("serialize")
            ),
            shutdown + 2,
        );
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
    fn audio_processed_round_trips_midi_out() {
        // The plugin's MIDI-out must survive the AudioProcessed reply wire trip.
        let midi_out: IpcMidiEventVec = [
            MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                midi1_velocity_to_midi2(100),
            )
            .with_frame_offset(0),
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::VOLUME,
                midi1_cc_to_midi2(64),
            )
            .with_frame_offset(64),
        ]
        .iter()
        .map(IpcMidiEvent::from)
        .collect();
        let msg = BridgeMessage::AudioProcessed {
            latency_us: 42,
            seq: 7,
            midi_out,
        };
        let bytes = bincode::serialize(&msg).unwrap();
        let back: BridgeMessage = bincode::deserialize(&bytes).unwrap();
        match back {
            BridgeMessage::AudioProcessed {
                latency_us,
                seq,
                midi_out,
            } => {
                assert_eq!(latency_us, 42);
                // The block-identity echo. Not load-bearing for audio
                // validity — the slab's per-slot sequence answers that — but it
                // must survive the wire trip for diagnostics to mean anything.
                assert_eq!(seq, 7);
                assert_eq!(midi_out.len(), 2);
                let events: Vec<MidiEvent> = midi_out.iter().map(|&e| e.into()).collect();
                assert_eq!(events[0].frame_offset, 0);
                assert!(is_note_on(&events[0]));
                assert_eq!(events[1].frame_offset, 64);
            }
            _ => panic!("wrong message type"),
        }
    }

    /// Every `TailChanged` arm survives the wire distinctly.
    ///
    /// The runtime signal is worth no more than the arm it carries: a
    /// plugin that raises its decay to "unbounded" and a plugin that drops
    /// to no tail at all must not arrive as the same message. This is the
    /// `LoadedPlugin.tail` wire test one level up, on the *update* path —
    /// the load-time field being round-trip-safe says nothing about a
    /// separately-encoded enum variant.
    #[test]
    fn every_tail_change_arm_survives_the_wire() {
        for tail in [
            PluginTail::Unknown,
            PluginTail::None,
            PluginTail::Finite(Samples(96_000)),
            PluginTail::Unbounded,
        ] {
            let bytes = bincode::serialize(&BridgeMessage::TailChanged { tail }).unwrap();
            match bincode::deserialize::<BridgeMessage>(&bytes).unwrap() {
                BridgeMessage::TailChanged { tail: back } => assert_eq!(back, tail),
                other => panic!("expected TailChanged, got {other:?}"),
            }
        }
    }

    #[test]
    fn protocol_version_matches_and_mismatches() {
        assert!(super::check_protocol_version(super::PROTOCOL_VERSION).is_ok());
        // A skew (0 = an ancient binary predating the version field) is rejected.
        assert!(super::check_protocol_version(0).is_err());
        assert!(super::check_protocol_version(super::PROTOCOL_VERSION + 1).is_err());
    }

    #[test]
    fn test_ipc_midi_event_roundtrip() {
        let event = MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            midi1_velocity_to_midi2(100),
        )
        .with_frame_offset(128);
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
            MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                midi1_velocity_to_midi2(100),
            )
            .with_frame_offset(0),
            MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                64,
                midi1_velocity_to_midi2(100),
            )
            .with_frame_offset(128),
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::VOLUME,
                midi1_cc_to_midi2(64),
            )
            .with_frame_offset(256),
        ]
        .iter()
        .map(IpcMidiEvent::from)
        .collect();

        let msg = HostMessage::ProcessAudio(Box::new(ProcessAudioData {
            seq: 42,
            num_samples: 512,
            midi_events,
            ..Default::default()
        }));

        let encoded = bincode::serialize(&msg).unwrap();
        let decoded: HostMessage = bincode::deserialize(&encoded).unwrap();

        match decoded {
            HostMessage::ProcessAudio(data) => {
                assert_eq!(data.seq, 42);
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
    fn test_transport_info_default() {
        let info = TransportInfo::default();
        assert_eq!(info.timing.tempo, 120.0);
        // 4/4 — the musical default, now expressed as the type's own default.
        assert_eq!(u32::from(info.timing.signature.beats_per_bar()), 4);
        assert_eq!(u32::from(info.timing.signature.note_value()), 4);
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
