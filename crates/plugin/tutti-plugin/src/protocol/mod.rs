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
/// Two shapes recur in the log below, and neither is safe to skip:
///
/// - A **struct** is positional: fields are written in declaration order with no
///   tags, so a peer one version behind stops reading before the new field, and
///   a payload from the newer side runs the decoder off the end or into the next
///   field's bytes. `serde(default)` does not rescue this — it covers the JSON
///   and struct-update paths, not this wire.
/// - An **enum variant** is a varint discriminant over declaration order, so a
///   peer receiving a tag it has no arm for fails the decode mid-stream rather
///   than at a message boundary.
///
/// # Version log
///
/// - v1: baseline.
/// - v2: `BridgeMessage::AudioProcessed` carries plugin `midi_out`.
/// - v3: `AudioProcessed` echoes the request's `buffer_id`, so the host can tell
///   a reply for THIS block from a stale one.
/// - v4: pipelined audio. `buffer_id: u32` becomes `seq: u64` indexing a ring of
///   slab slots; `SlabLayout` trades a flat `channels` total for `slots`, with
///   the two directions in disjoint regions. Mandatory: a v3 server reads `slots`
///   out of the bytes holding `channels`, gets a plausible small integer, and
///   maps a wrong-sized region in silence. The slab header's magic is the second
///   line of defence.
/// - v5: `ParameterInfo` is restructured so an absent declaration is not spelled
///   as a number — `ParamRange` (whose `Normalized` arm carries no bounds),
///   `ParamSteps` (splitting "continuous" from "unreported", which a bare count
///   fused at zero), and a `ParamFlags` bitset plus a `known` mask, so a
///   capability the format never reported reads as `None` rather than `false`.
///   Mandatory: bincode carries no field names, so a v4 payload deserializes from
///   misaligned bytes.
/// - v6: `PluginDescriptor::has_editor: bool` becomes `editor: EditorPresence`,
///   three-valued, because the probe paths cannot instantiate and so cannot
///   answer. Mandatory: the bool and the discriminant are both one byte, so a
///   skewed peer decodes plausible garbage rather than failing.
///
///   The persisted catalog is NOT governed by this constant — it is JSON, and
///   `PluginDescriptor::editor` carries `serde(default)` so an existing database
///   loads with `Unknown` instead of being quarantined.
/// - v7: `LoadedPlugin` gains `probed: Features`, the mask saying which
///   capabilities the loader actually asked about. Without it a clear `Features`
///   bit fuses "the plugin declined", "this loader never asked" and "the format
///   has no query" — and the AU loader probes exactly one of the ten. Appended
///   struct field.
/// - v8: the `HostMessage` `param_id: u32` fields become `ParamAddress`,
///   distinguishing an opaque plugin-chosen handle (VST3/CLAP/AU) from a VST2
///   positional index. Indistinguishable on the wire, the receiving loader had
///   to assume its own format's model — silently wrong the moment an address is
///   forwarded. Mandatory: the discriminant byte shifts every following field.
/// - v9: `ParameterQueue::param_id` becomes a `ParamAddress`, putting the
///   automation path on the same address type as v8's direct path. Mandatory for
///   v8's reason: the discriminant byte shifts every following field.
/// - v10: `LoadedPlugin` gains `tail: PluginTail` — how long a plugin keeps
///   sounding after its input stops, which a bounce needs so it does not truncate
///   a reverb mid-decay. A sum type rather than a count because "unbounded" and
///   "never asked" are real answers and neither is a number: an infinite tail
///   through `Seconds::to_samples` arrives as `Samples(0)`, bit-identical to no
///   tail at all. Appended struct field.
/// - v11: `BridgeMessage` gains `TailChanged { tail }`, so a tail that changes at
///   runtime reaches the host rather than being polled and dropped. CLAP requires
///   it: `clap.tail` pairs the plugin's `get` with a host `changed` callback
///   because raising a reverb's decay changes the tail after load. Appended
///   variant.
/// - v12: `PluginClass` carries parsed classification vocabularies instead of raw
///   strings — `Vst3 { category }` is a `Vst3SubCategories`, `Clap { features }` a
///   `Vec<ClapFeature>`. Mandatory: both sit *inside* an existing field rather
///   than appended, so a v11 peer decodes a `Vec<String>` where a
///   `Vec<ClapFeature>` was written and desynchronizes mid-message.
///
///   The persisted JSON catalog changes shape too, and unlike this wire it has no
///   version negotiation: `PluginDatabase::load` quarantines a file it cannot
///   parse, so an existing catalog is discarded and rescanned.
/// - v13: `HostMessage` gains `SetRenderMode { mode }`, so an offline bounce can
///   tell a hosted plugin it is not under realtime pressure. Appended variant,
///   but mandatory in one direction only — a v13 host sends it only when a caller
///   asks for offline, so a v12 server would survive a realtime session. The bump
///   refuses the pairing outright rather than leaving that to luck.
/// - v14: presets cross the wire — `HostMessage` gains `GetPresetList`,
///   `LoadPreset` and `GetCurrentPreset`; `BridgeMessage` gains `PresetList`,
///   `PresetLoaded` and `CurrentPreset`. Six appended variants, mandatory in both
///   directions: a v14 host asks for a preset list whenever a caller opens a
///   browser, so a v13 server meets an unknown tag in ordinary use.
/// - v15: `LoadedPlugin` gains `input_topology` / `output_topology`, carrying
///   *which speaker* each channel feeds beside the counts it already reported.
///   Appended struct fields, so a v14 server sends a payload two fields short.
/// - v16: `ParameterInfo` gains `group`, the display label for the group a
///   parameter belongs to. Every hosted format has a grouping mechanism and all
///   four format crates already decoded theirs, but the answers stopped at the
///   shared type — so a 400-parameter synth presented as one flat list. Appended
///   struct field.
///
///   `ParameterInfo::qualified_name` lands with it, so the field arrives with its
///   consumer rather than write-only. See
///   `docs/design/010-parameter-grouping.md`.
/// - v17: parameter *display* crosses the wire — `HostMessage` gains
///   `GetParameterText` and `GetParameterValueFromText`; `BridgeMessage` gains
///   `ParameterText` and `ParameterValueFromText`. All four formats implement
///   value↔text and none of the answers reached the host, leaving a caller with a
///   normalized `0.5` where the plugin would say `"800 Hz"`. Host-side formatting
///   cannot recover it — only the plugin knows its own taper and value names.
///   Four appended variants, mandatory in both directions: a v17 host sends these
///   whenever a caller renders a parameter field.
/// - v18: `load_state` gets an answer — `BridgeMessage` gains `StateLoaded`.
///   Without a reply frame the host dispatcher answered its own caller
///   immediately after writing the request, so the `bool` said the message had
///   been *sent*, not that the state had been *loaded*, and a plugin rejecting a
///   chunk surfaced as a silently un-restored preset. Appended variant, mandatory
///   in both directions: a v18 host waits for this frame, so a v17 server hangs
///   the caller until the state timeout.
pub const PROTOCOL_VERSION: u32 = 18;

/// Largest control-socket frame body either end will allocate for, in bytes.
///
/// The wire is `[u32 big-endian length][bincode payload]`, so the length is
/// **attacker-controlled**: a corrupt or hostile peer can advertise `u32::MAX`
/// and both readers used to answer with `vec![0u8; len]` — a 4 GiB zeroed
/// allocation made before a single byte of body had been seen, let alone
/// validated. Two things go wrong without a bound, and neither is theoretical:
///
/// - **The allocation.** Under Linux overcommit the mapping is cheap and the
///   hazard hides; with `overcommit_memory=2`, a cgroup limit, or a 32-bit
///   host, the allocator fails and Rust's OOM handler **aborts the process** —
///   the whole DAW, not the bridge.
/// - **The read that follows.** `read_exact` loops, and `SO_RCVTIMEO` restarts
///   on *every* syscall, so it bounds one `recv` and not the call. A peer that
///   dribbles one byte per timeout keeps the bridge thread inside `read_exact`
///   indefinitely, holding the buffer, never reaching the error that would
///   mark it crashed. `recv_within`'s timeout cannot end it and neither can the
///   caller's — the caller gives up and returns `None`, but the bridge thread
///   stays wedged and the bridge is never marked dead. Measured: a 300 ms
///   `SO_RCVTIMEO` survived 2.3 s of one-byte-per-200 ms dribble.
///
/// Checking the length *before* allocating closes both: an over-cap frame is
/// rejected in the four bytes it takes to read the prefix, with no allocation
/// and no second read to be stalled inside.
///
/// 64 MiB is far above any honest frame and far below a denial of service. The
/// largest message either direction can carry is `BridgeMessage::StateData` /
/// `HostMessage::LoadState` — a plugin's opaque state chunk, which for a
/// sample-based instrument legitimately reaches single-digit MiB. Every other
/// variant is bounded by a fixed struct or by `ParameterList`, and
/// `midi_out` is already capped at `MIDI_STACK_CAPACITY` server-side. A plugin
/// whose state genuinely exceeds this needs a chunked transfer, not a larger
/// number here.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

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
                // The block-identity echo. No longer load-bearing for audio
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
