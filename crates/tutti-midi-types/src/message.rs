//! [`MidiMessage`] — the app-facing decoded view of a [`Ump`].
//!
//! Modeled on `std::net::IpAddr`: a [`Ump`] is the wire form (a UMP packet
//! that may be MIDI 1.0 or 2.0 on the wire), and [`MidiMessage`] is the "just
//! tell me what it is" view an application matches on — the way `IpAddr` unifies
//! `Ipv4Addr`/`Ipv6Addr`. Like `IpAddr`, it carries the **common accessors**
//! ([`MidiMessage::note`], [`MidiMessage::velocity`], [`MidiMessage::channel`])
//! directly, so most code never has to `match` at all.
//!
//! Values are the MIDI 2.0 spec widths verbatim — 16-bit velocity, 32-bit
//! controllers/bend, per-note [`NoteId`] identity. No lossy narrowing happens
//! here; convert to `f32` at your DSP edge if you need to. Decode with
//! [`MidiMessage::from_ump`]; it first runs [`normalize`](crate::normalize()), so a MIDI 1.0
//! event is already promoted to its MIDI 2.0 form before you see it.
//!
//! System Real-Time / System Common messages (M2-104 §7.6 — UMP Message Type
//! 0x1: clock, transport start/stop/continue, MTC, song position/select) are
//! first-class variants. For the families this enum still does not model (SysEx,
//! Flex Data, UMP Stream, utility) [`MidiMessage::Other`] is returned; reach for
//! `ump.words()` + `midi2` when you need those.

use midi2::channel_voice2::ChannelVoice2 as Cv2;
use midi2::{Channeled, UmpMessage};
use tutti_ump::{
    Address, Bank, CCNumber, Message, MidiChannel, MidiGroup, NoteId, NoteNumber,
    RegisteredPerNoteController, System, U7, U14, Ump, Voice2, decode,
};

/// Which per-note controller a [`MidiMessage::PerNoteController`] carries.
/// Registered controllers name a spec-defined function; assignable ones carry a
/// raw index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerNoteController {
    /// A Registered Per-Note Controller identified by its spec bank/index.
    Registered {
        /// Spec-assigned controller number (M2-104 §7.4.5): 1 modulation,
        /// 2 breath, 3 pitch 7.25, 7 volume, 10 pan, 74 brightness.
        index: u8,
    },
    /// An Assignable Per-Note Controller identified by its raw index.
    Assignable {
        /// Device-defined controller number; carries no spec meaning.
        index: u8,
    },
}

/// Which channel-wide controller namespace a [`MidiMessage::RegisteredController`]
/// or [`MidiMessage::RelativeController`] addresses (M2-104 §7.4.7–7.4.8).
///
/// Registered (RPN) and Assignable (NRPN) get *one* discriminant rather than two
/// variants because they differ only in **which namespace** the `(bank, index)`
/// pair is looked up in — the wire shape, the data width, and everything a
/// consumer does with the value are identical. That is not two types by the
/// crate's own rule (a type needs a distinct range *or* a distinct algebra), and
/// it mirrors [`PerNoteController`], which made the same call for the per-note
/// pair. Contrast the absolute/relative split, which *is* two variants because
/// `u32` value and `i32` delta genuinely compose differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControllerNamespace {
    /// Registered Parameter Number — a spec-defined address (M2-104 §7.4.7).
    Registered,
    /// Assignable (non-registered) Parameter Number — a device-defined address.
    Assignable,
}

/// A MIDI 2.0 note-on attribute (M2-104 §7.4.2): extra per-note data carried
/// alongside the note. tutti's re-export of `midi2`'s attribute so consumers
/// don't import `midi2` to preserve it across a decode/re-encode round-trip.
pub type NoteAttribute = midi2::channel_voice2::NoteAttribute;

/// A decoded MIDI 2.0 message — tutti's application-facing view of a
/// [`Ump`]. See this module header. `#[non_exhaustive]` so added
/// message families never break an existing `match`.
///
/// No variant carries timing: a message in a block is `(Offset, Ump)` and the
/// caller keeps the offset beside the packet. Note variants carry the note
/// `attribute`. Unmodeled families keep their whole source packet in
/// [`Other`](Self::Other), so [`Ump::try_from`] can reconstruct any message
/// this view produced.
///
/// # Group
///
/// The view is **group-agnostic**: decoding drops the UMP group and
/// [`Ump::try_from`] re-encodes onto [`MidiGroup::FIRST`]. Only
/// [`Other`](Self::Other) keeps its packet, group included. A stream spanning
/// several groups must route on the [`Ump`] (its group is bits 24 to 27 of
/// word 0), not on this view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MidiMessage {
    /// Note on. `velocity` is the full 16-bit MIDI 2.0 value.
    NoteOn {
        /// Host-internal voice identity, distinct even between two live notes of
        /// the same number. Minted on decode; not carried on the wire.
        id: NoteId,
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// Note number, 0..=127.
        note: u8,
        /// **16-bit** velocity. A promoted MIDI 1.0 note upscales its 7 bits by
        /// Min-Center-Max, so `0x7F` becomes `0xFFFF` exactly; narrowing back
        /// with a shift is lossy and
        /// `tutti_midi_protocol::narrow_velocity` is the correct inverse.
        velocity: u16,
        /// MIDI 2.0 note attribute, preserved so a decode/re-encode round-trip
        /// does not drop it. `None` on a promoted MIDI 1.0 note.
        attribute: Option<NoteAttribute>,
    },
    /// Note off (also a MIDI 1.0 velocity-0 note-on, folded by `normalize`).
    NoteOff {
        /// Voice identity of the note being released.
        id: NoteId,
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// Note number, 0..=127.
        note: u8,
        /// **16-bit** release velocity. Zero for a folded velocity-0 MIDI 1.0 note-on.
        velocity: u16,
        /// MIDI 2.0 note attribute, if the source carried one.
        attribute: Option<NoteAttribute>,
    },
    /// Polyphonic key pressure (per-note aftertouch). `pressure` is 32-bit.
    PolyPressure {
        /// Voice identity the pressure applies to.
        id: NoteId,
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// Note number the pressure addresses, 0..=127.
        note: u8,
        /// **32-bit** unipolar pressure, full scale `u32::MAX`. A promoted MIDI
        /// 1.0 value occupies the whole range, not just its low 7 bits.
        pressure: u32,
    },
    /// Control change. `value` is the full 32-bit MIDI 2.0 value.
    ControlChange {
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// Controller number, 0..=127. See [`crate::cc`] for the named roster.
        index: u8,
        /// **32-bit** unipolar value, full scale `u32::MAX`. This is where the
        /// 7-bit/32-bit gap bites hardest: a consumer expecting CC's familiar
        /// 0..=127 reads full scale as `0xFFFFFFFF` and must downscale through
        /// `tutti_midi_protocol::narrow_value`, never by truncation.
        value: u32,
    },
    /// Program change, with an optional bank (MSB<<7 | LSB) when present.
    ProgramChange {
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// Program number, 0..=127.
        program: u8,
        /// 14-bit bank as `MSB << 7 | LSB`, or `None` when the message's bank
        /// valid bit is clear — which means "keep the current bank", not "bank 0".
        bank: Option<u16>,
    },
    /// Channel (mono) aftertouch. `pressure` is 32-bit.
    ChannelPressure {
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// **32-bit** unipolar pressure applying to every note on the channel.
        pressure: u32,
    },
    /// Channel pitch bend. `value` is 32-bit, bipolar around `0x8000_0000`.
    PitchBend {
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// **32-bit** bend, centre `0x8000_0000` — *not* zero. Treating this as
        /// unipolar puts a centred wheel at full positive bend. The semitone span
        /// it maps to is set out of band by RPN 0; see
        /// [`PitchBendSensitivity`](crate::PitchBendSensitivity).
        value: u32,
    },
    /// Per-note pitch bend (MIDI 2.0). Addresses one voice by `id`.
    PerNotePitchBend {
        /// Voice identity the bend applies to.
        id: NoteId,
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// Note number the bend addresses, 0..=127.
        note: u8,
        /// **32-bit** bend, centre `0x8000_0000`, scoped to this one note.
        value: u32,
    },
    /// Per-note controller (MIDI 2.0). `value` is 32-bit.
    PerNoteController {
        /// Voice identity the controller applies to.
        id: NoteId,
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// Note number the controller addresses, 0..=127.
        note: u8,
        /// Which controller, and in which of the two namespaces.
        controller: PerNoteController,
        /// **32-bit** unipolar value, full scale `u32::MAX`.
        value: u32,
    },
    /// Channel-wide Registered (RPN) or Assignable (NRPN) Controller, absolute
    /// form (M2-104 §7.4.7–7.4.8). `bank`/`index` are the 7-bit halves of the
    /// 14-bit parameter address; `data` is the full 32-bit value the parameter is
    /// **set to**.
    ///
    /// MIDI 2.0 gives these a dedicated Channel Voice 2 message, so no multi-CC
    /// running-status reassembly is needed — the address and the whole value
    /// arrive in one packet.
    RegisteredController {
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// Whether `bank`/`index` address the registered or assignable space.
        namespace: ControllerNamespace,
        /// High 7 bits of the 14-bit parameter address.
        bank: u8,
        /// Low 7 bits of the 14-bit parameter address.
        index: u8,
        /// **32-bit** value the parameter is set *to*, replacing what it held.
        data: u32,
    },
    /// Channel-wide Relative Registered/Assignable Controller (M2-104 §7.4.8):
    /// a signed *delta* applied to the parameter at `bank`/`index`, as an endless
    /// encoder produces.
    ///
    /// This is a **separate variant** from
    /// [`RegisteredController`](Self::RegisteredController) rather than a `bool`
    /// on it because the payloads have different algebra, not just a different
    /// name: the spec's data field here "contains a Two's Complement value", so
    /// it is an `i32` that *accumulates* onto the current value, where the
    /// absolute form's `u32` *replaces* it. Collapsing them would let a consumer
    /// read a decrement (`-1`) as `0xFFFF_FFFF` and slam the parameter to full
    /// scale. Per §7.4.8 these share the absolute form's address space and banks
    /// but "cannot be translated to the MIDI 1.0 Protocol".
    RelativeController {
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// Whether `bank`/`index` address the registered or assignable space.
        namespace: ControllerNamespace,
        /// High 7 bits of the 14-bit parameter address.
        bank: u8,
        /// Low 7 bits of the 14-bit parameter address.
        index: u8,
        /// Signed **32-bit** increment to *add* to the parameter's current value.
        /// The wire field is already two's-complement across the full width, so
        /// decoding is a reinterpretation and nothing is lost — but see the
        /// variant docs for what reading it back as unsigned would do.
        delta: i32,
    },
    /// Per-Note Management (MIDI 2.0): detach / reset the addressed note's
    /// controllers.
    PerNoteManagement {
        /// Voice identity being managed.
        id: NoteId,
        /// Channel, 0-indexed.
        channel: MidiChannel,
        /// Note number being managed, 0..=127.
        note: u8,
        /// Detach this note from its channel's controllers, so subsequent
        /// channel-wide messages stop affecting it.
        detach: bool,
        /// Reset this note's per-note controllers to their default values.
        reset: bool,
    },
    /// System Real-Time: timing clock (M2-104 §7.6, status 0xF8 — 24 clocks per
    /// quarter-note). Carries no channel or data.
    TimingClock,
    /// System Real-Time: transport start (M2-104 §7.6, status 0xFA — rewind to
    /// zero and play).
    Start,
    /// System Real-Time: transport continue (M2-104 §7.6, status 0xFB — play
    /// from the current position).
    Continue,
    /// System Real-Time: transport stop (M2-104 §7.6, status 0xFC).
    Stop,
    /// System Common: MIDI Time Code quarter-frame (M2-104 §7.6, status 0xF1).
    /// `code` is the 7-bit data byte: message type in bits 4..6, value in
    /// bits 0..3.
    TimeCode {
        /// The 7-bit quarter-frame byte. Feed it straight to
        /// [`MtcDecoder::feed`](crate::sync::MtcDecoder::feed), which owns the
        /// nibble reassembly.
        code: u8,
    },
    /// System Common: song position pointer (M2-104 §7.6, status 0xF2). 14-bit
    /// position in MIDI beats (1/16 notes) since song start.
    SongPosition {
        /// Position in sixteenth notes since song start, 0..=16383. Six MIDI
        /// clock ticks per unit — not a beat count.
        position: u16,
    },
    /// System Common: song select (M2-104 §7.6, status 0xF3). 7-bit song number.
    SongSelect {
        /// Song number, 0..=127.
        song: u8,
    },
    /// System Real-Time: active sensing (M2-104 §7.6, status 0xFE).
    ActiveSensing,
    /// System Real-Time: reset (M2-104 §7.6, status 0xFF).
    Reset,
    /// Any message family this view does not model (SysEx, Flex Data, UMP
    /// Stream, utility) — carries the whole source [`Ump`] so it is never
    /// information-free. Inspect it via `ump.words()` + `midi2`.
    Other(Ump),
}

impl MidiMessage {
    /// Note number, for note / poly-pressure / per-note messages.
    #[inline]
    pub fn note(&self) -> Option<u8> {
        match self {
            Self::NoteOn { note, .. }
            | Self::NoteOff { note, .. }
            | Self::PolyPressure { note, .. }
            | Self::PerNotePitchBend { note, .. }
            | Self::PerNoteController { note, .. }
            | Self::PerNoteManagement { note, .. } => Some(*note),
            _ => None,
        }
    }

    /// Per-note identity, for note / poly-pressure / per-note messages. Two
    /// simultaneous same-pitch notes have distinct ids.
    #[inline]
    pub fn id(&self) -> Option<NoteId> {
        match self {
            Self::NoteOn { id, .. }
            | Self::NoteOff { id, .. }
            | Self::PolyPressure { id, .. }
            | Self::PerNotePitchBend { id, .. }
            | Self::PerNoteController { id, .. }
            | Self::PerNoteManagement { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// Channel for any channel-voice message. `None` for [`Other`].
    ///
    /// [`Other`]: Self::Other
    #[inline]
    pub fn channel(&self) -> Option<MidiChannel> {
        match self {
            Self::NoteOn { channel, .. }
            | Self::NoteOff { channel, .. }
            | Self::PolyPressure { channel, .. }
            | Self::ControlChange { channel, .. }
            | Self::ProgramChange { channel, .. }
            | Self::ChannelPressure { channel, .. }
            | Self::PitchBend { channel, .. }
            | Self::PerNotePitchBend { channel, .. }
            | Self::PerNoteController { channel, .. }
            | Self::RegisteredController { channel, .. }
            | Self::RelativeController { channel, .. }
            | Self::PerNoteManagement { channel, .. } => Some(*channel),
            // System messages and `Other` carry no channel.
            _ => None,
        }
    }

    /// Velocity (full 16-bit) for note-on / note-off. `None` otherwise.
    #[inline]
    pub fn velocity(&self) -> Option<u16> {
        match self {
            Self::NoteOn { velocity, .. } | Self::NoteOff { velocity, .. } => Some(*velocity),
            _ => None,
        }
    }

    /// `true` for a note-on with non-zero velocity.
    #[inline]
    #[must_use]
    pub fn is_note_on(&self) -> bool {
        matches!(self, Self::NoteOn { velocity, .. } if *velocity > 0)
    }

    /// `true` for a note-off (including a folded velocity-0 note-on).
    #[inline]
    #[must_use]
    pub fn is_note_off(&self) -> bool {
        matches!(self, Self::NoteOff { .. }) || matches!(self, Self::NoteOn { velocity: 0, .. })
    }
}

impl MidiMessage {
    /// Decodes `ump` into the app-facing [`MidiMessage`] view. Runs [`normalize`](crate::normalize())
    /// first, so a MIDI 1.0 channel-voice packet arrives already promoted to its
    /// MIDI 2.0 form. Anything this view doesn't model yields
    /// [`MidiMessage::Other`] — use [`Ump::words`] + `midi2` for those.
    pub fn from_ump(ump: Ump) -> MidiMessage {
        let this = &ump;
        let ev = crate::normalize(this);
        let cv2 = match UmpMessage::try_from(ev.words()) {
            Ok(UmpMessage::ChannelVoice2(cv2)) => cv2,
            Ok(UmpMessage::SystemCommon(sc)) => {
                return system_common_message(sc).unwrap_or(MidiMessage::Other(*this));
            }
            // Preserve the *original* packet verbatim so a re-encode is exact.
            _ => return MidiMessage::Other(*this),
        };
        let channel = MidiChannel::masked(u8::from(cv2.channel()));
        let note_id = |note: u8| NoteId::from_channel_note(channel, NoteNumber::masked(note));

        match cv2 {
            Cv2::NoteOn(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::NoteOn {
                    id: note_id(note),
                    channel,
                    note,
                    velocity: m.velocity(),
                    attribute: m.attribute(),
                }
            }
            Cv2::NoteOff(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::NoteOff {
                    id: note_id(note),
                    channel,
                    note,
                    velocity: m.velocity(),
                    attribute: m.attribute(),
                }
            }
            Cv2::KeyPressure(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::PolyPressure {
                    id: note_id(note),
                    channel,
                    note,
                    pressure: m.key_pressure_data(),
                }
            }
            Cv2::ControlChange(m) => MidiMessage::ControlChange {
                channel,
                index: u8::from(m.control()),
                value: m.control_change_data(),
            },
            Cv2::ProgramChange(m) => MidiMessage::ProgramChange {
                channel,
                program: u8::from(m.program()),
                bank: crate::ump::program_change_bank_of(ev.words()),
            },
            Cv2::ChannelPressure(m) => MidiMessage::ChannelPressure {
                channel,
                pressure: m.channel_pressure_data(),
            },
            Cv2::ChannelPitchBend(m) => MidiMessage::PitchBend {
                channel,
                value: m.pitch_bend_data(),
            },
            Cv2::PerNotePitchBend(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::PerNotePitchBend {
                    id: note_id(note),
                    channel,
                    note,
                    value: m.pitch_bend_data(),
                }
            }
            Cv2::RegisteredPerNoteController(m) => {
                let note = u8::from(m.note_number());
                let (index, value) = controller_index_and_data(m.controller());
                MidiMessage::PerNoteController {
                    id: note_id(note),
                    channel,
                    note,
                    controller: PerNoteController::Registered { index },
                    value,
                }
            }
            Cv2::AssignablePerNoteController(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::PerNoteController {
                    id: note_id(note),
                    channel,
                    note,
                    controller: PerNoteController::Assignable { index: m.index() },
                    value: m.controller_data(),
                }
            }
            // The four channel-wide controller messages (M2-104 §7.4.7–7.4.8).
            // All four carry the same `(bank, index)` address; only the
            // namespace and the absolute/relative reading of the data differ.
            Cv2::RegisteredController(m) => MidiMessage::RegisteredController {
                channel,
                namespace: ControllerNamespace::Registered,
                bank: u8::from(m.bank()),
                index: u8::from(m.index()),
                data: m.controller_data(),
            },
            Cv2::AssignableController(m) => MidiMessage::RegisteredController {
                channel,
                namespace: ControllerNamespace::Assignable,
                bank: u8::from(m.bank()),
                index: u8::from(m.index()),
                data: m.controller_data(),
            },
            // `as i32` is the two's-complement reinterpretation the spec asks
            // for, not a lossy cast: the wire field *is* a signed value, so a
            // decrement must read as `-1` rather than `0xFFFF_FFFF`.
            Cv2::RelativeRegisteredController(m) => MidiMessage::RelativeController {
                channel,
                namespace: ControllerNamespace::Registered,
                bank: u8::from(m.bank()),
                index: u8::from(m.index()),
                delta: m.controller_data() as i32,
            },
            Cv2::RelativeAssignableController(m) => MidiMessage::RelativeController {
                channel,
                namespace: ControllerNamespace::Assignable,
                bank: u8::from(m.bank()),
                index: u8::from(m.index()),
                delta: m.controller_data() as i32,
            },
            // `midi2` reads Detach and Reset from the wrong bits (M2-104
            // Figure 51 has D in bit 1 and S in bit 0), so the flags come from
            // `tutti_ump`.
            Cv2::PerNoteManagement(m) => {
                let note = u8::from(m.note_number());
                let Ok(Message::Midi2(_, Voice2::PerNoteManagement { detach, reset, .. })) =
                    decode(ev)
                else {
                    return MidiMessage::Other(*this);
                };
                MidiMessage::PerNoteManagement {
                    id: note_id(note),
                    channel,
                    note,
                    detach,
                    reset,
                }
            }
            // Every Channel Voice 2 message `midi2` currently defines now has a
            // modeled variant, so this arm is unreachable *today* — but `Cv2` is
            // `#[non_exhaustive]`, so it must stay: it is what keeps a future
            // upstream variant compiling (as `Other`) instead of breaking the
            // build. Hence the allow rather than deleting the arm.
            #[allow(
                unreachable_patterns,
                reason = "`Cv2` is #[non_exhaustive]; the arm is unreachable only until upstream adds a variant"
            )]
            _ => MidiMessage::Other(*this),
        }
    }
}

/// A [`MidiMessage`] variant that this engine cannot re-encode to a
/// [`Ump`]. Every variant `MidiMessage::from_ump` produces *can* be
/// re-encoded, so this only arises for hand-built messages using a per-note
/// controller index or program bank outside the encodable range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnencodableMessage;

impl core::fmt::Display for UnencodableMessage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MidiMessage cannot be encoded to a Ump")
    }
}

impl std::error::Error for UnencodableMessage {}

/// A 7-bit field from a raw byte, masked (`& 0x7F`) as the wire does.
fn u7(raw: u8) -> U7 {
    U7::new(raw & 0x7F).unwrap_or_default()
}

/// A note number from a raw byte, masked (`& 0x7F`) as the wire does.
fn note_number(raw: u8) -> NoteNumber {
    NoteNumber::new(raw & 0x7F).unwrap_or_default()
}

impl TryFrom<MidiMessage> for Ump {
    type Error = UnencodableMessage;

    /// Re-encode a [`MidiMessage`] back to its wire [`Ump`] (and note attribute
    /// where present). Every message produced by [`MidiMessage::from_ump`]
    /// round-trips; the error case only occurs for hand-built controller/bank
    /// values with no MIDI-2 encoding.
    fn try_from(msg: MidiMessage) -> Result<Self, Self::Error> {
        let group = MidiGroup::FIRST;
        let voice = |channel: MidiChannel, v: Voice2| -> Ump {
            Message::Midi2(Address { group, channel }, v).into()
        };
        let system = |s: System| -> Ump { Message::System(group, s).into() };
        let ump = match msg {
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
                attribute,
                ..
            } => note_with_attribute(true, channel, note, velocity, attribute),
            MidiMessage::NoteOff {
                channel,
                note,
                velocity,
                attribute,
                ..
            } => note_with_attribute(false, channel, note, velocity, attribute),
            MidiMessage::PolyPressure {
                channel,
                note,
                pressure,
                ..
            } => voice(
                channel,
                Voice2::PolyPressure {
                    note: note_number(note),
                    value: pressure,
                },
            ),
            MidiMessage::ControlChange {
                channel,
                index,
                value,
                ..
            } => voice(
                channel,
                Voice2::ControlChange {
                    controller: CCNumber::masked(index),
                    value,
                },
            ),
            MidiMessage::ProgramChange {
                channel,
                program,
                bank,
                ..
            } => voice(
                channel,
                Voice2::ProgramChange {
                    program: u7(program),
                    bank: bank.map(|b| Bank {
                        msb: u7((b >> 7) as u8),
                        lsb: u7(b as u8),
                    }),
                },
            ),
            MidiMessage::ChannelPressure {
                channel, pressure, ..
            } => voice(channel, Voice2::ChannelPressure(pressure)),
            MidiMessage::PitchBend { channel, value, .. } => {
                voice(channel, Voice2::PitchBend(value))
            }
            MidiMessage::PerNotePitchBend {
                channel,
                note,
                value,
                ..
            } => voice(
                channel,
                Voice2::PerNotePitchBend {
                    note: note_number(note),
                    value,
                },
            ),
            MidiMessage::PerNoteController {
                channel,
                note,
                controller,
                value,
                ..
            } => voice(
                channel,
                match controller {
                    PerNoteController::Registered { index } => Voice2::RegisteredPerNote {
                        note: note_number(note),
                        controller: RegisteredPerNoteController::from(index),
                        value,
                    },
                    PerNoteController::Assignable { index } => Voice2::AssignablePerNote {
                        note: note_number(note),
                        controller: index,
                        value,
                    },
                },
            ),
            // The namespace picks the message; the absolute/relative split is
            // already carried by the variant itself.
            MidiMessage::RegisteredController {
                channel,
                namespace,
                bank,
                index,
                data,
                ..
            } => voice(
                channel,
                match namespace {
                    ControllerNamespace::Registered => Voice2::Registered {
                        bank: u7(bank),
                        index: u7(index),
                        value: data,
                    },
                    ControllerNamespace::Assignable => Voice2::Assignable {
                        bank: u7(bank),
                        index: u7(index),
                        value: data,
                    },
                },
            ),
            MidiMessage::RelativeController {
                channel,
                namespace,
                bank,
                index,
                delta,
                ..
            } => voice(
                channel,
                match namespace {
                    ControllerNamespace::Registered => Voice2::RelativeRegistered {
                        bank: u7(bank),
                        index: u7(index),
                        delta,
                    },
                    ControllerNamespace::Assignable => Voice2::RelativeAssignable {
                        bank: u7(bank),
                        index: u7(index),
                        delta,
                    },
                },
            ),
            MidiMessage::PerNoteManagement {
                channel,
                note,
                detach,
                reset,
                ..
            } => voice(
                channel,
                Voice2::PerNoteManagement {
                    note: note_number(note),
                    detach,
                    reset,
                },
            ),
            MidiMessage::TimingClock => system(System::TimingClock),
            MidiMessage::Start => system(System::Start),
            MidiMessage::Continue => system(System::Continue),
            MidiMessage::Stop => system(System::Stop),
            MidiMessage::TimeCode { code } => system(System::TimeCode(u7(code))),
            MidiMessage::SongPosition { position } => system(System::SongPosition(
                U14::new(position & 0x3FFF).unwrap_or_default(),
            )),
            MidiMessage::SongSelect { song } => system(System::SongSelect(u7(song))),
            MidiMessage::ActiveSensing => system(System::ActiveSensing),
            MidiMessage::Reset => system(System::Reset),
            // The source packet was preserved verbatim.
            MidiMessage::Other(ump) => ump,
        };
        Ok(ump)
    }
}

/// Build a note-on/off [`Ump`], re-applying a note `attribute` if present.
fn note_with_attribute(
    on: bool,
    channel: MidiChannel,
    note: u8,
    velocity: u16,
    attribute: Option<NoteAttribute>,
) -> Ump {
    use midi2::channel_voice2::{NoteOff, NoteOn};
    use midi2::prelude::*;
    let mut words = [0u32; 4];
    if on {
        let mut m = NoteOn::<[u32; 2]>::new();
        m.set_channel(u4::new(channel.get()));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_velocity(velocity);
        if let Some(attr) = attribute {
            m.set_attribute(Some(attr));
        }
        words[..2].copy_from_slice(m.data());
    } else {
        let mut m = NoteOff::<[u32; 2]>::new();
        m.set_channel(u4::new(channel.get()));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_velocity(velocity);
        if let Some(attr) = attribute {
            m.set_attribute(Some(attr));
        }
        words[..2].copy_from_slice(m.data());
    }
    Ump(words)
}

/// Split a registered per-note `Controller` into its spec index and 32-bit data
/// (M2-104 §7.4.10). Mirrors midi2's own index assignment.
fn controller_index_and_data(c: midi2::channel_voice2::Controller) -> (u8, u32) {
    use midi2::channel_voice2::Controller;
    match c {
        Controller::Modulation(d) => (1, d),
        Controller::Breath(d) => (2, d),
        Controller::Pitch7_25(v) => (3, v.to_bits()),
        Controller::Volume(d) => (7, d),
        Controller::Balance(d) => (8, d),
        Controller::Pan(d) => (10, d),
        Controller::Expression(d) => (11, d),
        Controller::SoundVariation(d) => (70, d),
        Controller::Timbre(d) => (71, d),
        Controller::ReleaseTime(d) => (72, d),
        Controller::AttackTime(d) => (73, d),
        Controller::Brightness(d) => (74, d),
        Controller::DecayTime(d) => (75, d),
        Controller::VebratoRate(d) => (76, d),
        Controller::VebratoDepth(d) => (77, d),
        Controller::VebratoDelay(d) => (78, d),
        Controller::ReverbSendLevel(d) => (91, d),
        Controller::ChorusSendLevel(d) => (93, d),
        Controller::SoundController { index, data } => (69 + index, data),
        Controller::EffectDepth { index, data } => (90 + index, data),
        Controller::Undefined(d) => (0, d),
        // `Controller` is #[non_exhaustive]; a future variant surfaces as index 0.
        _ => (0, 0),
    }
}

/// Map a decoded System Real-Time / System Common message to its [`MidiMessage`]
/// variant (M2-104 §7.6). Returns `None` for the families this view still leaves
/// in [`MidiMessage::Other`] (currently just Tune Request), so the caller
/// preserves the original event verbatim.
fn system_common_message(sc: midi2::system_common::SystemCommon<&[u32]>) -> Option<MidiMessage> {
    use midi2::system_common::SystemCommon as Sc;
    Some(match sc {
        Sc::TimingClock(_) => MidiMessage::TimingClock,
        Sc::Start(_) => MidiMessage::Start,
        Sc::Continue(_) => MidiMessage::Continue,
        Sc::Stop(_) => MidiMessage::Stop,
        Sc::TimeCode(m) => MidiMessage::TimeCode {
            code: u8::from(m.time_code()),
        },
        Sc::SongPositionPointer(m) => MidiMessage::SongPosition {
            position: u16::from(m.position()),
        },
        Sc::SongSelect(m) => MidiMessage::SongSelect {
            song: u8::from(m.song()),
        },
        Sc::ActiveSensing(_) => MidiMessage::ActiveSensing,
        Sc::Reset(_) => MidiMessage::Reset,
        // Tune Request has no modeled variant; the caller keeps it in `Other`.
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
