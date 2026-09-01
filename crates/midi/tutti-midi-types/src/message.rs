//! [`MidiMessage`] — the app-facing decoded view of a [`MidiEvent`].
//!
//! Modeled on `std::net::IpAddr`: a [`MidiEvent`] is the wire form (a UMP packet
//! that may be MIDI 1.0 or 2.0 on the wire), and [`MidiMessage`] is the "just
//! tell me what it is" view an application matches on — the way `IpAddr` unifies
//! `Ipv4Addr`/`Ipv6Addr`. Like `IpAddr`, it carries the **common accessors**
//! ([`MidiMessage::note`], [`MidiMessage::velocity`], [`MidiMessage::channel`])
//! directly, so most code never has to `match` at all.
//!
//! Values are the MIDI 2.0 spec widths verbatim — 16-bit velocity, 32-bit
//! controllers/bend, per-note [`NoteId`] identity. No lossy narrowing happens
//! here; convert to `f32` at your DSP edge if you need to. Decode with
//! [`MidiEvent::message`]; it first runs [`normalize`](crate::normalize()), so a MIDI 1.0
//! event is already promoted to its MIDI 2.0 form before you see it.
//!
//! System Real-Time / System Common messages (M2-104 §7.6 — UMP Message Type
//! 0x1: clock, transport start/stop/continue, MTC, song position/select) are
//! first-class variants. For the families this enum still does not model (SysEx,
//! Flex Data, UMP Stream, utility) [`MidiMessage::Other`] is returned; reach for
//! `event.data_words()` + `midi2` when you need those.

use midi2::channel_voice2::ChannelVoice2 as Cv2;
use midi2::{Channeled, UmpMessage};
use tutti_types::{CCNumber, MidiChannel, MidiGroup};

use crate::note_id::NoteId;
use crate::ump::MidiEvent;

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
/// [`MidiEvent`]. See this module header. `#[non_exhaustive]` so added
/// message families never break an existing `match`.
///
/// Every variant carries `frame_offset` (the source event's sample-accurate
/// timing) so a decode → re-encode round-trip preserves timing; note variants
/// carry the note `attribute`. Unmodeled families keep their whole source event
/// in [`Other`](Self::Other), so [`MidiEvent::try_from`] can reconstruct any
/// message this view produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MidiMessage {
    /// Note on. `velocity` is the full 16-bit MIDI 2.0 value.
    NoteOn {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Host-internal voice identity, distinct even between two live notes of
        /// the same number. Minted on decode; not carried on the wire.
        id: NoteId,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
        /// Note number, 0..=127.
        note: u8,
        /// **16-bit** velocity. A promoted MIDI 1.0 note upscales its 7 bits by
        /// Min-Center-Max, so `0x7F` becomes `0xFFFF` exactly; narrowing back
        /// with a shift is lossy and
        /// [`MidiEvent::velocity_u7`](crate::MidiEvent::velocity_u7) is the
        /// correct inverse.
        velocity: u16,
        /// MIDI 2.0 note attribute, preserved so a decode/re-encode round-trip
        /// does not drop it. `None` on a promoted MIDI 1.0 note.
        attribute: Option<NoteAttribute>,
    },
    /// Note off (also a MIDI 1.0 velocity-0 note-on, folded by `normalize`).
    NoteOff {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Voice identity of the note being released.
        id: NoteId,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
        /// Note number, 0..=127.
        note: u8,
        /// **16-bit** release velocity. Zero for a folded MIDI 1.0 note-off.
        velocity: u16,
        /// MIDI 2.0 note attribute, if the source carried one.
        attribute: Option<NoteAttribute>,
    },
    /// Polyphonic key pressure (per-note aftertouch). `pressure` is 32-bit.
    PolyPressure {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Voice identity the pressure applies to.
        id: NoteId,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
        /// Note number the pressure addresses, 0..=127.
        note: u8,
        /// **32-bit** unipolar pressure, full scale `u32::MAX`. A promoted MIDI
        /// 1.0 value occupies the whole range, not just its low 7 bits.
        pressure: u32,
    },
    /// Control change. `value` is the full 32-bit MIDI 2.0 value.
    ControlChange {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
        /// Controller number, 0..=127. See [`crate::cc`] for the named roster.
        index: u8,
        /// **32-bit** unipolar value, full scale `u32::MAX`. This is where the
        /// 7-bit/32-bit gap bites hardest: a consumer expecting CC's familiar
        /// 0..=127 reads full scale as `0xFFFFFFFF` and must downscale through
        /// [`crate::convert`], never by truncation.
        value: u32,
    },
    /// Program change, with an optional bank (MSB<<7 | LSB) when present.
    ProgramChange {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
        /// Program number, 0..=127.
        program: u8,
        /// 14-bit bank as `MSB << 7 | LSB`, or `None` when the message's bank
        /// valid bit is clear — which means "keep the current bank", not "bank 0".
        bank: Option<u16>,
    },
    /// Channel (mono) aftertouch. `pressure` is 32-bit.
    ChannelPressure {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
        /// **32-bit** unipolar pressure applying to every note on the channel.
        pressure: u32,
    },
    /// Channel pitch bend. `value` is 32-bit, bipolar around `0x8000_0000`.
    PitchBend {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
        /// **32-bit** bend, centre `0x8000_0000` — *not* zero. Treating this as
        /// unipolar puts a centred wheel at full positive bend. The semitone span
        /// it maps to is set out of band by RPN 0; see
        /// [`PitchBendSensitivity`](crate::PitchBendSensitivity).
        value: u32,
    },
    /// Per-note pitch bend (MIDI 2.0). Addresses one voice by `id`.
    PerNotePitchBend {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Voice identity the bend applies to.
        id: NoteId,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
        /// Note number the bend addresses, 0..=127.
        note: u8,
        /// **32-bit** bend, centre `0x8000_0000`, scoped to this one note.
        value: u32,
    },
    /// Per-note controller (MIDI 2.0). `value` is 32-bit.
    PerNoteController {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Voice identity the controller applies to.
        id: NoteId,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
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
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
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
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
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
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Voice identity being managed.
        id: NoteId,
        /// Channel, 0-indexed (0..=15).
        channel: u8,
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
    TimingClock {
        /// Sample offset within the block this tick lands on.
        frame_offset: u32,
    },
    /// System Real-Time: transport start (M2-104 §7.6, status 0xFA — rewind to
    /// zero and play).
    Start {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
    },
    /// System Real-Time: transport continue (M2-104 §7.6, status 0xFB — play
    /// from the current position).
    Continue {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
    },
    /// System Real-Time: transport stop (M2-104 §7.6, status 0xFC).
    Stop {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
    },
    /// System Common: MIDI Time Code quarter-frame (M2-104 §7.6, status 0xF1).
    /// `code` is the 7-bit data byte: message type in bits 4..6, value in
    /// bits 0..3.
    TimeCode {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// The 7-bit quarter-frame byte. Feed it straight to
        /// [`MtcDecoder::feed`](crate::sync::MtcDecoder::feed), which owns the
        /// nibble reassembly.
        code: u8,
    },
    /// System Common: song position pointer (M2-104 §7.6, status 0xF2). 14-bit
    /// position in MIDI beats (1/16 notes) since song start.
    SongPosition {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Position in sixteenth notes since song start, 0..=16383. Six MIDI
        /// clock ticks per unit — not a beat count.
        position: u16,
    },
    /// System Common: song select (M2-104 §7.6, status 0xF3). 7-bit song number.
    SongSelect {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
        /// Song number, 0..=127.
        song: u8,
    },
    /// System Real-Time: active sensing (M2-104 §7.6, status 0xFE).
    ActiveSensing {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
    },
    /// System Real-Time: reset (M2-104 §7.6, status 0xFF).
    Reset {
        /// Sample offset within the block this event lands on.
        frame_offset: u32,
    },
    /// Any message family this view does not model (SysEx, Flex Data, UMP
    /// Stream, utility) — carries the whole source [`MidiEvent`] so it is never
    /// information-free. Inspect it via `event.data_words()` + `midi2`.
    Other(MidiEvent),
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

    /// Channel (0-15) for any channel-voice message. `None` for [`Other`].
    ///
    /// [`Other`]: Self::Other
    #[inline]
    pub fn channel(&self) -> Option<u8> {
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

    /// Sample-accurate offset within the current audio block, for every message.
    #[inline]
    pub fn frame_offset(&self) -> u32 {
        match self {
            Self::NoteOn { frame_offset, .. }
            | Self::NoteOff { frame_offset, .. }
            | Self::PolyPressure { frame_offset, .. }
            | Self::ControlChange { frame_offset, .. }
            | Self::ProgramChange { frame_offset, .. }
            | Self::ChannelPressure { frame_offset, .. }
            | Self::PitchBend { frame_offset, .. }
            | Self::PerNotePitchBend { frame_offset, .. }
            | Self::PerNoteController { frame_offset, .. }
            | Self::RegisteredController { frame_offset, .. }
            | Self::RelativeController { frame_offset, .. }
            | Self::PerNoteManagement { frame_offset, .. }
            | Self::TimingClock { frame_offset }
            | Self::Start { frame_offset }
            | Self::Continue { frame_offset }
            | Self::Stop { frame_offset }
            | Self::TimeCode { frame_offset, .. }
            | Self::SongPosition { frame_offset, .. }
            | Self::SongSelect { frame_offset, .. }
            | Self::ActiveSensing { frame_offset }
            | Self::Reset { frame_offset } => *frame_offset,
            Self::Other(ev) => ev.frame_offset,
        }
    }

    /// `true` for a note-on with non-zero velocity.
    #[inline]
    pub fn is_note_on(&self) -> bool {
        matches!(self, Self::NoteOn { velocity, .. } if *velocity > 0)
    }

    /// `true` for a note-off (including a folded velocity-0 note-on).
    #[inline]
    pub fn is_note_off(&self) -> bool {
        matches!(self, Self::NoteOff { .. }) || matches!(self, Self::NoteOn { velocity: 0, .. })
    }
}

impl MidiEvent {
    /// Decode into the app-facing [`MidiMessage`] view. Runs [`normalize`](crate::normalize())
    /// first, so a MIDI 1.0 channel-voice event arrives already promoted to its
    /// MIDI 2.0 form. Anything this view doesn't model yields
    /// [`MidiMessage::Other`] — use [`data_words`](Self::data_words) + `midi2`
    /// for those.
    pub fn message(&self) -> MidiMessage {
        let ev = crate::normalize(self);
        let frame_offset = ev.frame_offset;
        let cv2 = match UmpMessage::try_from(ev.data_words()) {
            Ok(UmpMessage::ChannelVoice2(cv2)) => cv2,
            Ok(UmpMessage::SystemCommon(sc)) => {
                return system_common_message(sc, frame_offset).unwrap_or(MidiMessage::Other(*self))
            }
            // Preserve the *original* event verbatim so a re-encode is exact.
            _ => return MidiMessage::Other(*self),
        };
        let channel = u8::from(cv2.channel());
        let note_id = |note: u8| NoteId::from_channel_note(channel, note);

        match cv2 {
            Cv2::NoteOn(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::NoteOn {
                    frame_offset,
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
                    frame_offset,
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
                    frame_offset,
                    id: note_id(note),
                    channel,
                    note,
                    pressure: m.key_pressure_data(),
                }
            }
            Cv2::ControlChange(m) => MidiMessage::ControlChange {
                frame_offset,
                channel,
                index: u8::from(m.control()),
                value: m.control_change_data(),
            },
            Cv2::ProgramChange(m) => MidiMessage::ProgramChange {
                frame_offset,
                channel,
                program: u8::from(m.program()),
                bank: m.bank().map(u16::from),
            },
            Cv2::ChannelPressure(m) => MidiMessage::ChannelPressure {
                frame_offset,
                channel,
                pressure: m.channel_pressure_data(),
            },
            Cv2::ChannelPitchBend(m) => MidiMessage::PitchBend {
                frame_offset,
                channel,
                value: m.pitch_bend_data(),
            },
            Cv2::PerNotePitchBend(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::PerNotePitchBend {
                    frame_offset,
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
                    frame_offset,
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
                    frame_offset,
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
                frame_offset,
                channel,
                namespace: ControllerNamespace::Registered,
                bank: u8::from(m.bank()),
                index: u8::from(m.index()),
                data: m.controller_data(),
            },
            Cv2::AssignableController(m) => MidiMessage::RegisteredController {
                frame_offset,
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
                frame_offset,
                channel,
                namespace: ControllerNamespace::Registered,
                bank: u8::from(m.bank()),
                index: u8::from(m.index()),
                delta: m.controller_data() as i32,
            },
            Cv2::RelativeAssignableController(m) => MidiMessage::RelativeController {
                frame_offset,
                channel,
                namespace: ControllerNamespace::Assignable,
                bank: u8::from(m.bank()),
                index: u8::from(m.index()),
                delta: m.controller_data() as i32,
            },
            Cv2::PerNoteManagement(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::PerNoteManagement {
                    frame_offset,
                    id: note_id(note),
                    channel,
                    note,
                    detach: m.detach(),
                    reset: m.reset(),
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
            _ => MidiMessage::Other(*self),
        }
    }
}

/// A [`MidiMessage`] variant that this engine cannot re-encode to a
/// [`MidiEvent`]. Every variant `MidiEvent::message` produces *can* be
/// re-encoded, so this only arises for hand-built messages using a per-note
/// controller index or program bank outside the encodable range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnencodableMessage;

impl core::fmt::Display for UnencodableMessage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MidiMessage cannot be encoded to a MidiEvent")
    }
}

impl std::error::Error for UnencodableMessage {}

impl TryFrom<MidiMessage> for MidiEvent {
    type Error = UnencodableMessage;

    /// Re-encode a [`MidiMessage`] back to its wire [`MidiEvent`], preserving the
    /// frame offset (and note attribute where present). Every message produced by
    /// [`MidiEvent::message`] round-trips; the error case only occurs for
    /// hand-built controller/bank values with no MIDI-2 encoding.
    fn try_from(msg: MidiMessage) -> Result<Self, Self::Error> {
        let ev = match msg {
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
                attribute,
                ..
            } => note_with_attribute(true, MidiChannel::new(channel), note, velocity, attribute),
            MidiMessage::NoteOff {
                channel,
                note,
                velocity,
                attribute,
                ..
            } => note_with_attribute(false, MidiChannel::new(channel), note, velocity, attribute),
            MidiMessage::PolyPressure {
                channel,
                note,
                pressure,
                ..
            } => MidiEvent::poly_pressure(
                MidiGroup::FIRST,
                MidiChannel::new(channel),
                note,
                pressure,
            ),
            MidiMessage::ControlChange {
                channel,
                index,
                value,
                ..
            } => MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::new(channel),
                CCNumber::new(index),
                value,
            ),
            MidiMessage::ProgramChange {
                channel,
                program,
                bank,
                ..
            } => MidiEvent::program_change(
                MidiGroup::FIRST,
                MidiChannel::new(channel),
                program,
                bank,
            ),
            MidiMessage::ChannelPressure {
                channel, pressure, ..
            } => MidiEvent::channel_pressure(MidiGroup::FIRST, MidiChannel::new(channel), pressure),
            MidiMessage::PitchBend { channel, value, .. } => {
                MidiEvent::pitch_bend(MidiGroup::FIRST, MidiChannel::new(channel), value)
            }
            MidiMessage::PerNotePitchBend {
                channel,
                note,
                value,
                ..
            } => MidiEvent::per_note_pitch_bend(
                MidiGroup::FIRST,
                MidiChannel::new(channel),
                note,
                value,
            ),
            MidiMessage::PerNoteController {
                channel,
                note,
                controller,
                value,
                ..
            } => {
                let (index, registered) = match controller {
                    PerNoteController::Registered { index } => (index, true),
                    PerNoteController::Assignable { index } => (index, false),
                };
                MidiEvent::per_note_controller(
                    MidiGroup::FIRST,
                    MidiChannel::new(channel),
                    note,
                    index,
                    value,
                    registered,
                )
            }
            // The namespace picks the constructor; the absolute/relative split
            // is already carried by the variant itself.
            MidiMessage::RegisteredController {
                channel,
                namespace,
                bank,
                index,
                data,
                ..
            } => {
                let build = match namespace {
                    ControllerNamespace::Registered => MidiEvent::registered_controller,
                    ControllerNamespace::Assignable => MidiEvent::assignable_controller,
                };
                build(
                    MidiGroup::FIRST,
                    MidiChannel::new(channel),
                    bank,
                    index,
                    data,
                )
            }
            MidiMessage::RelativeController {
                channel,
                namespace,
                bank,
                index,
                delta,
                ..
            } => {
                let build = match namespace {
                    ControllerNamespace::Registered => MidiEvent::relative_registered_controller,
                    ControllerNamespace::Assignable => MidiEvent::relative_assignable_controller,
                };
                build(
                    MidiGroup::FIRST,
                    MidiChannel::new(channel),
                    bank,
                    index,
                    delta,
                )
            }
            MidiMessage::PerNoteManagement {
                channel,
                note,
                detach,
                reset,
                ..
            } => MidiEvent::per_note_management(
                MidiGroup::FIRST,
                MidiChannel::new(channel),
                note,
                detach,
                reset,
            ),
            MidiMessage::TimingClock { .. } => MidiEvent::timing_clock(MidiGroup::FIRST),
            MidiMessage::Start { .. } => MidiEvent::start(MidiGroup::FIRST),
            MidiMessage::Continue { .. } => MidiEvent::continue_msg(MidiGroup::FIRST),
            MidiMessage::Stop { .. } => MidiEvent::stop(MidiGroup::FIRST),
            MidiMessage::TimeCode { code, .. } => {
                MidiEvent::mtc_quarter_frame(MidiGroup::FIRST, code)
            }
            MidiMessage::SongPosition { position, .. } => {
                MidiEvent::song_position(MidiGroup::FIRST, position)
            }
            MidiMessage::SongSelect { song, .. } => MidiEvent::song_select(MidiGroup::FIRST, song),
            MidiMessage::ActiveSensing { .. } => MidiEvent::active_sensing(MidiGroup::FIRST),
            MidiMessage::Reset { .. } => MidiEvent::system_reset(MidiGroup::FIRST),
            // The source event was preserved verbatim.
            MidiMessage::Other(ev) => return Ok(ev),
        };
        Ok(ev.with_frame_offset(msg.frame_offset()))
    }
}

/// Build a note-on/off [`MidiEvent`], re-applying a note `attribute` if present.
fn note_with_attribute(
    on: bool,
    channel: MidiChannel,
    note: u8,
    velocity: u16,
    attribute: Option<NoteAttribute>,
) -> MidiEvent {
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
    MidiEvent::from_ump(0, &words[..2])
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
fn system_common_message(
    sc: midi2::system_common::SystemCommon<&[u32]>,
    frame_offset: u32,
) -> Option<MidiMessage> {
    use midi2::system_common::SystemCommon as Sc;
    Some(match sc {
        Sc::TimingClock(_) => MidiMessage::TimingClock { frame_offset },
        Sc::Start(_) => MidiMessage::Start { frame_offset },
        Sc::Continue(_) => MidiMessage::Continue { frame_offset },
        Sc::Stop(_) => MidiMessage::Stop { frame_offset },
        Sc::TimeCode(m) => MidiMessage::TimeCode {
            frame_offset,
            code: u8::from(m.time_code()),
        },
        Sc::SongPositionPointer(m) => MidiMessage::SongPosition {
            frame_offset,
            position: u16::from(m.position()),
        },
        Sc::SongSelect(m) => MidiMessage::SongSelect {
            frame_offset,
            song: u8::from(m.song()),
        },
        Sc::ActiveSensing(_) => MidiMessage::ActiveSensing { frame_offset },
        Sc::Reset(_) => MidiMessage::Reset { frame_offset },
        // Tune Request has no modeled variant; the caller keeps it in `Other`.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_on_decodes_full_width() {
        let msg = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(3), 60, 0xC000).message();
        assert!(msg.is_note_on());
        assert_eq!(msg.note(), Some(60));
        assert_eq!(msg.channel(), Some(3));
        assert_eq!(msg.velocity(), Some(0xC000)); // full 16-bit, not narrowed
    }

    #[test]
    fn midi1_note_on_promotes_and_decodes() {
        // A MIDI 1.0 wire note-on decodes the same way — protocol is invisible here.
        let ev = MidiEvent::from_midi1_bytes(0, &[0x93, 60, 100]).unwrap();
        let msg = ev.message();
        assert!(msg.is_note_on());
        assert_eq!(msg.channel(), Some(3));
        assert_eq!(msg.note(), Some(60));
    }

    #[test]
    fn velocity_zero_note_on_reads_as_note_off() {
        let ev = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 0]).unwrap();
        let msg = ev.message();
        assert!(msg.is_note_off());
        assert!(!msg.is_note_on());
    }

    #[test]
    fn control_change_carries_32bit_value() {
        let msg = MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::new(5),
            CCNumber::BRIGHTNESS,
            0xDEAD_BEEF,
        )
        .message();
        match msg {
            MidiMessage::ControlChange {
                channel,
                index,
                value,
                ..
            } => {
                assert_eq!(channel, 5);
                assert_eq!(index, 74);
                assert_eq!(value, 0xDEAD_BEEF);
            }
            other => panic!("expected ControlChange, got {other:?}"),
        }
    }

    #[test]
    fn same_pitch_different_channel_have_distinct_ids() {
        let a = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 60, 0x8000)
            .message()
            .id()
            .unwrap();
        let b = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(2), 60, 0x8000)
            .message()
            .id()
            .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn per_note_pitch_bend_addresses_one_note() {
        let msg =
            MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x9000_0000)
                .message();
        match msg {
            MidiMessage::PerNotePitchBend { note, value, .. } => {
                assert_eq!(note, 64);
                assert_eq!(value, 0x9000_0000);
            }
            other => panic!("expected PerNotePitchBend, got {other:?}"),
        }
    }

    #[test]
    fn system_transport_messages_decode_first_class() {
        // Timing clock / start / stop / continue are modeled variants now, not
        // `Other`. They carry no channel.
        assert!(matches!(
            MidiEvent::timing_clock(MidiGroup::FIRST).message(),
            MidiMessage::TimingClock { .. }
        ));
        assert!(matches!(
            MidiEvent::start(MidiGroup::FIRST).message(),
            MidiMessage::Start { .. }
        ));
        assert!(matches!(
            MidiEvent::stop(MidiGroup::FIRST).message(),
            MidiMessage::Stop { .. }
        ));
        assert!(matches!(
            MidiEvent::continue_msg(MidiGroup::FIRST).message(),
            MidiMessage::Continue { .. }
        ));
        assert_eq!(
            MidiEvent::timing_clock(MidiGroup::FIRST)
                .message()
                .channel(),
            None
        );
    }

    #[test]
    fn system_common_data_messages_decode_their_payload() {
        match MidiEvent::mtc_quarter_frame(MidiGroup::FIRST, 0x5F).message() {
            MidiMessage::TimeCode { code, .. } => assert_eq!(code, 0x5F),
            other => panic!("expected TimeCode, got {other:?}"),
        }
        match MidiEvent::song_position(MidiGroup::FIRST, 12345).message() {
            MidiMessage::SongPosition { position, .. } => assert_eq!(position, 12345),
            other => panic!("expected SongPosition, got {other:?}"),
        }
        match MidiEvent::song_select(MidiGroup::FIRST, 0x4F).message() {
            MidiMessage::SongSelect { song, .. } => assert_eq!(song, 0x4F),
            other => panic!("expected SongSelect, got {other:?}"),
        }
    }

    #[test]
    fn tune_request_stays_other() {
        // Tune Request is System Common but has no modeled variant — it must
        // still round-trip through `Other`.
        let tune = MidiEvent::tune_request(MidiGroup::FIRST);
        assert_eq!(tune.message(), MidiMessage::Other(tune));
    }

    #[test]
    fn absolute_channel_controllers_decode_structured() {
        // RPN and NRPN differ only in namespace; both carry bank/index/data.
        let rpn = MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(3),
            0x12,
            0x34,
            0xDEAD_BEEF,
        );
        assert_eq!(
            rpn.message(),
            MidiMessage::RegisteredController {
                frame_offset: 0,
                channel: 3,
                namespace: ControllerNamespace::Registered,
                bank: 0x12,
                index: 0x34,
                data: 0xDEAD_BEEF,
            }
        );

        let nrpn = MidiEvent::assignable_controller(
            MidiGroup::FIRST,
            MidiChannel::new(9),
            0x01,
            0x02,
            0x0000_1000,
        );
        assert_eq!(
            nrpn.message(),
            MidiMessage::RegisteredController {
                frame_offset: 0,
                channel: 9,
                namespace: ControllerNamespace::Assignable,
                bank: 0x01,
                index: 0x02,
                data: 0x0000_1000,
            }
        );
    }

    #[test]
    fn relative_channel_controllers_decode_structured() {
        let rpn = MidiEvent::relative_registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(5),
            0x40,
            0x07,
            42,
        );
        assert_eq!(
            rpn.message(),
            MidiMessage::RelativeController {
                frame_offset: 0,
                channel: 5,
                namespace: ControllerNamespace::Registered,
                bank: 0x40,
                index: 0x07,
                delta: 42,
            }
        );

        let nrpn = MidiEvent::relative_assignable_controller(
            MidiGroup::FIRST,
            MidiChannel::new(0),
            0x7F,
            0x7E,
            7,
        );
        assert_eq!(
            nrpn.message(),
            MidiMessage::RelativeController {
                frame_offset: 0,
                channel: 0,
                namespace: ControllerNamespace::Assignable,
                bank: 0x7F,
                index: 0x7E,
                delta: 7,
            }
        );
    }

    #[test]
    fn relative_controller_delta_stays_signed() {
        // M2-104 §7.4.8: the data field "contains a Two's Complement value". A
        // decrement must arrive as a negative `i32`, not as a near-`u32::MAX`
        // reinterpretation that would slam the parameter to full scale.
        for delta in [-1i32, -128, -0x0100_0000, i32::MIN, 0, 1, i32::MAX] {
            for (build, namespace) in [
                (
                    MidiEvent::relative_registered_controller
                        as fn(MidiGroup, MidiChannel, u8, u8, i32) -> MidiEvent,
                    ControllerNamespace::Registered,
                ),
                (
                    MidiEvent::relative_assignable_controller,
                    ControllerNamespace::Assignable,
                ),
            ] {
                let ev = build(MidiGroup::FIRST, MidiChannel::new(3), 0x12, 0x34, delta);
                match ev.message() {
                    MidiMessage::RelativeController {
                        delta: decoded,
                        namespace: ns,
                        ..
                    } => {
                        assert_eq!(decoded, delta, "delta {delta} for {namespace:?}");
                        assert_eq!(ns, namespace);
                    }
                    other => panic!("expected RelativeController, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn bank_and_index_are_not_transposed() {
        // Distinct bank/index values, so a swap in either the decode or the
        // re-encode shows up rather than cancelling out.
        let ev =
            MidiEvent::registered_controller(MidiGroup::FIRST, MidiChannel::new(3), 0x11, 0x22, 1);
        match ev.message() {
            MidiMessage::RegisteredController { bank, index, .. } => {
                assert_eq!(bank, 0x11);
                assert_eq!(index, 0x22);
            }
            other => panic!("expected RegisteredController, got {other:?}"),
        }
    }

    #[test]
    fn channel_controllers_round_trip_to_identical_wire_bytes() {
        // Decode → re-encode must reproduce the source packet exactly, frame
        // offset included, for all four channel-wide controller messages.
        for ev in [
            MidiEvent::registered_controller(
                MidiGroup::FIRST,
                MidiChannel::new(3),
                0x12,
                0x34,
                0xDEAD_BEEF,
            ),
            MidiEvent::assignable_controller(
                MidiGroup::FIRST,
                MidiChannel::new(9),
                0x01,
                0x02,
                0x0000_1000,
            ),
            MidiEvent::relative_registered_controller(
                MidiGroup::FIRST,
                MidiChannel::new(5),
                0x40,
                0x07,
                -1234,
            ),
            MidiEvent::relative_assignable_controller(
                MidiGroup::FIRST,
                MidiChannel::new(15),
                0x7F,
                0x00,
                i32::MIN,
            ),
        ] {
            let ev = ev.with_frame_offset(91);
            let msg = ev.message();
            assert!(
                !matches!(msg, MidiMessage::Other(_)),
                "must not fall through to Other: {msg:?}"
            );
            assert_eq!(msg.frame_offset(), 91);
            let back = MidiEvent::try_from(msg).expect("channel controller re-encodable");
            assert_eq!(
                back.data_words(),
                ev.data_words(),
                "wire mismatch for {msg:?}"
            );
            assert_eq!(back, ev, "round-trip mismatch for {msg:?}");
        }
    }

    #[test]
    fn channel_controllers_report_their_channel() {
        // The `channel()` accessor's `|`-chain must cover the new variants —
        // otherwise they read as channel-less system messages.
        assert_eq!(
            MidiEvent::registered_controller(MidiGroup::FIRST, MidiChannel::new(11), 0, 6, 0)
                .message()
                .channel(),
            Some(11)
        );
        assert_eq!(
            MidiEvent::relative_assignable_controller(
                MidiGroup::FIRST,
                MidiChannel::new(4),
                0,
                6,
                0
            )
            .message()
            .channel(),
            Some(4)
        );
    }

    #[test]
    fn absolute_and_relative_are_different_variants() {
        // Same address, same bit pattern in the data field, different meaning:
        // `-1` as an absolute set is `0xFFFF_FFFF`, as a delta it is one step
        // down. The variant split is what keeps a consumer from confusing them.
        let absolute = MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(3),
            0x12,
            0x34,
            0xFFFF_FFFF,
        )
        .message();
        let relative = MidiEvent::relative_registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(3),
            0x12,
            0x34,
            -1,
        )
        .message();
        assert!(matches!(
            absolute,
            MidiMessage::RegisteredController {
                data: 0xFFFF_FFFF,
                ..
            }
        ));
        assert!(matches!(
            relative,
            MidiMessage::RelativeController { delta: -1, .. }
        ));
        assert_ne!(absolute, relative);
    }

    #[test]
    fn round_trip_preserves_frame_offset() {
        // The view must not drop sample-accurate timing (project round-trip invariant).
        let ev = MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::new(5),
            CCNumber::BRIGHTNESS,
            0xABCD_1234,
        )
        .with_frame_offset(137);
        let msg = ev.message();
        assert_eq!(msg.frame_offset(), 137);
        let back = MidiEvent::try_from(msg).expect("re-encodable");
        assert_eq!(back, ev);
    }

    #[test]
    fn round_trip_preserves_note_attribute() {
        use midi2::channel_voice2::NoteAttribute;
        use midi2::num::Fixed7_9;
        // A note-on carrying a Pitch7_9 attribute must survive decode → re-encode.
        let mut on = midi2::channel_voice2::NoteOn::<[u32; 2]>::new();
        {
            use midi2::prelude::*;
            on.set_channel(u4::new(3));
            on.set_note_number(u7::new(60));
            on.set_velocity(0x8000);
            on.set_attribute(Some(NoteAttribute::Pitch7_9(Fixed7_9::from_bits(0x1234))));
        }
        let ev = MidiEvent::from_ump(0, {
            use midi2::Data;
            on.data()
        });
        let msg = ev.message();
        match msg {
            MidiMessage::NoteOn { attribute, .. } => {
                assert_eq!(
                    attribute,
                    Some(NoteAttribute::Pitch7_9(Fixed7_9::from_bits(0x1234)))
                );
            }
            other => panic!("expected NoteOn, got {other:?}"),
        }
        let back = MidiEvent::try_from(msg).expect("re-encodable");
        assert_eq!(back, ev);
    }

    #[test]
    fn round_trip_other_is_exact() {
        // An unmodeled message (tune request — System Common with no variant)
        // re-encodes byte-for-byte via the preserved event.
        let ev = MidiEvent::tune_request(MidiGroup::FIRST).with_frame_offset(42);
        assert!(matches!(ev.message(), MidiMessage::Other(_)));
        let back = MidiEvent::try_from(ev.message()).expect("Other round-trips");
        assert_eq!(back, ev);
    }

    #[test]
    fn system_transport_round_trips_preserve_frame_offset() {
        // Each modeled system message must survive decode → re-encode with its
        // frame offset intact (project round-trip invariant).
        for ev in [
            MidiEvent::timing_clock(MidiGroup::FIRST),
            MidiEvent::start(MidiGroup::FIRST),
            MidiEvent::continue_msg(MidiGroup::FIRST),
            MidiEvent::stop(MidiGroup::FIRST),
            MidiEvent::mtc_quarter_frame(MidiGroup::FIRST, 0x5F),
            MidiEvent::song_position(MidiGroup::FIRST, 12345),
            MidiEvent::song_select(MidiGroup::FIRST, 0x4F),
            MidiEvent::active_sensing(MidiGroup::FIRST),
            MidiEvent::system_reset(MidiGroup::FIRST),
        ] {
            let ev = ev.with_frame_offset(77);
            let msg = ev.message();
            assert_eq!(msg.frame_offset(), 77);
            let back = MidiEvent::try_from(msg).expect("system message re-encodable");
            assert_eq!(back, ev, "round-trip mismatch for {msg:?}");
        }
    }
}
