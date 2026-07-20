//! UMP-based [`MidiEvent`] with sample-accurate timing.
//!
//! A [`MidiEvent`] is a 20-byte packed struct — a 32-bit frame offset plus
//! four UMP words — carrying any MIDI message type. Construction goes through
//! [`midi2`] (spec-compliant encoding). Decoding is a `TryFrom` into
//! [`midi2::UmpMessage`] via [`MidiEvent::data_words`]:
//!
//! ```ignore
//! use midi2::UmpMessage;
//! if let Ok(msg) = UmpMessage::try_from(ev.data_words()) {
//!     // pattern match on msg
//! }
//! ```

use midi2::prelude::*;

use crate::compat::Vec;

/// Packed UMP event with sample-accurate timing.
///
/// The `data` field is a midi2-compatible `[u32; 4]` buffer — callers with a
/// `midi2::UmpMessage` in hand can simply copy its `.data()` into the first
/// N words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct MidiEvent {
    /// Offset within the current audio buffer, in samples.
    pub frame_offset: u32,
    /// Raw UMP words. [`Self::data_words`] returns the meaningful prefix.
    pub data: [u32; 4],
}

impl MidiEvent {
    /// Construct with pre-built UMP words and a frame offset.
    ///
    /// `words` must be 1, 2, or 4 entries long matching the UMP message type
    /// in the first word's top nibble. Extra slots in `data` are zero-padded.
    #[inline]
    pub fn from_ump(frame_offset: u32, words: &[u32]) -> Self {
        let mut data = [0u32; 4];
        let n = words.len().min(4);
        data[..n].copy_from_slice(&words[..n]);
        Self { frame_offset, data }
    }

    /// Builder-style setter used in chain form, e.g.
    /// `MidiEvent::note_on(...).with_frame_offset(128)`.
    #[inline]
    #[must_use]
    pub fn with_frame_offset(mut self, frame_offset: u32) -> Self {
        self.frame_offset = frame_offset;
        self
    }

    /// Return the meaningful prefix of [`Self::data`] (1, 2, or 4 words)
    /// per the UMP spec. Hand directly to `midi2::UmpMessage::try_from`.
    #[inline]
    pub fn data_words(&self) -> &[u32] {
        let n = ump_word_count((self.data[0] >> 28) as u8);
        &self.data[..n]
    }
}

// -----------------------------------------------------------------------------
// Quick accessors — thin passthroughs over midi2 for fast note/CC lookup
// -----------------------------------------------------------------------------
//
// Callers that need more than these (per-note pitch bend, program change bank,
// sysex, etc.) match `UmpMessage::try_from(ev.data_words())` directly. These
// cover the note-on/off + velocity lookup that MPE, voice allocators, and
// tests repeatedly need.

impl MidiEvent {
    /// `true` if this is a Channel Voice 1 or 2 Note On with non-zero velocity.
    pub fn is_note_on(&self) -> bool {
        use midi2::channel_voice1::ChannelVoice1;
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::UmpMessage;
        match UmpMessage::try_from(self.data_words()) {
            Ok(UmpMessage::ChannelVoice2(ChannelVoice2::NoteOn(m))) => m.velocity() > 0,
            Ok(UmpMessage::ChannelVoice1(ChannelVoice1::NoteOn(m))) => u8::from(m.velocity()) > 0,
            _ => false,
        }
    }

    /// `true` if this is a Note Off (or MIDI 1.0 velocity-0 NoteOn).
    pub fn is_note_off(&self) -> bool {
        use midi2::channel_voice1::ChannelVoice1;
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::UmpMessage;
        match UmpMessage::try_from(self.data_words()) {
            Ok(UmpMessage::ChannelVoice2(ChannelVoice2::NoteOff(_))) => true,
            Ok(UmpMessage::ChannelVoice1(ChannelVoice1::NoteOff(_))) => true,
            Ok(UmpMessage::ChannelVoice1(ChannelVoice1::NoteOn(m))) => u8::from(m.velocity()) == 0,
            _ => false,
        }
    }

    /// Note number for note-on/off/poly-pressure/per-note events.
    pub fn note(&self) -> Option<u8> {
        use midi2::channel_voice1::ChannelVoice1;
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::UmpMessage;
        match UmpMessage::try_from(self.data_words()).ok()? {
            UmpMessage::ChannelVoice2(ChannelVoice2::NoteOn(m)) => Some(u8::from(m.note_number())),
            UmpMessage::ChannelVoice2(ChannelVoice2::NoteOff(m)) => Some(u8::from(m.note_number())),
            UmpMessage::ChannelVoice2(ChannelVoice2::KeyPressure(m)) => {
                Some(u8::from(m.note_number()))
            }
            UmpMessage::ChannelVoice2(ChannelVoice2::PerNotePitchBend(m)) => {
                Some(u8::from(m.note_number()))
            }
            UmpMessage::ChannelVoice1(ChannelVoice1::NoteOn(m)) => Some(u8::from(m.note_number())),
            UmpMessage::ChannelVoice1(ChannelVoice1::NoteOff(m)) => Some(u8::from(m.note_number())),
            UmpMessage::ChannelVoice1(ChannelVoice1::KeyPressure(m)) => {
                Some(u8::from(m.note_number()))
            }
            _ => None,
        }
    }

    /// Velocity as a 7-bit value (downconverted from MIDI 2.0's 16-bit form
    /// via spec Min-Center-Max).
    pub fn velocity_u7(&self) -> Option<u8> {
        use crate::convert::midi2_velocity_to_midi1;
        use midi2::channel_voice1::ChannelVoice1;
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::UmpMessage;
        match UmpMessage::try_from(self.data_words()).ok()? {
            UmpMessage::ChannelVoice2(ChannelVoice2::NoteOn(m)) => {
                Some(midi2_velocity_to_midi1(m.velocity()))
            }
            UmpMessage::ChannelVoice2(ChannelVoice2::NoteOff(m)) => {
                Some(midi2_velocity_to_midi1(m.velocity()))
            }
            UmpMessage::ChannelVoice1(ChannelVoice1::NoteOn(m)) => Some(u8::from(m.velocity())),
            UmpMessage::ChannelVoice1(ChannelVoice1::NoteOff(m)) => Some(u8::from(m.velocity())),
            _ => None,
        }
    }

    /// Channel nibble (0-15) for a channel-voice event, read directly from the
    /// UMP word without a full decode. `None` for system, SysEx, and utility
    /// messages, which carry no channel. Covers both MIDI 1.0 (UMP type 0x2)
    /// and MIDI 2.0 (type 0x4) channel voice — the channel sits in the same
    /// bit position in both, so the hot path (e.g. MIDI routing by channel)
    /// avoids paying for a `midi2::UmpMessage::try_from` dispatch.
    #[inline]
    pub fn channel(&self) -> Option<u8> {
        let type_nibble = (self.data[0] >> 28) & 0x0F;
        // UMP type 0x2 = MIDI 1.0 channel voice, 0x4 = MIDI 2.0 channel voice.
        if type_nibble == 0x2 || type_nibble == 0x4 {
            Some(((self.data[0] >> 16) & 0x0F) as u8)
        } else {
            None
        }
    }
}

// -----------------------------------------------------------------------------
// MIDI 1.0 wire format → UMP
// -----------------------------------------------------------------------------

impl MidiEvent {
    /// Parse raw MIDI 1.0 wire bytes (2-3 byte channel-voice or 1-byte system
    /// real-time message) into a UMP [`MidiEvent`] of type 0x2 (Channel Voice 1)
    /// or 0x1 (System). Returns `None` on malformed input or SysEx (use
    /// [`Self::sysex7_fragments`] for that).
    pub fn from_midi1_bytes(frame_offset: u32, bytes: &[u8]) -> Option<Self> {
        use midly::live::{LiveEvent, SystemRealtime};
        let ev = LiveEvent::parse(bytes).ok()?;
        let out = match ev {
            LiveEvent::Midi { channel, message } => {
                midi1_channel_voice_to_ump(channel.as_int(), message)
            }
            LiveEvent::Realtime(rt) => match rt {
                SystemRealtime::TimingClock => Self::timing_clock(0),
                SystemRealtime::Start => Self::start(0),
                SystemRealtime::Continue => Self::continue_msg(0),
                SystemRealtime::Stop => Self::stop(0),
                SystemRealtime::ActiveSensing => Self::active_sensing(0),
                SystemRealtime::Reset => Self::system_reset(0),
                _ => return None,
            },
            LiveEvent::Common(common) => match common {
                midly::live::SystemCommon::MidiTimeCodeQuarterFrame(kind, val) => {
                    let data = (mtc_qf_nibble(kind) << 4) | val.as_int();
                    Self::mtc_quarter_frame(0, data)
                }
                midly::live::SystemCommon::SongPosition(pos) => {
                    Self::song_position(0, pos.as_int())
                }
                midly::live::SystemCommon::SongSelect(song) => Self::song_select(0, song.as_int()),
                midly::live::SystemCommon::TuneRequest => Self::tune_request(0),
                midly::live::SystemCommon::SysEx(_)
                | midly::live::SystemCommon::Undefined(_, _) => {
                    return None;
                }
            },
        };
        Some(out.with_frame_offset(frame_offset))
    }
}

/// Map a midly `MtcQuarterFrameMessage` to its 0-7 piece code (top nibble
/// of the MTC quarter-frame data byte).
fn mtc_qf_nibble(kind: midly::live::MtcQuarterFrameMessage) -> u8 {
    use midly::live::MtcQuarterFrameMessage::*;
    match kind {
        FramesLow => 0,
        FramesHigh => 1,
        SecondsLow => 2,
        SecondsHigh => 3,
        MinutesLow => 4,
        MinutesHigh => 5,
        HoursLow => 6,
        HoursHigh => 7,
    }
}

/// Build a UMP type 0x2 (MIDI 1.0 Channel Voice) event from a midly
/// `MidiMessage`. Keeps the 7-bit data values (no upconversion to MIDI 2.0).
fn midi1_channel_voice_to_ump(channel: u8, msg: midly::MidiMessage) -> MidiEvent {
    use midly::MidiMessage::*;
    let (opcode, d1, d2) = match msg {
        NoteOff { key, vel } => (0x8u32, key.as_int(), vel.as_int()),
        NoteOn { key, vel } => (0x9u32, key.as_int(), vel.as_int()),
        Aftertouch { key, vel } => (0xAu32, key.as_int(), vel.as_int()),
        Controller { controller, value } => (0xBu32, controller.as_int(), value.as_int()),
        ProgramChange { program } => (0xCu32, program.as_int(), 0),
        ChannelAftertouch { vel } => (0xDu32, vel.as_int(), 0),
        PitchBend { bend } => {
            let bend14 = (bend.as_int() as i32 + 8192).clamp(0, 16383) as u32;
            let lsb = (bend14 & 0x7F) as u8;
            let msb = ((bend14 >> 7) & 0x7F) as u8;
            (0xEu32, lsb, msb)
        }
    };
    let w0 = (0x2u32 << 28)
        | (opcode << 20)
        | (((channel & 0x0F) as u32) << 16)
        | (((d1 & 0x7F) as u32) << 8)
        | ((d2 & 0x7F) as u32);
    MidiEvent::from_ump(0, &[w0])
}

impl MidiEvent {
    /// Emit the MIDI 1.0 wire form of this event (1-3 bytes), if the message
    /// has a 1.0 representation. Returns `None` for MIDI 2.0-only messages
    /// (per-note controllers, RPN/NRPN, utility, SysEx) and non-channel-voice
    /// UMP types that don't correspond to a 1.0 status byte.
    pub fn to_midi1_bytes(&self) -> Option<([u8; 3], u8)> {
        let type_nibble = ((self.data[0] >> 28) & 0x0F) as u8;
        match type_nibble {
            0x2 => self.cv1_to_midi1_bytes(),
            0x4 => self.cv2_to_midi1_bytes(),
            0x1 => self.system_to_midi1_bytes(),
            _ => None,
        }
    }

    fn cv1_to_midi1_bytes(&self) -> Option<([u8; 3], u8)> {
        let opcode = ((self.data[0] >> 20) & 0x0F) as u8;
        let channel = ((self.data[0] >> 16) & 0x0F) as u8;
        let d1 = ((self.data[0] >> 8) & 0x7F) as u8;
        let d2 = (self.data[0] & 0x7F) as u8;
        let status = (opcode << 4) | channel;
        let len = match opcode {
            0x8 | 0x9 | 0xA | 0xB | 0xE => 3,
            0xC | 0xD => 2,
            _ => return None,
        };
        Some(([status, d1, d2], len))
    }

    fn cv2_to_midi1_bytes(&self) -> Option<([u8; 3], u8)> {
        use crate::convert::{
            midi2_cc_to_midi1, midi2_pitch_bend_to_midi1, midi2_velocity_to_midi1,
        };
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::UmpMessage;
        let msg = UmpMessage::try_from(self.data_words()).ok()?;
        let UmpMessage::ChannelVoice2(cv2) = msg else {
            return None;
        };
        let channel = u8::from(cv2.channel());
        let (opcode, d1, d2, len): (u8, u8, u8, u8) = match cv2 {
            ChannelVoice2::NoteOn(m) => (
                0x9,
                u8::from(m.note_number()),
                midi2_velocity_to_midi1(m.velocity()),
                3,
            ),
            ChannelVoice2::NoteOff(m) => (
                0x8,
                u8::from(m.note_number()),
                midi2_velocity_to_midi1(m.velocity()),
                3,
            ),
            ChannelVoice2::ControlChange(m) => (
                0xB,
                u8::from(m.control()),
                midi2_cc_to_midi1(m.control_change_data()),
                3,
            ),
            ChannelVoice2::ChannelPitchBend(m) => {
                let bend14 = midi2_pitch_bend_to_midi1(m.pitch_bend_data());
                (0xE, (bend14 & 0x7F) as u8, ((bend14 >> 7) & 0x7F) as u8, 3)
            }
            ChannelVoice2::ProgramChange(m) => (0xC, u8::from(m.program()), 0, 2),
            ChannelVoice2::ChannelPressure(m) => {
                (0xD, midi2_cc_to_midi1(m.channel_pressure_data()), 0, 2)
            }
            ChannelVoice2::KeyPressure(m) => (
                0xA,
                u8::from(m.note_number()),
                midi2_cc_to_midi1(m.key_pressure_data()),
                3,
            ),
            // Per-note pitch bend, per-note controller, RPN/NRPN have no
            // 1.0 single-message equivalent.
            _ => return None,
        };
        let status = (opcode << 4) | (channel & 0x0F);
        Some(([status, d1, d2], len))
    }

    fn system_to_midi1_bytes(&self) -> Option<([u8; 3], u8)> {
        let status = ((self.data[0] >> 16) & 0xFF) as u8;
        let d1 = ((self.data[0] >> 8) & 0x7F) as u8;
        let d2 = (self.data[0] & 0x7F) as u8;
        match status {
            0xF1 => Some(([status, d1, 0], 2)),
            0xF2 => Some(([status, d1, d2], 3)),
            0xF3 => Some(([status, d1, 0], 2)),
            0xF6 | 0xF8 | 0xFA..=0xFC | 0xFE | 0xFF => Some(([status, 0, 0], 1)),
            _ => None,
        }
    }
}

// -----------------------------------------------------------------------------
// Channel Voice 2 constructors (UMP type 0x4, two words)
// -----------------------------------------------------------------------------

impl MidiEvent {
    #[inline]
    pub fn note_on(group: u8, channel: u8, note: u8, velocity: u16) -> Self {
        use midi2::channel_voice2::NoteOn;
        let mut m = NoteOn::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_velocity(velocity);
        Self::from_ump(0, m.data())
    }

    /// Note-on from a 7-bit MIDI 1.0 velocity (0-127), widened to the 16-bit
    /// MIDI 2.0 field by left-shifting 9 bits.
    ///
    /// This is the shift-widen many callers were open-coding as
    /// `note_on(g, c, n, (vel as u16) << 9)`. Note it is *not* the spec
    /// Min-Center-Max upconvert (`convert::midi1_velocity_to_midi2`): `<< 9`
    /// maps 127 → 65024, not 65535. Preserved here verbatim so the helper is a
    /// drop-in for the existing call sites; reach for `note_on` +
    /// `midi1_velocity_to_midi2` when exact full-range fidelity matters.
    #[inline]
    pub fn note_on_7bit(group: u8, channel: u8, note: u8, velocity_u7: u8) -> Self {
        Self::note_on(group, channel, note, (velocity_u7 as u16) << 9)
    }

    #[inline]
    pub fn note_off(group: u8, channel: u8, note: u8, velocity: u16) -> Self {
        use midi2::channel_voice2::NoteOff;
        let mut m = NoteOff::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_velocity(velocity);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn cc(group: u8, channel: u8, cc: u8, value: u32) -> Self {
        use midi2::channel_voice2::ControlChange;
        let mut m = ControlChange::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_control(u7::new(cc & 0x7F));
        m.set_control_change_data(value);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn pitch_bend(group: u8, channel: u8, bend: u32) -> Self {
        use midi2::channel_voice2::ChannelPitchBend;
        let mut m = ChannelPitchBend::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_pitch_bend_data(bend);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn per_note_pitch_bend(group: u8, channel: u8, note: u8, bend: u32) -> Self {
        use midi2::channel_voice2::PerNotePitchBend;
        let mut m = PerNotePitchBend::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_pitch_bend_data(bend);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn program_change(group: u8, channel: u8, program: u8, bank: Option<u16>) -> Self {
        use midi2::channel_voice2::ProgramChange;
        let mut m = ProgramChange::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_program(u7::new(program & 0x7F));
        m.set_bank(bank.map(|b| u14::new(b & 0x3FFF)));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn channel_pressure(group: u8, channel: u8, pressure: u32) -> Self {
        use midi2::channel_voice2::ChannelPressure;
        let mut m = ChannelPressure::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_channel_pressure_data(pressure);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn poly_pressure(group: u8, channel: u8, note: u8, pressure: u32) -> Self {
        use midi2::channel_voice2::KeyPressure;
        let mut m = KeyPressure::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_key_pressure_data(pressure);
        Self::from_ump(0, m.data())
    }

    /// Per-Note Controller: `registered=true` for Registered (opcode 0x0),
    /// `registered=false` for Assignable (opcode 0x1).
    ///
    /// `index` accepts the full 0-255 range; midi2's semantic `Controller`
    /// enum only covers a spec-mandated subset, so the UMP word is written
    /// directly.
    #[inline]
    pub fn per_note_controller(
        group: u8,
        channel: u8,
        note: u8,
        index: u8,
        value: u32,
        registered: bool,
    ) -> Self {
        // UMP type 0x4, status nibble 0x0 (registered) or 0x1 (assignable).
        let opcode: u32 = if registered { 0x0 } else { 0x1 };
        let w0 = (0x4u32 << 28)
            | (((group & 0x0F) as u32) << 24)
            | (opcode << 20)
            | (((channel & 0x0F) as u32) << 16)
            | (((note & 0x7F) as u32) << 8)
            | (index as u32);
        Self::from_ump(0, &[w0, value])
    }

    /// Per-Note Management (M2-104 §7.4.5). `detach` = D (detach per-note
    /// controllers from prior notes on this note number); `reset` = S (reset
    /// per-note controllers to defaults). `D=0, S=0` has no defined function.
    #[inline]
    pub fn per_note_management(
        group: u8,
        channel: u8,
        note: u8,
        detach: bool,
        reset: bool,
    ) -> Self {
        use midi2::channel_voice2::PerNoteManagement;
        let mut m = PerNoteManagement::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_detach(detach);
        m.set_reset(reset);
        Self::from_ump(0, m.data())
    }
}

// -----------------------------------------------------------------------------
// System Real-Time / System Common (UMP type 0x1, one word)
// -----------------------------------------------------------------------------

impl MidiEvent {
    #[inline]
    pub fn timing_clock(group: u8) -> Self {
        use midi2::system_common::TimingClock;
        let mut m = TimingClock::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn start(group: u8) -> Self {
        use midi2::system_common::Start;
        let mut m = Start::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn continue_msg(group: u8) -> Self {
        use midi2::system_common::Continue;
        let mut m = Continue::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn stop(group: u8) -> Self {
        use midi2::system_common::Stop;
        let mut m = Stop::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn active_sensing(group: u8) -> Self {
        use midi2::system_common::ActiveSensing;
        let mut m = ActiveSensing::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn system_reset(group: u8) -> Self {
        use midi2::system_common::Reset;
        let mut m = Reset::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn mtc_quarter_frame(group: u8, data: u8) -> Self {
        use midi2::system_common::TimeCode;
        let mut m = TimeCode::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_time_code(u7::new(data & 0x7F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn song_position(group: u8, position: u16) -> Self {
        use midi2::system_common::SongPositionPointer;
        let mut m = SongPositionPointer::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_position(u14::new(position & 0x3FFF));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn song_select(group: u8, song: u8) -> Self {
        use midi2::system_common::SongSelect;
        let mut m = SongSelect::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_song(u7::new(song & 0x7F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn tune_request(group: u8) -> Self {
        use midi2::system_common::TuneRequest;
        let mut m = TuneRequest::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }
}

// -----------------------------------------------------------------------------
// Utility (UMP type 0x0, one word)
// -----------------------------------------------------------------------------

impl MidiEvent {
    #[inline]
    pub fn noop() -> Self {
        use midi2::utility::NoOp;
        Self::from_ump(0, NoOp::<[u32; 1]>::new().data())
    }

    /// JR (jitter-reduction) timestamp, 16-bit payload. The UMP spec reserves
    /// the group nibble on utility messages — tutti sets it directly here
    /// because `midi2::utility::Timestamp` exposes only the 16-bit data field.
    #[inline]
    pub fn jr_timestamp(group: u8, timestamp: u16) -> Self {
        use midi2::utility::Timestamp;
        let mut m = Timestamp::<[u32; 1]>::new();
        m.set_time_data(timestamp);
        let mut words = [0u32; 1];
        words[0] = m.data()[0] | (((group & 0x0F) as u32) << 24);
        Self::from_ump(0, &words)
    }
}

// -----------------------------------------------------------------------------
// SysEx 7-bit fragmentation
// -----------------------------------------------------------------------------
//
// UMP type 0x3 packets are 2 words each: a header with {0x3|group|status|len}
// and up to 6 payload bytes packed across the remaining octets. midi2's
// `Sysex7` type requires a `Vec<u32>` buffer, which is gated behind midi2's
// `std` feature — unavailable in no_std. Fragmentation is hand-written here,
// and round-trip-verified against midi2 in the test module below.

/// SysEx7 status nibble: a complete message in one packet.
pub const SYSEX7_STATUS_SINGLE: u8 = 0x0;
/// SysEx7 status nibble: first packet of a multi-packet message.
pub const SYSEX7_STATUS_START: u8 = 0x1;
/// SysEx7 status nibble: a middle packet of a multi-packet message.
pub const SYSEX7_STATUS_CONTINUE: u8 = 0x2;
/// SysEx7 status nibble: last packet of a multi-packet message.
pub const SYSEX7_STATUS_END: u8 = 0x3;

impl MidiEvent {
    /// Build SysEx 7-bit packets (UMP type 0x3, 64-bit each) for `data` and
    /// push them onto `out`. `data` is the payload *between* 0xF0 and 0xF7
    /// (no delimiters). Payloads ≤ 6 bytes produce a single packet; longer
    /// payloads produce `Start` + `Continue*` + `End`.
    pub fn sysex7_fragments(group: u8, data: &[u8], out: &mut Vec<MidiEvent>) {
        if data.len() <= 6 {
            out.push(Self::sysex7_packet(group, SYSEX7_STATUS_SINGLE, data));
            return;
        }
        let total = data.len();
        let mut emitted = 0usize;
        let mut chunks = data.chunks(6);
        let first = chunks.next().unwrap_or(&[]);
        emitted += first.len();
        out.push(Self::sysex7_packet(group, SYSEX7_STATUS_START, first));
        for chunk in chunks {
            emitted += chunk.len();
            let status = if emitted >= total {
                SYSEX7_STATUS_END
            } else {
                SYSEX7_STATUS_CONTINUE
            };
            out.push(Self::sysex7_packet(group, status, chunk));
        }
    }

    /// Build a single self-contained SysEx 7-bit packet (UMP type 0x3) from a
    /// payload of up to 6 bytes (the data *between* 0xF0 and 0xF7, no
    /// delimiters). Returns `None` if the payload exceeds one packet — use
    /// [`Self::sysex7_fragments`] for longer messages. The no-`alloc`
    /// single-packet counterpart to `sysex7_fragments`.
    pub fn sysex7_single(group: u8, payload: &[u8]) -> Option<Self> {
        if payload.len() > 6 {
            return None;
        }
        Some(Self::sysex7_packet(group, SYSEX7_STATUS_SINGLE, payload))
    }

    /// Decode a single SysEx 7-bit packet (UMP type 0x3) into its
    /// `(status, payload)` — the inverse of [`Self::sysex7_packet`]. Returns the
    /// status nibble ([`SYSEX7_STATUS_SINGLE`]/`START`/`CONTINUE`/`END`) and the
    /// up-to-6 payload bytes (no 0xF0/0xF7 delimiters). `None` for any event
    /// that isn't a type-0x3 packet, or one whose declared length exceeds 6.
    ///
    /// Reassembling a multi-packet SysEx stream is the caller's job — this
    /// decodes one packet, mirroring how [`Self::sysex7_fragments`] emits them.
    pub fn sysex7_payload(&self) -> Option<(u8, [u8; 6], usize)> {
        let w0 = self.data[0];
        if (w0 >> 28) & 0x0F != 0x3 {
            return None;
        }
        let status = ((w0 >> 20) & 0x0F) as u8;
        let n = ((w0 >> 16) & 0x0F) as usize;
        if n > 6 {
            return None;
        }
        let w1 = self.data[1];
        let mut out = [0u8; 6];
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            *slot = match i {
                0 => (w0 >> 8) & 0x7F,
                1 => w0 & 0x7F,
                2 => (w1 >> 24) & 0x7F,
                3 => (w1 >> 16) & 0x7F,
                4 => (w1 >> 8) & 0x7F,
                5 => w1 & 0x7F,
                _ => unreachable!(),
            } as u8;
        }
        Some((status, out, n))
    }

    fn sysex7_packet(group: u8, status: u8, payload: &[u8]) -> Self {
        debug_assert!(payload.len() <= 6);
        let n = payload.len() as u8;
        let mut w0 = (0x3u32 << 28)
            | (((group & 0x0F) as u32) << 24)
            | (((status & 0x0F) as u32) << 20)
            | (((n & 0x0F) as u32) << 16);
        let mut w1 = 0u32;
        for (i, &b) in payload.iter().enumerate() {
            let byte = (b & 0x7F) as u32;
            match i {
                0 => w0 |= byte << 8,
                1 => w0 |= byte,
                2 => w1 |= byte << 24,
                3 => w1 |= byte << 16,
                4 => w1 |= byte << 8,
                5 => w1 |= byte,
                _ => unreachable!(),
            }
        }
        Self::from_ump(0, &[w0, w1])
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Word count per UMP message type nibble (MIDI 2.0 spec §2.1.3).
#[inline]
pub(crate) const fn ump_word_count(type_nibble: u8) -> usize {
    match type_nibble & 0x0F {
        0x0 | 0x1 | 0x2 | 0x6 | 0x7 => 1,
        0x3 | 0x4 | 0x8 | 0x9 | 0xA => 2,
        0x5 | 0xB | 0xC | 0xD | 0xE | 0xF => 4,
        _ => 1,
    }
}

// =============================================================================
// RPN / NRPN (Registered / Assignable Controllers) + MPE Configuration Message
// -----------------------------------------------------------------------------
// MIDI 2.0 gives RPN and NRPN dedicated Channel Voice 2 messages carrying a
// (bank, index) 14-bit address and a full 32-bit data field — no multi-CC
// running-status dance. `RegisteredController` = RPN, `AssignableController` =
// NRPN (M2-104 §7.4.7–7.4.8). The MPE Configuration Message is RPN 0x0000/0x06.
// =============================================================================

impl MidiEvent {
    /// MIDI 2.0 **Registered Controller (RPN)** — `bank`/`index` select the
    /// registered parameter, `data` is its full 32-bit value.
    #[inline]
    pub fn registered_controller(
        group: u8,
        channel: u8,
        bank: u8,
        index: u8,
        data: u32,
    ) -> Self {
        use midi2::channel_voice2::RegisteredController;
        let mut m = RegisteredController::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_bank(u7::new(bank & 0x7F));
        m.set_index(u7::new(index & 0x7F));
        m.set_controller_data(data);
        Self::from_ump(0, m.data())
    }

    /// MIDI 2.0 **Assignable Controller (NRPN)** — `bank`/`index` select the
    /// non-registered parameter, `data` is its full 32-bit value.
    #[inline]
    pub fn assignable_controller(
        group: u8,
        channel: u8,
        bank: u8,
        index: u8,
        data: u32,
    ) -> Self {
        use midi2::channel_voice2::AssignableController;
        let mut m = AssignableController::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_bank(u7::new(bank & 0x7F));
        m.set_index(u7::new(index & 0x7F));
        m.set_controller_data(data);
        Self::from_ump(0, m.data())
    }
}

/// RPN bank for the MPE Configuration Message and Pitch-Bend Sensitivity: `0x00`.
pub const RPN_BANK_MPE: u8 = 0x00;
/// RPN index of the **MPE Configuration Message** (MCM): `0x06`. The data field
/// carries the member-channel count for the zone the message's channel names
/// (RP-053 / M2-104): a master channel + N members, `0` disables the zone.
pub const RPN_INDEX_MCM: u8 = 0x06;
/// RPN index of **Pitch Bend Sensitivity**: `0x00`. Data is the 7.25 fixed-point
/// semitone range (see [`crate::mpe::PitchBendSensitivity`]).
pub const RPN_INDEX_PITCH_BEND_SENSITIVITY: u8 = 0x00;

// =============================================================================
// Flex Data (UMP Message Type 0xD)
// -----------------------------------------------------------------------------
// Flex Data messages carry musical metadata that MIDI 1.0 kept in SMF "meta
// events" — tempo, time signature, key signature, metronome, chord names, and
// text/lyrics (M2-104 §7.5). They are group-scoped (no channel). The
// constructors below cover the fixed-size subset a DAW clip needs; the
// variable-length text/chord messages are not yet exposed.
// =============================================================================

impl MidiEvent {
    /// Flex Data **Set Tempo** from beats-per-minute. The wire field is the
    /// number of 10-nanosecond units per quarter note, so `bpm` is inverted:
    /// `600_000_000 / bpm` (60 s / bpm, in 10 ns units).
    #[inline]
    pub fn flex_set_tempo(group: u8, bpm: f64) -> Self {
        use midi2::flex_data::SetTempo;
        let ten_ns_per_qn = if bpm > 0.0 {
            (600_000_000.0 / bpm).round() as u32
        } else {
            0
        };
        let mut m = SetTempo::<[u32; 4]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_number_of_10_nanosecond_units_per_quarter_note(ten_ns_per_qn);
        Self::from_ump(0, m.data())
    }

    /// Flex Data **Set Time Signature**. `numerator`/`denominator` are the beats
    /// per bar and the beat unit; `num_32nd_notes` is the number of 1/32 notes
    /// per quarter note (usually 8).
    #[inline]
    pub fn flex_set_time_signature(
        group: u8,
        numerator: u8,
        denominator: u8,
        num_32nd_notes: u8,
    ) -> Self {
        use midi2::flex_data::SetTimeSignature;
        let mut m = SetTimeSignature::<[u32; 4]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_numerator(numerator);
        m.set_denominator(denominator);
        m.set_number_of_32nd_notes(num_32nd_notes);
        Self::from_ump(0, m.data())
    }

    /// Flex Data **Set Metronome**. `clocks_per_click` = MIDI clocks per primary
    /// click; the three bar accents mark which subdivisions are accented.
    #[inline]
    pub fn flex_set_metronome(
        group: u8,
        clocks_per_click: u8,
        bar_accent1: u8,
        bar_accent2: u8,
        bar_accent3: u8,
    ) -> Self {
        use midi2::flex_data::SetMetronome;
        let mut m = SetMetronome::<[u32; 4]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_number_of_clocks_per_primary_click(clocks_per_click);
        m.set_bar_accent1(bar_accent1);
        m.set_bar_accent2(bar_accent2);
        m.set_bar_accent3(bar_accent3);
        Self::from_ump(0, m.data())
    }
}

/// Recover BPM from a Flex Data Set Tempo [`MidiEvent`], or `None` if `event`
/// isn't one. Inverse of [`MidiEvent::flex_set_tempo`].
pub fn flex_tempo_bpm(event: &MidiEvent) -> Option<f64> {
    use midi2::flex_data::FlexData;
    use midi2::UmpMessage;
    let UmpMessage::FlexData(FlexData::SetTempo(m)) =
        UmpMessage::try_from(event.data_words()).ok()?
    else {
        return None;
    };
    let ten_ns = m.number_of_10_nanosecond_units_per_quarter_note();
    (ten_ns != 0).then(|| 600_000_000.0 / ten_ns as f64)
}

// =============================================================================
// UMP Stream (MT=0xF): Endpoint Discovery + Function Blocks
// -----------------------------------------------------------------------------
// UMP Stream messages configure the endpoint itself (protocol negotiation,
// endpoint & Function Block topology) rather than carrying musical data
// (M2-104 §7.1.1). The JR Timestamp constructor lives above; Start/End-of-Clip
// (also UMP Stream) are built by the clip-file codec (`clip_file`).
// =============================================================================

impl MidiEvent {
    /// UMP Stream **Endpoint Discovery** — the protocol-negotiation request an
    /// endpoint sends to learn a peer's capabilities. `ump_major`/`ump_minor`
    /// are the supported UMP version; the `request_*` flags select which replies
    /// to ask for (endpoint info / device identity / name / product id / stream
    /// configuration).
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn endpoint_discovery(
        ump_major: u8,
        ump_minor: u8,
        request_endpoint_info: bool,
        request_device_identity: bool,
        request_endpoint_name: bool,
        request_product_instance_id: bool,
        request_stream_configuration: bool,
    ) -> Self {
        use midi2::ump_stream::EndpointDiscovery;
        let mut m = EndpointDiscovery::<[u32; 4]>::new();
        m.set_ump_version_major(ump_major);
        m.set_ump_version_minor(ump_minor);
        m.set_request_endpoint_info(request_endpoint_info);
        m.set_request_device_identity(request_device_identity);
        m.set_request_endpoint_name(request_endpoint_name);
        m.set_request_product_instance_id(request_product_instance_id);
        m.set_request_stream_configuration(request_stream_configuration);
        Self::from_ump(0, m.data())
    }

    /// UMP Stream **Function Block Info** — declares one Function Block: whether
    /// it is `active`, its `block_number`, the `first_group` it spans and how
    /// many groups (`num_groups`), and its `direction`
    /// (input/output/bidirectional).
    #[inline]
    pub fn function_block_info(
        active: bool,
        block_number: u8,
        first_group: u8,
        num_groups: u8,
        direction: FunctionBlockDirection,
    ) -> Self {
        use midi2::ump_stream::{Direction, FunctionBlockInfo};
        let mut m = FunctionBlockInfo::<[u32; 4]>::new();
        m.set_active(active);
        m.set_function_block_number(u7::new(block_number & 0x7F));
        m.set_first_group(u4::new(first_group & 0x0F));
        m.set_number_of_groups_spanned(num_groups);
        m.set_direction(match direction {
            FunctionBlockDirection::Input => Direction::Input,
            FunctionBlockDirection::Output => Direction::Output,
            FunctionBlockDirection::Bidirectional => Direction::Bidirectional,
        });
        Self::from_ump(0, m.data())
    }

}

/// Direction of a Function Block, for [`MidiEvent::function_block_info`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionBlockDirection {
    Input,
    Output,
    Bidirectional,
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi2::{channel_voice2, system_common, utility, UmpMessage};

    /// Layout contract: `#[repr(C)] { u32, [u32;4] }` is exactly 20 bytes.
    #[test]
    fn layout_is_20_bytes() {
        assert_eq!(core::mem::size_of::<MidiEvent>(), 20);
    }

    #[test]
    fn with_frame_offset_preserves_payload() {
        let ev = MidiEvent::note_on(0, 0, 60, 0x8000);
        let shifted = ev.with_frame_offset(128);
        assert_eq!(shifted.frame_offset, 128);
        assert_eq!(shifted.data, ev.data);
    }

    #[test]
    fn note_on_decodes_via_midi2() {
        let ev = MidiEvent::note_on(0, 5, 60, 0x8000);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        match msg {
            UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::NoteOn(m)) => {
                assert_eq!(u8::from(m.channel()), 5);
                assert_eq!(u8::from(m.note_number()), 60);
                assert_eq!(m.velocity(), 0x8000);
            }
            _ => panic!("expected CV2 NoteOn, got {msg:?}"),
        }
    }

    #[test]
    fn cc_decodes_via_midi2() {
        let ev = MidiEvent::cc(0, 2, 74, 0xDEAD_BEEF);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        let UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::ControlChange(m)) = msg else {
            panic!("expected CV2 ControlChange");
        };
        assert_eq!(u8::from(m.channel()), 2);
        assert_eq!(u8::from(m.control()), 74);
        assert_eq!(m.control_change_data(), 0xDEAD_BEEF);
    }

    #[test]
    fn per_note_management_decodes_via_midi2() {
        let ev = MidiEvent::per_note_management(0, 4, 60, true, false);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        let UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::PerNoteManagement(m)) = msg
        else {
            panic!("expected CV2 PerNoteManagement");
        };
        assert_eq!(u8::from(m.channel()), 4);
        assert_eq!(u8::from(m.note_number()), 60);
        assert!(m.detach());
        assert!(!m.reset());
    }

    #[test]
    fn channel_reads_both_voice_versions() {
        // MIDI 2.0 channel voice (UMP type 0x4).
        assert_eq!(MidiEvent::note_on(0, 5, 60, 0x8000).channel(), Some(5));
        // MIDI 1.0 channel voice (UMP type 0x2), built via the wire bridge.
        let cv1 = MidiEvent::from_midi1_bytes(0, &[0x93, 0x3C, 0x64]).unwrap();
        assert_eq!(cv1.channel(), Some(3));
        // System messages carry no channel.
        assert_eq!(MidiEvent::timing_clock(0).channel(), None);
        assert_eq!(MidiEvent::noop().channel(), None);
    }

    #[test]
    fn timing_clock_is_one_word() {
        let ev = MidiEvent::timing_clock(0);
        assert_eq!(ev.data_words().len(), 1);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        assert!(matches!(
            msg,
            UmpMessage::SystemCommon(system_common::SystemCommon::TimingClock(_))
        ));
    }

    #[test]
    fn song_position_round_trips_14bit() {
        let ev = MidiEvent::song_position(0, 12345);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        let UmpMessage::SystemCommon(system_common::SystemCommon::SongPositionPointer(m)) = msg
        else {
            panic!("expected SongPositionPointer");
        };
        assert_eq!(u16::from(m.position()), 12345);
    }

    #[test]
    fn noop_is_utility_zero_word() {
        let ev = MidiEvent::noop();
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        assert!(matches!(
            msg,
            UmpMessage::Utility(utility::Utility::NoOp(_))
        ));
    }

    #[test]
    fn sysex7_single_packet() {
        let mut out = Vec::new();
        MidiEvent::sysex7_fragments(0, &[0x7E, 0x7F, 0x06, 0x01], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data_words().len(), 2);
    }

    #[test]
    fn sysex7_multi_packet() {
        let mut out = Vec::new();
        let data: [u8; 15] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        MidiEvent::sysex7_fragments(0, &data, &mut out);
        // 15 bytes / 6 per packet = 3 packets
        assert_eq!(out.len(), 3);
        for ev in &out {
            assert_eq!(ev.data_words().len(), 2);
        }
    }

    #[test]
    fn sysex7_single_payload_round_trips() {
        for payload in [&[][..], &[0x42][..], &[0x7E, 0x7F, 0x06, 0x01][..], &[1, 2, 3, 4, 5, 6][..]] {
            let ev = MidiEvent::sysex7_single(0, payload).expect("fits one packet");
            let (status, bytes, n) = ev.sysex7_payload().expect("decodes");
            assert_eq!(status, SYSEX7_STATUS_SINGLE);
            assert_eq!(n, payload.len());
            assert_eq!(&bytes[..n], payload);
        }
        // 7 bytes is more than one packet.
        assert!(MidiEvent::sysex7_single(0, &[0; 7]).is_none());
    }

    #[test]
    fn registered_controller_decodes_via_midi2() {
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::{Channeled, UmpMessage};
        let ev = MidiEvent::registered_controller(0, 3, 0x00, 0x06, 0xDEAD_BEEF);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m)) => {
                assert_eq!(u8::from(m.channel()), 3);
                assert_eq!(u8::from(m.bank()), 0x00);
                assert_eq!(u8::from(m.index()), 0x06);
                assert_eq!(m.controller_data(), 0xDEAD_BEEF);
            }
            other => panic!("expected RegisteredController, got {other:?}"),
        }
    }

    #[test]
    fn assignable_controller_decodes_via_midi2() {
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::{Channeled, UmpMessage};
        let ev = MidiEvent::assignable_controller(0, 9, 0x12, 0x34, 0x0000_1000);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::ChannelVoice2(ChannelVoice2::AssignableController(m)) => {
                assert_eq!(u8::from(m.channel()), 9);
                assert_eq!(u8::from(m.bank()), 0x12);
                assert_eq!(u8::from(m.index()), 0x34);
                assert_eq!(m.controller_data(), 0x0000_1000);
            }
            other => panic!("expected AssignableController, got {other:?}"),
        }
    }

    #[test]
    fn flex_set_tempo_round_trips_bpm() {
        for bpm in [60.0, 120.0, 140.0, 174.0] {
            let ev = MidiEvent::flex_set_tempo(0, bpm);
            let decoded = flex_tempo_bpm(&ev).expect("is a Set Tempo");
            // 10ns-per-qn is integer-quantized, so allow a tiny epsilon.
            assert!((decoded - bpm).abs() < 0.05, "bpm {bpm} → {decoded}");
        }
    }

    #[test]
    fn flex_set_time_signature_decodes_via_midi2() {
        use midi2::flex_data::FlexData;
        let ev = MidiEvent::flex_set_time_signature(0, 7, 8, 8);
        match midi2::UmpMessage::try_from(ev.data_words()).unwrap() {
            midi2::UmpMessage::FlexData(FlexData::SetTimeSignature(m)) => {
                assert_eq!(m.numerator(), 7);
                assert_eq!(m.denominator(), 8);
                assert_eq!(m.number_of_32nd_notes(), 8);
            }
            other => panic!("expected SetTimeSignature, got {other:?}"),
        }
    }

    #[test]
    fn flex_tempo_bpm_rejects_non_tempo() {
        assert!(flex_tempo_bpm(&MidiEvent::note_on(0, 0, 60, 0x8000)).is_none());
    }

    #[test]
    fn endpoint_discovery_decodes_via_midi2() {
        use midi2::ump_stream::UmpStream;
        use midi2::UmpMessage;
        let ev = MidiEvent::endpoint_discovery(1, 1, true, false, true, false, false);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::UmpStream(UmpStream::EndpointDiscovery(m)) => {
                assert_eq!(m.ump_version_major(), 1);
                assert_eq!(m.ump_version_minor(), 1);
                assert!(m.request_endpoint_info());
                assert!(!m.request_device_identity());
                assert!(m.request_endpoint_name());
            }
            other => panic!("expected EndpointDiscovery, got {other:?}"),
        }
    }

    #[test]
    fn function_block_info_decodes_via_midi2() {
        use midi2::ump_stream::{Direction, UmpStream};
        use midi2::UmpMessage;
        let ev = MidiEvent::function_block_info(true, 2, 4, 1, FunctionBlockDirection::Output);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::UmpStream(UmpStream::FunctionBlockInfo(m)) => {
                assert!(m.active());
                assert_eq!(u8::from(m.function_block_number()), 2);
                assert_eq!(u8::from(m.first_group()), 4);
                assert_eq!(m.number_of_groups_spanned(), 1);
                assert_eq!(m.direction(), Direction::Output);
            }
            other => panic!("expected FunctionBlockInfo, got {other:?}"),
        }
    }

    #[test]
    fn jr_timestamp_decodes_via_midi2() {
        use midi2::utility::Utility;
        use midi2::UmpMessage;
        let ev = MidiEvent::jr_timestamp(0, 0x1234);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::Utility(Utility::Timestamp(m)) => {
                assert_eq!(u16::from(m.time_data()), 0x1234);
            }
            other => panic!("expected JR Timestamp, got {other:?}"),
        }
    }
}
