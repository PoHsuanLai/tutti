//! `midi2` as a decoding oracle.
//!
//! This is the only file that names `midi2`. When the crate is dropped, delete
//! this file and the `midi2` dev-dependency; nothing else changes, because every
//! disagreement found is pinned as an analytic case in `spec_words.rs` or
//! `reserved.rs`, from the words in M2-104.
//!
//! Random packets go through both decoders. Both are projected to a `Fact`: a
//! family and the field values `midi2` exposes (Flex, Stream and Mixed Data Set
//! compare as the family alone, and SysEx as group and payload). A packet one
//! decoder accepts and the other refuses, or accepts as another message or with
//! other fields, is a disagreement. `cause` names each one settled against
//! M2-104, and a disagreement it does not explain fails the test, which prints
//! every distinct one with a witness (`--no-capture`).
//!
//! Mutation record (seen to fail with `UNEXPLAINED`, restored): the MIDI 2.0
//! Note On velocity read from bit 15 in `decode` -> `cv2NoteOn: fields differ`.

mod common;

use midi2::channel_voice1::ChannelVoice1;
use midi2::channel_voice2::ChannelVoice2;
use midi2::system_common::SystemCommon;
use midi2::utility::Utility as Util;
use midi2::{Channeled, Grouped, UmpMessage};
use tutti_ump::{Address, Attribute, Message, System, Ump, Utility, Voice1, Voice2, decode};

#[derive(Debug, PartialEq, Eq, Clone)]
struct Fact {
    kind: String,
    fields: Vec<i64>,
}

fn fact(kind: &str, fields: &[i64]) -> Fact {
    Fact {
        kind: kind.to_string(),
        fields: fields.to_vec(),
    }
}

fn name(debug: String) -> String {
    debug
        .split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or("")
        .to_string()
}

fn addr(a: Address) -> [i64; 2] {
    [i64::from(a.group.get()), i64::from(a.channel.get())]
}

fn with(kind: &str, a: [i64; 2], rest: &[i64]) -> Fact {
    let mut f = a.to_vec();
    f.extend_from_slice(rest);
    fact(kind, &f)
}

fn ours(m: &Message) -> Fact {
    match *m {
        Message::Utility(u) => match u {
            Utility::Noop => fact("NoOp", &[]),
            Utility::JrClock(t) => fact("Clock", &[i64::from(t)]),
            Utility::JrTimestamp(t) => fact("Timestamp", &[i64::from(t)]),
            Utility::TicksPerQuarterNote(n) => fact("DeltaClockstampTpq", &[i64::from(n.get())]),
            Utility::DeltaClockstamp(t) => fact("DeltaClockstamp", &[i64::from(t.get())]),
        },
        Message::System(g, s) => {
            let g = i64::from(g.get());
            match s {
                System::TimeCode(d) => fact("TimeCode", &[g, i64::from(d.get())]),
                System::SongPosition(p) => fact("SongPositionPointer", &[g, i64::from(p.get())]),
                System::SongSelect(d) => fact("SongSelect", &[g, i64::from(d.get())]),
                System::TuneRequest => fact("TuneRequest", &[g]),
                System::TimingClock => fact("TimingClock", &[g]),
                System::Start => fact("Start", &[g]),
                System::Continue => fact("Continue", &[g]),
                System::Stop => fact("Stop", &[g]),
                System::ActiveSensing => fact("ActiveSensing", &[g]),
                System::Reset => fact("Reset", &[g]),
            }
        }
        Message::Midi1(a, v) => {
            let a = addr(a);
            match v {
                Voice1::NoteOff { note, velocity } => with(
                    "cv1NoteOff",
                    a,
                    &[i64::from(note.get()), i64::from(velocity.get())],
                ),
                Voice1::NoteOn { note, velocity } => with(
                    "cv1NoteOn",
                    a,
                    &[i64::from(note.get()), i64::from(velocity.get())],
                ),
                Voice1::PolyPressure { note, pressure } => with(
                    "cv1KeyPressure",
                    a,
                    &[i64::from(note.get()), i64::from(pressure.get())],
                ),
                Voice1::ControlChange { controller, value } => with(
                    "cv1ControlChange",
                    a,
                    &[i64::from(controller.get()), i64::from(value.get())],
                ),
                Voice1::ProgramChange(p) => with("cv1ProgramChange", a, &[i64::from(p.get())]),
                Voice1::ChannelPressure(p) => with("cv1ChannelPressure", a, &[i64::from(p.get())]),
                Voice1::PitchBend(b) => with("cv1PitchBend", a, &[i64::from(b.get())]),
            }
        }
        Message::Midi2(a, v) => {
            let a = addr(a);
            let attr = |at: Attribute| match at {
                Attribute::None => [0, 0],
                Attribute::Manufacturer(d) => [1, i64::from(d)],
                Attribute::Profile(d) => [2, i64::from(d)],
                Attribute::Pitch(d) => [3, i64::from(d)],
            };
            match v {
                Voice2::RegisteredPerNote {
                    note,
                    controller,
                    value,
                } => {
                    let _ = controller;
                    with(
                        "RegisteredPerNoteController",
                        a,
                        &[i64::from(note.get()), i64::from(value)],
                    )
                }
                Voice2::AssignablePerNote {
                    note,
                    controller,
                    value,
                } => with(
                    "AssignablePerNoteController",
                    a,
                    &[
                        i64::from(note.get()),
                        i64::from(controller),
                        i64::from(value),
                    ],
                ),
                Voice2::Registered { bank, index, value } => with(
                    "RegisteredController",
                    a,
                    &[
                        i64::from(bank.get()),
                        i64::from(index.get()),
                        i64::from(value),
                    ],
                ),
                Voice2::Assignable { bank, index, value } => with(
                    "AssignableController",
                    a,
                    &[
                        i64::from(bank.get()),
                        i64::from(index.get()),
                        i64::from(value),
                    ],
                ),
                Voice2::RelativeRegistered { bank, index, delta } => with(
                    "RelativeRegisteredController",
                    a,
                    &[
                        i64::from(bank.get()),
                        i64::from(index.get()),
                        i64::from(delta as u32),
                    ],
                ),
                Voice2::RelativeAssignable { bank, index, delta } => with(
                    "RelativeAssignableController",
                    a,
                    &[
                        i64::from(bank.get()),
                        i64::from(index.get()),
                        i64::from(delta as u32),
                    ],
                ),
                Voice2::PerNotePitchBend { note, value } => with(
                    "PerNotePitchBend",
                    a,
                    &[i64::from(note.get()), i64::from(value)],
                ),
                Voice2::NoteOff {
                    note,
                    velocity,
                    attribute,
                } => {
                    let at = attr(attribute);
                    with(
                        "cv2NoteOff",
                        a,
                        &[i64::from(note.get()), i64::from(velocity), at[0], at[1]],
                    )
                }
                Voice2::NoteOn {
                    note,
                    velocity,
                    attribute,
                } => {
                    let at = attr(attribute);
                    with(
                        "cv2NoteOn",
                        a,
                        &[i64::from(note.get()), i64::from(velocity), at[0], at[1]],
                    )
                }
                Voice2::PolyPressure { note, value } => {
                    with("KeyPressure", a, &[i64::from(note.get()), i64::from(value)])
                }
                Voice2::ControlChange { controller, value } => with(
                    "cv2ControlChange",
                    a,
                    &[i64::from(controller.get()), i64::from(value)],
                ),
                Voice2::ProgramChange { program, bank } => {
                    let bank =
                        bank.map_or(-1, |b| i64::from(b.msb.get()) << 7 | i64::from(b.lsb.get()));
                    with("cv2ProgramChange", a, &[i64::from(program.get()), bank])
                }
                Voice2::ChannelPressure(v) => with("cv2ChannelPressure", a, &[i64::from(v)]),
                Voice2::PitchBend(v) => with("ChannelPitchBend", a, &[i64::from(v)]),
                Voice2::PerNoteManagement {
                    note,
                    detach,
                    reset,
                } => with(
                    "PerNoteManagement",
                    a,
                    &[i64::from(note.get()), i64::from(detach), i64::from(reset)],
                ),
            }
        }
        Message::Sysex7(g, p) => {
            let mut f = vec![i64::from(g.get())];
            f.extend(p.bytes().iter().map(|b| i64::from(b.get())));
            fact("Sysex7", &f)
        }
        Message::Data128(g, tutti_ump::Data128::Sysex8(p)) => {
            let mut f = vec![i64::from(g.get()), i64::from(p.stream_id())];
            f.extend(p.bytes().iter().map(|&b| i64::from(b)));
            fact("Sysex8", &f)
        }
        Message::Data128(g, tutti_ump::Data128::Sysex8Abort { stream_id }) => {
            fact("Sysex8", &[i64::from(g.get()), i64::from(stream_id)])
        }
        Message::Data128(..) => fact("MixedDataSet", &[]),
        Message::Flex(_, _) | Message::Stream(_) => fact(&flex_stream_name(m), &[]),
    }
}

fn flex_stream_name(m: &Message) -> String {
    use tutti_ump::{Flex, FlexTextKind as K};
    match m {
        Message::Flex(_, Flex::SetTempo(_)) => "SetTempo".into(),
        Message::Flex(_, Flex::SetTimeSignature(_)) => "SetTimeSignature".into(),
        Message::Flex(_, Flex::SetMetronome(_)) => "SetMetronome".into(),
        Message::Flex(_, Flex::SetKeySignature(..)) => "SetKeySignature".into(),
        Message::Flex(_, Flex::SetChordName(..)) => "SetChordName".into(),
        Message::Flex(_, Flex::Text { kind, .. }) => match kind {
            K::UnknownMetadata => "UnknownMetadataText",
            K::ProjectName => "ProjectName",
            K::CompositionName => "CompositionName",
            K::ClipName => "MidiClipName",
            K::Copyright => "CopyrightNotice",
            K::Composer => "ComposerName",
            K::Lyricist => "LyricistName",
            K::Arranger => "ArrangerName",
            K::Publisher => "PublisherName",
            K::PrimaryPerformer => "PrimaryPerformerName",
            K::AccompanyingPerformer => "AccompanyingPerformerName",
            K::RecordingDate => "RecordingDate",
            K::RecordingLocation => "RecordingLocation",
            K::UnknownPerformance => "UnknownPerformanceText",
            K::Lyrics => "Lyrics",
            K::LyricsLanguage => "LyricsLanguage",
            K::Ruby => "Ruby",
            K::RubyLanguage => "RubyLanguage",
        }
        .into(),
        Message::Stream(s) => name(format!("{s:?}")),
        _ => String::new(),
    }
}

fn theirs(words: &[u32]) -> Option<Fact> {
    let m = UmpMessage::try_from(words).ok()?;
    Some(match m {
        UmpMessage::Utility(u) => match u {
            Util::NoOp(_) => fact("NoOp", &[]),
            Util::Clock(m) => fact("Clock", &[i64::from(m.time_data())]),
            Util::Timestamp(m) => fact("Timestamp", &[i64::from(m.time_data())]),
            Util::DeltaClockstampTpq(m) => fact("DeltaClockstampTpq", &[i64::from(m.time_data())]),
            Util::DeltaClockstamp(m) => {
                fact("DeltaClockstamp", &[i64::from(u32::from(m.time_data()))])
            }
            #[allow(unreachable_patterns)]
            _ => fact("?", &[]),
        },
        UmpMessage::SystemCommon(s) => match s {
            SystemCommon::TimeCode(m) => fact(
                "TimeCode",
                &[
                    i64::from(u8::from(m.group())),
                    i64::from(u8::from(m.time_code())),
                ],
            ),
            SystemCommon::SongPositionPointer(m) => fact(
                "SongPositionPointer",
                &[
                    i64::from(u8::from(m.group())),
                    i64::from(u16::from(m.position())),
                ],
            ),
            SystemCommon::SongSelect(m) => fact(
                "SongSelect",
                &[
                    i64::from(u8::from(m.group())),
                    i64::from(u8::from(m.song())),
                ],
            ),
            SystemCommon::TuneRequest(m) => fact("TuneRequest", &[i64::from(u8::from(m.group()))]),
            SystemCommon::TimingClock(m) => fact("TimingClock", &[i64::from(u8::from(m.group()))]),
            SystemCommon::Start(m) => fact("Start", &[i64::from(u8::from(m.group()))]),
            SystemCommon::Continue(m) => fact("Continue", &[i64::from(u8::from(m.group()))]),
            SystemCommon::Stop(m) => fact("Stop", &[i64::from(u8::from(m.group()))]),
            SystemCommon::ActiveSensing(m) => {
                fact("ActiveSensing", &[i64::from(u8::from(m.group()))])
            }
            SystemCommon::Reset(m) => fact("Reset", &[i64::from(u8::from(m.group()))]),
            _ => fact("?", &[]),
        },
        UmpMessage::ChannelVoice1(v) => {
            macro_rules! a {
                ($m:expr) => {
                    [
                        i64::from(u8::from($m.group())),
                        i64::from(u8::from($m.channel())),
                    ]
                };
            }
            match v {
                ChannelVoice1::NoteOff(m) => with(
                    "cv1NoteOff",
                    a!(m),
                    &[
                        i64::from(u8::from(m.note_number())),
                        i64::from(u8::from(m.velocity())),
                    ],
                ),
                ChannelVoice1::NoteOn(m) => with(
                    "cv1NoteOn",
                    a!(m),
                    &[
                        i64::from(u8::from(m.note_number())),
                        i64::from(u8::from(m.velocity())),
                    ],
                ),
                ChannelVoice1::KeyPressure(m) => with(
                    "cv1KeyPressure",
                    a!(m),
                    &[
                        i64::from(u8::from(m.note_number())),
                        i64::from(u8::from(m.pressure())),
                    ],
                ),
                ChannelVoice1::ControlChange(m) => with(
                    "cv1ControlChange",
                    a!(m),
                    &[
                        i64::from(u8::from(m.control())),
                        i64::from(u8::from(m.control_data())),
                    ],
                ),
                ChannelVoice1::ProgramChange(m) => with(
                    "cv1ProgramChange",
                    a!(m),
                    &[i64::from(u8::from(m.program()))],
                ),
                ChannelVoice1::ChannelPressure(m) => with(
                    "cv1ChannelPressure",
                    a!(m),
                    &[i64::from(u8::from(m.pressure()))],
                ),
                ChannelVoice1::PitchBend(m) => {
                    with("cv1PitchBend", a!(m), &[i64::from(u16::from(m.bend()))])
                }
            }
        }
        UmpMessage::ChannelVoice2(v) => {
            macro_rules! a {
                ($m:expr) => {
                    [
                        i64::from(u8::from($m.group())),
                        i64::from(u8::from($m.channel())),
                    ]
                };
            }
            let at = |a: Option<midi2::channel_voice2::NoteAttribute>| match a {
                None => [0, 0],
                Some(midi2::channel_voice2::NoteAttribute::ManufacturerSpecific(d)) => {
                    [1, i64::from(d)]
                }
                Some(midi2::channel_voice2::NoteAttribute::ProfileSpecific(d)) => [2, i64::from(d)],
                Some(midi2::channel_voice2::NoteAttribute::Pitch7_9(d)) => {
                    [3, i64::from(d.to_bits())]
                }
                Some(_) => [-1, -1],
            };
            match v {
                ChannelVoice2::NoteOn(m) => {
                    let t = at(m.attribute());
                    with(
                        "cv2NoteOn",
                        a!(m),
                        &[
                            i64::from(u8::from(m.note_number())),
                            i64::from(m.velocity()),
                            t[0],
                            t[1],
                        ],
                    )
                }
                ChannelVoice2::NoteOff(m) => {
                    let t = at(m.attribute());
                    with(
                        "cv2NoteOff",
                        a!(m),
                        &[
                            i64::from(u8::from(m.note_number())),
                            i64::from(m.velocity()),
                            t[0],
                            t[1],
                        ],
                    )
                }
                ChannelVoice2::KeyPressure(m) => with(
                    "KeyPressure",
                    a!(m),
                    &[
                        i64::from(u8::from(m.note_number())),
                        i64::from(m.key_pressure_data()),
                    ],
                ),
                ChannelVoice2::ControlChange(m) => with(
                    "cv2ControlChange",
                    a!(m),
                    &[
                        i64::from(u8::from(m.control())),
                        i64::from(m.control_change_data()),
                    ],
                ),
                ChannelVoice2::ChannelPitchBend(m) => {
                    with("ChannelPitchBend", a!(m), &[i64::from(m.pitch_bend_data())])
                }
                ChannelVoice2::ChannelPressure(m) => with(
                    "cv2ChannelPressure",
                    a!(m),
                    &[i64::from(m.channel_pressure_data())],
                ),
                ChannelVoice2::PerNotePitchBend(m) => with(
                    "PerNotePitchBend",
                    a!(m),
                    &[
                        i64::from(u8::from(m.note_number())),
                        i64::from(m.pitch_bend_data()),
                    ],
                ),
                ChannelVoice2::PerNoteManagement(m) => with(
                    "PerNoteManagement",
                    a!(m),
                    &[
                        i64::from(u8::from(m.note_number())),
                        i64::from(m.detach()),
                        i64::from(m.reset()),
                    ],
                ),
                ChannelVoice2::ProgramChange(m) => with(
                    "cv2ProgramChange",
                    a!(m),
                    &[
                        i64::from(u8::from(m.program())),
                        m.bank().map_or(-1, |b| i64::from(u16::from(b))),
                    ],
                ),
                ChannelVoice2::RegisteredController(m) => with(
                    "RegisteredController",
                    a!(m),
                    &[
                        i64::from(u8::from(m.bank())),
                        i64::from(u8::from(m.index())),
                        i64::from(m.controller_data()),
                    ],
                ),
                ChannelVoice2::AssignableController(m) => with(
                    "AssignableController",
                    a!(m),
                    &[
                        i64::from(u8::from(m.bank())),
                        i64::from(u8::from(m.index())),
                        i64::from(m.controller_data()),
                    ],
                ),
                ChannelVoice2::RelativeRegisteredController(m) => with(
                    "RelativeRegisteredController",
                    a!(m),
                    &[
                        i64::from(u8::from(m.bank())),
                        i64::from(u8::from(m.index())),
                        i64::from(m.controller_data()),
                    ],
                ),
                ChannelVoice2::RelativeAssignableController(m) => with(
                    "RelativeAssignableController",
                    a!(m),
                    &[
                        i64::from(u8::from(m.bank())),
                        i64::from(u8::from(m.index())),
                        i64::from(m.controller_data()),
                    ],
                ),
                ChannelVoice2::AssignablePerNoteController(m) => with(
                    "AssignablePerNoteController",
                    a!(m),
                    &[
                        i64::from(u8::from(m.note_number())),
                        i64::from(m.index()),
                        i64::from(m.controller_data()),
                    ],
                ),
                ChannelVoice2::RegisteredPerNoteController(m) => {
                    use midi2::channel_voice2::Controller as C;
                    let data = match m.controller() {
                        C::Pitch7_25(d) => i64::from(d.to_bits()),
                        C::Modulation(d)
                        | C::Breath(d)
                        | C::Volume(d)
                        | C::Balance(d)
                        | C::Pan(d)
                        | C::Expression(d)
                        | C::SoundVariation(d)
                        | C::Timbre(d)
                        | C::ReleaseTime(d)
                        | C::AttackTime(d)
                        | C::Brightness(d)
                        | C::DecayTime(d)
                        | C::VebratoRate(d)
                        | C::VebratoDepth(d)
                        | C::VebratoDelay(d)
                        | C::Undefined(d)
                        | C::ReverbSendLevel(d)
                        | C::ChorusSendLevel(d) => i64::from(d),
                        C::SoundController { data, .. } | C::EffectDepth { data, .. } => {
                            i64::from(data)
                        }
                        #[allow(unreachable_patterns)]
                        _ => -1,
                    };
                    with(
                        "RegisteredPerNoteController",
                        a!(m),
                        &[i64::from(u8::from(m.note_number())), data],
                    )
                }
            }
        }
        UmpMessage::Sysex7(m) => {
            use midi2::Sysex;
            // The packet's status is not exposed; group and payload are.
            let mut f = vec![i64::from(u8::from(m.group()))];
            f.extend(m.payload().map(|b| i64::from(u8::from(b))));
            fact("Sysex7", &f)
        }
        UmpMessage::Sysex8(m) => {
            use midi2::Sysex;
            let mut f = vec![i64::from(u8::from(m.group())), i64::from(m.stream_id())];
            f.extend(m.payload().map(i64::from));
            fact("Sysex8", &f)
        }
        UmpMessage::FlexData(m) => fact(&name(format!("{m:?}")), &[]),
        UmpMessage::UmpStream(m) => fact(&name(format!("{m:?}")), &[]),
        _ => fact("?", &[]),
    })
}

/// The cause of a disagreement, when it is one settled against M2-104. The
/// predicates read the words directly. A disagreement none of them explains is
/// new, and fails the test.
///
/// Every cause is `midi2` being stricter than the specification, laxer, or
/// wrong; `tutti-ump` follows the specification in each, and `spec_words.rs`
/// and `reserved.rs` pin that with words from the figures.
fn cause(w: [u32; 4], a: Option<&Fact>, b: Option<&Fact>) -> Option<&'static str> {
    let w0 = w[0];
    let byte = |word: u32, n: u32| (word >> (8 * n)) as u8;
    let mt = w0 >> 28;
    let kind = a.or(b).map_or("", |f| f.kind.as_str());
    let text_bytes: Vec<u8> = match (mt, kind) {
        (0xD, _) => w[1..].iter().flat_map(|x| x.to_be_bytes()).collect(),
        (0xF, "EndpointName" | "ProductInstanceId") => {
            let mut v = (w0 as u16).to_be_bytes().to_vec();
            v.extend(w[1..].iter().flat_map(|x| x.to_be_bytes()));
            v
        }
        (0xF, "FunctionBlockName") => {
            let mut v = vec![byte(w0, 0)];
            v.extend(w[1..].iter().flat_map(|x| x.to_be_bytes()));
            v
        }
        _ => Vec::new(),
    };
    let status = (w0 >> 20) & 0xf;
    let count = ((w0 >> 16) & 0xf) as usize;
    match (a, b) {
        (Some(a), Some(b)) if a.kind == b.kind => match a.kind.as_str() {
            // Figure 51: D is bit 1 and S bit 0. `midi2` has them the other way.
            "PerNoteManagement" => Some("midi2 swaps the Per-Note Management D and S flags"),
            // Figure 64 and Table 30: bank MSB is the high byte. `midi2` reads
            // them swapped.
            "cv2ProgramChange" if w0 & 1 == 1 => {
                Some("midi2 swaps the Program Change bank MSB and LSB")
            }
            _ => None,
        },
        // `midi2` accepts what the specification reserves.
        (None, Some(b)) => match b.kind.as_str() {
            "TimeCode" | "SongPositionPointer" | "SongSelect"
                if (w0 >> 15) & 1 == 1 || (w0 >> 7) & 1 == 1 =>
            {
                Some("midi2 accepts a System data byte with bit 7 set (drawn 0)")
            }
            "Sysex7"
                if count <= 6
                    && [
                        byte(w0, 1),
                        byte(w0, 0),
                        byte(w[1], 3),
                        byte(w[1], 2),
                        byte(w[1], 1),
                        byte(w[1], 0),
                    ][..count]
                        .iter()
                        .any(|b| b & 0x80 != 0) =>
            {
                Some("midi2 accepts a SysEx 7 data byte with bit 7 set (drawn 0)")
            }
            "EndpointInfo" if (w[1] >> 24) & 0x7f > 32 => {
                Some("midi2 accepts 33 to 127 Function Blocks (reserved)")
            }
            "FunctionBlockDiscovery" if byte(w0, 1) != 0xFF && byte(w0, 1) > 31 => {
                Some("midi2 accepts a Function Block number above 31 other than 0xFF")
            }
            "FunctionBlockInfo"
                if (byte(w0, 1) & 0x7f) > 31
                    || w0 & 3 == 0
                    || (w0 >> 2) & 3 == 3
                    || byte(w[1], 3) > 15
                    || !(1..=16).contains(&byte(w[1], 2)) =>
            {
                Some("midi2 accepts a reserved Function Block Info value")
            }
            "FunctionBlockName" if byte(w0, 1) > 31 => {
                Some("midi2 accepts a Function Block number above 31")
            }
            "StreamConfigurationRequest" | "StreamConfigurationNotification"
                if !matches!(byte(w0, 1), 1 | 2) =>
            {
                Some("midi2 accepts a reserved protocol value")
            }
            _ => None,
        },
        // `midi2` refuses what the specification allows.
        (Some(a), None) => match a.kind.as_str() {
            // midi2 decodes whole messages; the codec decodes packets.
            "Sysex7" | "Sysex8" if status != 0 => {
                Some("midi2 takes whole messages: it refuses a lone Start, Continue or End")
            }
            "MixedDataSet" => Some("midi2 has no Mixed Data Set"),
            // Table 15 allows -8 for the bass, "same as the tonic".
            // Table 14 defines 0x1B, 7 Suspended 4th. `midi2` stops at 0x1A.
            "SetChordName" if byte(w[1], 2) == 0x1B || byte(w[3], 2) == 0x1B => {
                Some("midi2 refuses chord type 0x1B (Table 14)")
            }
            "SetChordName" if w[3] >> 28 == 0x8 => {
                Some("midi2 refuses a chord bass of sharps/flats -8 (Table 15)")
            }
            _ if !text_bytes.is_empty()
                && (if mt == 0xD {
                    (w0 >> 22) & 3
                } else {
                    (w0 >> 26) & 3
                }) != 0 =>
            {
                Some("midi2 takes whole messages: it refuses a lone Start, Continue or End")
            }
            // Text is validated per packet. A character may span packets.
            _ if !text_bytes.is_empty()
                && (std::str::from_utf8(&text_bytes).is_err()
                    || (a.kind == "ProductInstanceId"
                        && text_bytes
                            .iter()
                            .any(|b| *b != 0 && !(32..=126).contains(b)))) =>
            {
                Some("midi2 validates UTF-8 or ASCII in one packet; the text is the reassembly")
            }
            _ => None,
        },
        _ => None,
    }
}

/// A disagreement, reduced to what it says: the family and what differs.
fn disagreement(w: [u32; 4]) -> Option<(String, Option<&'static str>)> {
    let ump = Ump(w);
    let a = decode(ump).ok().map(|m| ours(&m));
    let b = theirs(ump.words());
    let what = match (&a, &b) {
        (Some(a), Some(b)) if a == b => return None,
        (Some(a), Some(b)) if a.kind == b.kind => format!("{}: fields differ", a.kind),
        (Some(a), Some(b)) => format!("ours {} but midi2 {}", a.kind, b.kind),
        (Some(a), None) => format!("midi2 refuses {}", a.kind),
        (None, Some(b)) => format!("midi2 accepts {} we refuse", b.kind),
        (None, None) => return None,
    };
    Some((what, cause(w, a.as_ref(), b.as_ref())))
}

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Random packets, half with the message type forced to one that is defined,
/// through both decoders. Prints each distinct disagreement with a witness.
#[test]
fn random_packets_agree_with_midi2() {
    use std::collections::BTreeMap;
    let mut seen: BTreeMap<String, ([u32; 4], usize)> = BTreeMap::new();
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    for n in 0..2_000_000u32 {
        let r = xorshift(&mut s);
        let mut w = [
            r as u32,
            (r >> 32) as u32,
            xorshift(&mut s) as u32,
            xorshift(&mut s) as u32,
        ];
        if n % 3 == 2 {
            w = common::shape(w, xorshift(&mut s) as u8);
        } else if n % 3 == 0 {
            const MT: [u32; 8] = [0, 1, 2, 3, 4, 5, 0xD, 0xF];
            w[0] = MT[(xorshift(&mut s) % 8) as usize] << 28 | (w[0] & 0x0FFF_FFFF);
            // Bias the opcode of the channel voice types into the defined range
            // and the Stream and Flex words toward fewer reserved values.
            if w[0] >> 28 == 0xF {
                const ST: [u32; 12] = [0, 1, 2, 3, 4, 5, 6, 0x10, 0x11, 0x12, 0x20, 0x21];
                w[0] = (w[0] & 0xFC00_FFFF) | ST[(xorshift(&mut s) % 12) as usize] << 16;
            }
            if w[0] >> 28 == 0xD {
                w[0] = (w[0] & !0x0030_FFFF)
                    | 0x0010_0000
                    | ((xorshift(&mut s) % 3) as u32) << 8
                    | (xorshift(&mut s) % 7) as u32;
            }
        }
        if let Some((what, why)) = disagreement(w) {
            let key = format!("{what} | {}", why.unwrap_or("UNEXPLAINED"));
            seen.entry(key).and_modify(|e| e.1 += 1).or_insert((w, 1));
        }
    }
    let mut unexplained = Vec::new();
    for (key, (w, count)) in &seen {
        eprintln!("{key} x{count}: {w:08x?}");
        if key.ends_with("UNEXPLAINED") {
            unexplained.push(format!("{key}: {w:08x?}"));
        }
    }
    assert!(
        unexplained.is_empty(),
        "unsettled disagreements:\n{}",
        unexplained.join("\n")
    );
}
