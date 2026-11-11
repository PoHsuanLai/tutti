//! CLAP event list implementations.
//!
//! Events wrap the actual clap-sys C structs so that pointers returned by
//! `input_events_get` have the correct C memory layout for plugins to cast.

//! # Narrowing casts are denied in this module
//!
//! This file converts between this crate's vocabulary and the CLAP C ABI, where
//! the worst mistake is a cast that changes a value's meaning: CLAP's `-1`
//! wildcard for "all channels / all keys", masked, becomes channel 15, note 127
//! — one phantom voice, while every real voice keeps ringing.
//!
//! So truncating and sign-changing casts are denied here. Where a cast is safe,
//! the `#[allow(..., reason = "...")]` says why; the justification is the point,
//! not the lint. Keep those allows as narrow as the cast they cover — a
//! function-wide allow silently re-permits the very wrap it should prevent.
#![deny(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use crate::types::{
    ClapNoteExpression, NoteExpressionType, ParameterChanges, ParameterPoint, ParameterQueue,
};
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, CLAP_EVENT_MIDI_SYSEX, CLAP_EVENT_NOTE_CHOKE,
    CLAP_EVENT_NOTE_END, CLAP_EVENT_NOTE_EXPRESSION, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON,
    CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_VALUE,
    CLAP_NOTE_EXPRESSION_BRIGHTNESS, CLAP_NOTE_EXPRESSION_EXPRESSION, CLAP_NOTE_EXPRESSION_PAN,
    CLAP_NOTE_EXPRESSION_PRESSURE, CLAP_NOTE_EXPRESSION_TUNING, CLAP_NOTE_EXPRESSION_VIBRATO,
    CLAP_NOTE_EXPRESSION_VOLUME, clap_event_header, clap_event_midi, clap_event_midi_sysex,
    clap_event_note, clap_event_note_expression, clap_event_param_gesture, clap_event_param_mod,
    clap_event_param_value, clap_event_transport,
};
use std::ptr;
use tutti_clap_ffi::{EventRef, EventSink, PluginEvent};
use tutti_graph::UmpAt;
use tutti_plugin_types::{
    Normalized, ParamId, RtMidiEvents, SYSEX_MESSAGE_CAPACITY, StableOrder, WireMidi, note_id_for,
    note_id_to_channel_note, push_sysex, sysex_assembler, sysex_packets,
};

use crate::types::RtNoteExpressions;
use tutti_midi_protocol::{
    Narrower, PitchBendSensitivity, narrow, u16_to_unit_f32, u32_to_unit_f32, widen, widen_bytes,
};
use tutti_types::Semitones;
use tutti_ump::{
    Address, CCNumber, Message, MidiChannel, MidiGroup, NoteId, NoteNumber, Rpn, SysexAssembler,
    SysexStep, U7, Ump, Voice1, Voice2, decode,
};
use tutti_voicing::{NoteControl, NoteEvent, PerNoteState};

/// A single CLAP event, wrapping the underlying `#[repr(C)]` `clap_sys`
/// struct so a pointer to its `header` field can be cast back by the plugin.
///
/// Construct via the `note_on`/`note_off`/`midi`/`param_value`/`note_expression`
/// helpers, or from an `(Offset, Ump)` event added to an [`InputEventList`]
/// ([`InputEventList::add_midi`]).
#[derive(Debug)]
pub enum ClapEvent {
    /// A voice starts. Host→plugin.
    NoteOn(clap_event_note),
    /// A voice is released and may ring out through its tail. Host→plugin.
    NoteOff(clap_event_note),
    /// A voice is cut immediately, skipping the release. Host→plugin.
    NoteChoke(clap_event_note),
    /// A voice has finished sounding and its `note_id` may be reused.
    /// Plugin→host.
    NoteEnd(clap_event_note),
    /// A raw MIDI 1.0 message.
    Midi(clap_event_midi),
    /// A per-voice expression change (tuning, brightness, …).
    NoteExpression(clap_event_note_expression),
    /// A parameter is set to a value, in the parameter's plain range.
    ParamValue(clap_event_param_value),
    /// A parameter is offset by a modulation amount, leaving its base value
    /// untouched.
    ParamMod(clap_event_param_mod),
    /// A user began a continuous edit (a knob-drag starting), bracketing the
    /// `ParamValue` events that follow.
    ParamGestureBegin(clap_event_param_gesture),
    /// The continuous edit opened by `ParamGestureBegin` finished.
    ParamGestureEnd(clap_event_param_gesture),
    /// A transport update inside the block (`CLAP_EVENT_TRANSPORT`): the
    /// tempo, position and play state from its time on. Host→plugin.
    Transport(clap_event_transport),
    /// A complete SysEx message, `0xF0 … 0xF7`.
    ///
    /// On an [`OutputEventList`] it owns its buffer: the inner C struct's
    /// `buffer` pointer aliases `_data`, so this variant must not be moved out
    /// of its list. On an [`InputEventList`] `_data` is empty and `buffer`
    /// points into the list's SysEx reassembly buffer, valid until the list
    /// is next cleared.
    MidiSysex {
        /// The C event whose `buffer` field points into `_data`, or into the
        /// input list's reassembly buffer.
        inner: clap_event_midi_sysex,
        /// Backing storage for `inner.buffer` on an output list (empty on an
        /// input list). Kept alive alongside it; moving the variant would
        /// dangle that pointer.
        _data: Vec<u8>,
    },
}

/// The per-note bend range a plugin's emitted tuning is put on the MIDI wire
/// at: the MPE default (±48), which a receiver assumes until a stream sets
/// another with RPN #00/07, and the host sends none alongside. The inbound
/// direction reads each channel's range from the stream
/// ([`PerNoteState`], which scales a bend by it).
fn emitted_bend_range_semitones() -> f64 {
    f64::from(PitchBendSensitivity::MPE_DEFAULT.as_semitones_f32())
}

/// The per-note controllers with a CLAP note expression, and the expression
/// each is: the one table both directions read (registered and assignable
/// per-note controllers share the number).
const PER_NOTE_EXPRESSIONS: [(CCNumber, NoteExpressionType); 3] = [
    (CCNumber::VOLUME, NoteExpressionType::Volume),
    (CCNumber::PAN, NoteExpressionType::Pan),
    (CCNumber::BRIGHTNESS, NoteExpressionType::Brightness),
];

/// Maps a MIDI-2 per-note controller number to the CLAP note-expression it
/// corresponds to ([`PER_NOTE_EXPRESSIONS`]), or `None` when there's no
/// standard counterpart.
fn per_note_controller_expression(index: u8) -> Option<NoteExpressionType> {
    PER_NOTE_EXPRESSIONS
        .iter()
        .find(|(controller, _)| controller.get() == index)
        .map(|&(_, expression)| expression)
}

/// CLAP's upper bound for `CLAP_NOTE_EXPRESSION_VOLUME`.
///
/// The spec (`clap/events.h`) defines VOLUME as a **gain**, not a unit
/// fraction: "with 0 < x <= 4, plain = 20 * log(x)". So 1.0 is unity, 4.0 is
/// +12 dB, and 0 is excluded — a strict inequality, because 20·log(0) is −∞.
const CLAP_VOLUME_MAX_GAIN: f64 = 4.0;

/// Smallest VOLUME gain the host will emit.
///
/// CLAP excludes 0 from the VOLUME range, so a MIDI volume of 0 cannot be sent
/// verbatim. −120 dB is inaudible at any practical bit depth and is what a
/// fader's "−∞" position resolves to in practice, so it stands in for silence
/// while staying inside the legal open interval.
const CLAP_VOLUME_MIN_GAIN: f64 = 1e-6;

/// MIDI-2 per-note volume (unit `0..=1`, full scale = unity) → a CLAP VOLUME
/// gain in the spec's `0 < x <= 4`.
///
/// Unity is preserved: MIDI full scale maps to gain 1.0, matching CLAP's
/// "1.0 = unity". A MIDI controller cannot express boost, so the `1..4` half of
/// the CLAP range is unreachable *from MIDI* — that is correct, not a loss. The
/// bottom is clamped to [`CLAP_VOLUME_MIN_GAIN`], because 0.0 is outside
/// CLAP's open interval.
fn unit_to_clap_volume(unit: f32) -> f64 {
    let gain = f64::from(unit.clamp(0.0, 1.0));
    gain.max(CLAP_VOLUME_MIN_GAIN)
}

/// A CLAP VOLUME gain (`0 < x <= 4`) → MIDI-2 per-note volume (unit `0..=1`).
///
/// Exact inverse of [`unit_to_clap_volume`] over the attenuating half `0..=1`,
/// so a host→plugin→host round trip is lossless there and unity stays unity.
///
/// A plugin may legally emit boost (`1 < x <= 4`). MIDI's unit per-note volume
/// controller has no headroom above unity, so boost saturates at 1.0 — the
/// alternative (rescaling by 4) would move unity to 0.25 and silently attenuate
/// every ordinary value by 12 dB on the way back. Saturating loses only the
/// boost amount; rescaling would corrupt the whole range.
#[allow(
    clippy::cast_possible_truncation,
    reason = "CLAP gain is f64; the unit vocabulary is f32 by definition"
)]
fn clap_volume_to_unit(gain: f64) -> f32 {
    gain.clamp(0.0, CLAP_VOLUME_MAX_GAIN).min(1.0) as f32
}

/// Scales a MIDI-2 unit `0..=1` controller value into the CLAP value range of
/// `ty`. Only VOLUME differs from the identity: Pan (`0` left, `0.5`
/// centre, `1` right), Brightness, Expression, Vibrato and Pressure are all
/// genuinely `0..1` per `clap/events.h`.
fn expression_value_from_unit(ty: NoteExpressionType, unit: f32) -> f64 {
    match ty {
        NoteExpressionType::Volume => unit_to_clap_volume(unit),
        _ => f64::from(unit),
    }
}

/// Inverse of [`expression_value_from_unit`].
#[allow(
    clippy::cast_possible_truncation,
    reason = "CLAP expression values are f64; the unit vocabulary is f32"
)]
fn expression_value_to_unit(ty: NoteExpressionType, value: f64) -> f32 {
    match ty {
        NoteExpressionType::Volume => clap_volume_to_unit(value),
        _ => value as f32,
    }
}

/// Inverse of [`per_note_controller_expression`].
fn expression_to_per_note_controller_index(ty: NoteExpressionType) -> Option<u8> {
    PER_NOTE_EXPRESSIONS
        .iter()
        .find(|&&(_, expression)| expression == ty)
        .map(|(controller, _)| controller.get())
}

/// A CLAP event struct's size for its `clap_event_header::size` field.
///
/// Every CLAP event struct is a few dozen bytes, so the `usize` → `u32`
/// narrowing cannot lose anything; stating it once beats repeating an `#[allow]`
/// at each of the five constructors that fill this field.
#[allow(
    clippy::cast_possible_truncation,
    reason = "CLAP event structs are tens of bytes; the header field is u32"
)]
const fn header_size<T>() -> u32 {
    std::mem::size_of::<T>() as u32
}

/// Resolves a CLAP note event's `(channel, key)` into a concrete MIDI address,
/// or `None` when it names no single voice.
///
/// **CLAP types `channel` and `key` as `i16`, and `-1` is a wildcard** meaning
/// "all channels" / "all keys" (`clap/events.h`). Masking a wildcard the way a
/// non-negative value is masked does not preserve that meaning, it invents a
/// different one: `-1i16 as u8` is `0xFF`, so `& 0x0F` yields channel **15** and
/// `& 0x7F` yields note **127**. An all-notes-off aimed at every sounding voice
/// arrives pointed at one phantom voice, and every real voice keeps ringing.
///
/// So a wildcard falls back to the host-minted `note_id`, which addresses a
/// specific voice. A plugin's own `note_id` space cannot be decoded, in which
/// case this returns `None` and the caller drops the event — dropping it is
/// recoverable, misaddressing it is not.
///
/// Shared by the note-on, note-off and note-expression paths.
/// A MIDI 2.0 channel voice packet on group 1 and `channel`.
fn voice2(channel: MidiChannel, voice: Voice2) -> Ump {
    Message::Midi2(Address::first(channel), voice).into()
}

fn note_address(channel: i16, key: i16, note_id: i32) -> Option<(MidiChannel, NoteNumber)> {
    if channel >= 0 && key >= 0 {
        // Both are `>= 0` in this branch (the wildcard case returned above),
        // and each is masked to the width MIDI wants, so neither narrowing can
        // drop a bit the mask would have kept.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "non-negative in this branch and masked to 4/7 bits"
        )]
        Some((
            MidiChannel::masked(channel as u8),
            NoteNumber::masked(key as u8),
        ))
    } else {
        note_id_to_channel_note(note_id)
    }
}

// Host→plugin events built below leave `header.flags = 0`, so
// `CLAP_EVENT_IS_LIVE` is never set. IS_LIVE marks an event as originating from
// live hardware interaction (a physical knob/key) rather than sequencer
// playback, letting a plugin treat the two differently (e.g. smoothing). The
// live-vs-playback distinction is not threaded from the caller today — the
// `(Offset, Ump)` → `ClapEvent` conversion has no such signal, and neither does the
// process/flush entry path. Rather than invent a bogus source flag, IS_LIVE
// stays unset until callers can supply a real live/playback origin.
impl ClapEvent {
    /// Borrows the common CLAP event header (time, type, space ID, flags).
    pub fn header(&self) -> &clap_event_header {
        match self {
            ClapEvent::NoteOn(e) => &e.header,
            ClapEvent::NoteOff(e) => &e.header,
            ClapEvent::NoteChoke(e) => &e.header,
            ClapEvent::NoteEnd(e) => &e.header,
            ClapEvent::Midi(e) => &e.header,
            ClapEvent::NoteExpression(e) => &e.header,
            ClapEvent::ParamValue(e) => &e.header,
            ClapEvent::ParamMod(e) => &e.header,
            ClapEvent::ParamGestureBegin(e) => &e.header,
            ClapEvent::ParamGestureEnd(e) => &e.header,
            ClapEvent::Transport(e) => &e.header,
            ClapEvent::MidiSysex { inner, .. } => &inner.header,
        }
    }

    /// Mints the host's `note_id` for a `(channel, key)` voice, matching what
    /// [`per_note_expression`](Self::per_note_expression) stamps.
    ///
    /// [`note_id_for`] takes a channel and a note; note/off events carry CLAP's signed
    /// `i16` wildcard-capable fields. A wildcard (`< 0`) or out-of-range value
    /// has no `(channel, key)` voice to name, so it yields CLAP's `-1`
    /// "unspecified note id" — which the spec permits and plugins fall back
    /// from by matching on the rest of the (port, channel, key, note_id) tuple.
    fn voice_note_id(channel: i16, key: i16) -> i32 {
        let channel = u8::try_from(channel).ok().and_then(MidiChannel::new);
        let key = u8::try_from(key).ok().and_then(NoteNumber::new);
        match (channel, key) {
            (Some(channel), Some(key)) => note_id_for(channel, key),
            _ => -1,
        }
    }

    /// Mutable view of the common CLAP event header, so the host can correct
    /// `time` at the boundary (see [`InputEventList::clamp_times`]).
    fn header_mut(&mut self) -> &mut clap_event_header {
        match self {
            ClapEvent::NoteOn(e) => &mut e.header,
            ClapEvent::NoteOff(e) => &mut e.header,
            ClapEvent::NoteChoke(e) => &mut e.header,
            ClapEvent::NoteEnd(e) => &mut e.header,
            ClapEvent::Midi(e) => &mut e.header,
            ClapEvent::NoteExpression(e) => &mut e.header,
            ClapEvent::ParamValue(e) => &mut e.header,
            ClapEvent::ParamMod(e) => &mut e.header,
            ClapEvent::ParamGestureBegin(e) => &mut e.header,
            ClapEvent::ParamGestureEnd(e) => &mut e.header,
            ClapEvent::Transport(e) => &mut e.header,
            ClapEvent::MidiSysex { inner, .. } => &mut inner.header,
        }
    }

    /// Builds a CLAP note-on event. `velocity` is normalized to `[0.0, 1.0]`.
    ///
    /// The `note_id` is minted from `(channel, key)` with the *same*
    /// [`note_id_for`] the note-expression path uses, so a plugin keying its
    /// voice map on `note_id` — the normal MPE-capable design — finds the voice
    /// a later `NOTE_EXPRESSION` refers to.
    pub fn note_on(time: u32, channel: i16, key: i16, velocity: f64) -> Self {
        ClapEvent::NoteOn(clap_event_note {
            header: clap_event_header {
                size: header_size::<clap_event_note>(),
                time,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_NOTE_ON,
                flags: 0,
            },
            note_id: Self::voice_note_id(channel, key),
            port_index: 0,
            channel,
            key,
            velocity,
        })
    }

    /// Builds a CLAP note-off event. `velocity` is normalized to `[0.0, 1.0]`.
    ///
    /// Carries the same `(channel, key)`-derived `note_id` as the matching
    /// [`note_on`](Self::note_on), so the release lands on the voice the
    /// note-on opened.
    pub fn note_off(time: u32, channel: i16, key: i16, velocity: f64) -> Self {
        ClapEvent::NoteOff(clap_event_note {
            header: clap_event_header {
                size: header_size::<clap_event_note>(),
                time,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_NOTE_OFF,
                flags: 0,
            },
            note_id: Self::voice_note_id(channel, key),
            port_index: 0,
            channel,
            key,
            velocity,
        })
    }

    /// Re-encodes a note event as `clap_event_midi` on note port `port`, for a
    /// note port that takes the MIDI dialect only. A note expression has no
    /// CLAP-side form there: poly pressure falls back to its own MIDI 1.0 bytes
    /// (`key_pressure`, `0xAn`), and the rest (per-note pitch bend,
    /// controllers) is `None`. Other events pass through.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "velocity is clamped to 0..=127 before the cast"
    )]
    fn into_midi_dialect(self, port: u16, key_pressure: Option<[u8; 3]>) -> Option<Self> {
        let (note_on, e) = match self {
            ClapEvent::NoteOn(e) => (true, e),
            ClapEvent::NoteOff(e) => (false, e),
            ClapEvent::NoteExpression(e) => {
                return Some(ClapEvent::midi(e.header.time, port, key_pressure?));
            }
            other => return Some(other),
        };
        // A note-on at velocity 0 is a note-off in MIDI 1.0, so the lowest
        // audible note-on velocity is 1.
        let floor = if note_on { 1.0 } else { 0.0 };
        let velocity = U7::new((e.velocity * 127.0).round().clamp(floor, 127.0) as u8)?;
        let (channel, key) = (u8::try_from(e.channel).ok()?, u8::try_from(e.key).ok()?);
        let (channel, note) = (MidiChannel::masked(channel), NoteNumber::masked(key));
        let voice = if note_on {
            Voice1::NoteOn { note, velocity }
        } else {
            Voice1::NoteOff { note, velocity }
        };
        Some(ClapEvent::midi(
            e.header.time,
            port,
            voice.bytes(channel).padded(),
        ))
    }

    /// This note event on note port `port`, naming the note `note_id` (`-1`
    /// for a Note Off, which the plugin matches by port, channel and key).
    fn addressed(mut self, port: u16, note_id: i32) -> Self {
        let port = i16::try_from(port).unwrap_or(-1);
        match &mut self {
            ClapEvent::NoteOn(e) | ClapEvent::NoteOff(e) => {
                e.port_index = port;
                e.note_id = note_id;
            }
            ClapEvent::NoteExpression(e) => {
                e.port_index = port;
                e.note_id = note_id;
            }
            _ => {}
        }
        self
    }

    /// Builds a generic 3-byte MIDI-1 event (status + two data bytes).
    pub fn midi(time: u32, port_index: u16, data: [u8; 3]) -> Self {
        ClapEvent::Midi(clap_event_midi {
            header: clap_event_header {
                size: header_size::<clap_event_midi>(),
                time,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_MIDI,
                flags: 0,
            },
            port_index,
            data,
        })
    }

    /// Builds a parameter-value event. Targets every note/port/channel/key
    /// (wildcard `-1`) — use the constructors on `clap_sys` directly if you
    /// need to scope more tightly.
    pub fn param_value(time: u32, param_id: u32, value: f64) -> Self {
        ClapEvent::ParamValue(clap_event_param_value {
            header: clap_event_header {
                size: header_size::<clap_event_param_value>(),
                time,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        })
    }

    /// Builds a note-expression event targeting a note by its CLAP `note_id`.
    ///
    /// **Partial:** `None` for [`Custom`](NoteExpressionType::Custom). CLAP's
    /// `clap_note_expression` is a closed set of seven ids with no vendor range,
    /// so a dimension defined by some *other* plugin (a VST3 id above
    /// `kCustomStart`) has nothing to map onto. Dropping it is the honest
    /// answer; picking the numerically-nearest CLAP id would deliver a
    /// plugin-specific value as, say, brightness.
    pub fn note_expression(
        time: u32,
        expression_type: NoteExpressionType,
        note_id: i32,
        value: f64,
    ) -> Option<Self> {
        let expression_id = match expression_type {
            NoteExpressionType::Volume => CLAP_NOTE_EXPRESSION_VOLUME,
            NoteExpressionType::Pan => CLAP_NOTE_EXPRESSION_PAN,
            NoteExpressionType::Tuning => CLAP_NOTE_EXPRESSION_TUNING,
            NoteExpressionType::Vibrato => CLAP_NOTE_EXPRESSION_VIBRATO,
            NoteExpressionType::Brightness => CLAP_NOTE_EXPRESSION_BRIGHTNESS,
            NoteExpressionType::Pressure => CLAP_NOTE_EXPRESSION_PRESSURE,
            NoteExpressionType::Expression => CLAP_NOTE_EXPRESSION_EXPRESSION,
            NoteExpressionType::Custom(_) => return None,
        };

        Some(ClapEvent::NoteExpression(clap_event_note_expression {
            header: clap_event_header {
                size: header_size::<clap_event_note_expression>(),
                time,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_NOTE_EXPRESSION,
                flags: 0,
            },
            expression_id,
            note_id,
            port_index: 0,
            channel: -1,
            key: -1,
            value,
        }))
    }

    /// The bytes and frame offset of a plugin-emitted SysEx message, which
    /// is several UMP packets rather than the one event [`Self::to_midi`]
    /// returns. `None` for any other event.
    fn sysex_bytes(&self) -> Option<(&[u8], u32)> {
        match self {
            // An input list's event has no `_data` and is never read back.
            ClapEvent::MidiSysex { inner, _data } if !_data.is_empty() => {
                Some((_data.as_slice(), inner.header.time))
            }
            _ => None,
        }
    }

    /// Converts a `ClapEvent` back to a Tutti UMP packet with the time the
    /// plugin reported ([`WireMidi`]: raw, not clamped to a block).
    ///
    /// Typed NoteOn/Off events build a native MIDI-2 Channel Voice event, so the
    /// plugin's `f64` velocity is preserved at MIDI-2's full 16-bit width instead
    /// of being squashed to 7 bits. Generic `Midi` events upconvert from their
    /// raw MIDI-1 bytes. Returns `None` for non-MIDI variants (NoteExpression,
    /// ParamValue, etc.).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "CLAP carries velocities and expression values as f64; the MIDI-2 conversion helpers take f32"
    )]
    #[must_use]
    pub fn to_midi(&self) -> Option<WireMidi> {
        use tutti_midi_protocol::unit_f32_to_u16;
        match self {
            ClapEvent::NoteOn(e) => {
                let (channel, note) = note_address(e.channel, e.key, e.note_id)?;
                Some(WireMidi {
                    offset: e.header.time,
                    ump: voice2(
                        channel,
                        Voice2::note_on(note, unit_f32_to_u16(e.velocity as f32)),
                    ),
                })
            }
            ClapEvent::NoteOff(e) => {
                let (channel, note) = note_address(e.channel, e.key, e.note_id)?;
                // Preserve the plugin's release velocity on the way back to
                // MIDI-2, mirroring the NoteOn path.
                Some(WireMidi {
                    offset: e.header.time,
                    ump: voice2(
                        channel,
                        Voice2::note_off(note, unit_f32_to_u16(e.velocity as f32)),
                    ),
                })
            }
            ClapEvent::NoteExpression(e) => Self::note_expression_to_midi(e),
            // Promote the MIDI-1 bytes to Channel Voice 2 at this edge, so the
            // engine sees one vocabulary regardless of source — matching the
            // hardware input path. System / SysEx messages pass through unchanged.
            ClapEvent::Midi(e) => Some(WireMidi {
                offset: e.header.time,
                ump: widen_bytes(MidiGroup::FIRST, &e.data)?,
            }),
            _ => None,
        }
    }

    /// Rebuilds the MIDI-2 per-note [`WireMidi`] a CLAP note-expression stands
    /// for: Tuning → per-note pitch bend, Pressure → poly pressure,
    /// Volume/Pan/Brightness → the matching per-note controller. `(channel, note)`
    /// come from the event's `channel`/`key` when the host stamped them, else
    /// from decoding its `note_id`. Returns `None` for dimensions with no MIDI-2
    /// per-note counterpart (Vibrato, Expression).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "CLAP expression values are f64; the MIDI-2 helpers take f32"
    )]
    fn note_expression_to_midi(e: &clap_event_note_expression) -> Option<WireMidi> {
        use tutti_midi_protocol::{signed_f32_to_bend_u32, unit_f32_to_u32};

        let (channel, note) = note_address(e.channel, e.key, e.note_id)?;
        let time = e.header.time;

        let expression_type = match e.expression_id {
            id if id == CLAP_NOTE_EXPRESSION_VOLUME => NoteExpressionType::Volume,
            id if id == CLAP_NOTE_EXPRESSION_PAN => NoteExpressionType::Pan,
            id if id == CLAP_NOTE_EXPRESSION_TUNING => NoteExpressionType::Tuning,
            id if id == CLAP_NOTE_EXPRESSION_BRIGHTNESS => NoteExpressionType::Brightness,
            id if id == CLAP_NOTE_EXPRESSION_PRESSURE => NoteExpressionType::Pressure,
            _ => return None,
        };

        let event = match expression_type {
            NoteExpressionType::Tuning => {
                let signed = (e.value / emitted_bend_range_semitones()) as f32;
                voice2(
                    channel,
                    Voice2::PerNotePitchBend {
                        note,
                        value: signed_f32_to_bend_u32(signed),
                    },
                )
            }
            NoteExpressionType::Pressure => voice2(
                channel,
                Voice2::PolyPressure {
                    note,
                    value: unit_f32_to_u32(e.value as f32),
                },
            ),
            other => {
                let index = expression_to_per_note_controller_index(other)?;
                voice2(
                    channel,
                    Voice2::AssignablePerNote {
                        note,
                        controller: index,
                        // VOLUME arrives as a CLAP gain (`0 < x <= 4`), not a
                        // unit fraction — convert before the unit encoding, or a
                        // plugin's unity 1.0 and its +12 dB 4.0 both saturate to
                        // MIDI full scale indistinguishably.
                        value: unit_f32_to_u32(expression_value_to_unit(other, e.value)),
                    },
                )
            }
        };
        Some(WireMidi {
            offset: time,
            ump: event,
        })
    }

    /// A note-expression event for the note `note_id` on note port `port`,
    /// stamping the `channel` and `key` it is known by (`None` is CLAP's `-1`
    /// wildcard) so a plugin can match on those as well as the id.
    fn per_note_expression(
        time: u32,
        expression_type: NoteExpressionType,
        note_id: i32,
        port: u16,
        channel: MidiChannel,
        key: Option<NoteNumber>,
        value: f64,
    ) -> Option<Self> {
        let mut event =
            Self::note_expression(time, expression_type, note_id, value)?.addressed(port, note_id);
        if let ClapEvent::NoteExpression(ne) = &mut event {
            ne.channel = i16::from(channel.get());
            ne.key = key.map_or(-1, |key| i16::from(key.get()));
        }
        Some(event)
    }
}

/// The note id a plugin is told for `id`. A minted id past `i32::MAX` (after
/// two billion notes) has no `i32` form and is CLAP's `-1` "unspecified": the
/// plugin falls back to port, channel and key.
fn plugin_note_id(id: NoteId) -> i32 {
    i32::try_from(id.raw()).unwrap_or(-1)
}

/// The expression and CLAP value a per-note controller stands for on the note
/// `note`, or `None` when CLAP has no expression for it. A Pitch is the
/// tuning relative to the note's key, `note`.
fn control_expression(
    control: &NoteControl,
    note: NoteNumber,
) -> Option<(NoteExpressionType, f64)> {
    match *control {
        NoteControl::Pitch(pitch) => Some((
            NoteExpressionType::Tuning,
            f64::from(pitch.note_number()) - f64::from(note.get()),
        )),
        NoteControl::Pressure(value) => Some((
            NoteExpressionType::Pressure,
            f64::from(u32_to_unit_f32(value)),
        )),
        NoteControl::Registered(controller, value) => {
            let ty = per_note_controller_expression(controller.number())?;
            Some((ty, expression_value_from_unit(ty, u32_to_unit_f32(value))))
        }
        NoteControl::Assignable(index, value) => {
            let ty = per_note_controller_expression(index)?;
            Some((ty, expression_value_from_unit(ty, u32_to_unit_f32(value))))
        }
    }
}

/// What a Per-Note Management Reset puts each expression CLAP has back to:
/// no tuning, no pressure, unity volume, centred pan and brightness.
const EXPRESSION_DEFAULTS: [(NoteExpressionType, f64); 5] = [
    (NoteExpressionType::Tuning, 0.0),
    (NoteExpressionType::Pressure, 0.0),
    (NoteExpressionType::Volume, 1.0),
    (NoteExpressionType::Pan, 0.5),
    (NoteExpressionType::Brightness, 0.5),
];

/// Common interface shared by [`InputEventList`] and [`OutputEventList`].
pub trait EventList {
    /// Number of events currently held in the list.
    fn len(&self) -> usize;

    /// Whether the list has no events.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Removes all events.
    fn clear(&mut self);
}

/// Owned list of input events passed into `clap_plugin.process()`.
///
/// The plugin reads it through the FFI crate's `clap_input_events`, built over
/// this list ([`tutti_clap_ffi::EventSource`]) for the span of the
/// call.
#[derive(Debug)]
pub struct InputEventList {
    pub(crate) events: Vec<ClapEvent>,
    /// [`sort_by_time`](Self::sort_by_time)'s scratch, reserved with the
    /// events so the sort never allocates.
    order: StableOrder,
    /// The parameter events one [`add_param_changes`](Self::add_param_changes)
    /// may push: the room reserved for them at activation
    /// ([`set_param_point_room`](Self::set_param_point_room)). Unbounded
    /// until set.
    pub(crate) param_room: usize,
    /// Parameter points not handed on whole since the list was built: see
    /// [`param_points_dropped`](Self::param_points_dropped).
    param_points_dropped: u64,
    /// MIDI messages a banked Program Change could not stage for lack of
    /// room: see [`midi_dropped`](Self::midi_dropped).
    midi_dropped: u64,
    /// Set by [`reserve`](Self::reserve): MIDI then stays within the reserved
    /// room, where an unreserved list (a test, an off-RT build) just grows.
    bounded: bool,
    /// The per-note state of the MIDI added so far (M2-104 section 7.4): each
    /// Group, channel and Note Number's held values, the notes that respond
    /// to them and each channel's bend sensitivity. Kept across blocks
    /// ([`EventList::clear`] leaves it): it is the stream's state.
    notes: PerNoteState,
    /// The MIDI 2.0 to MIDI 1.0 state of every `clap_event_midi` sent: which
    /// parameter and bank each channel of each Group has had selected.
    narrower: Narrower,
    /// The input note ports the plugin has: a Group is note port `group`.
    ports: usize,
    /// Bit `n` is set when note port `n` takes the MIDI dialect only.
    midi_ports: u16,
    /// Messages of a Group the plugin has no note port for: see
    /// [`groups_dropped`](Self::groups_dropped).
    groups_dropped: u64,
    /// Reassembles SysEx7 packets and owns the bytes an input
    /// [`ClapEvent::MidiSysex`] points at. No room until
    /// [`reserve_sysex`](Self::reserve_sysex).
    sysex: SysexAssembler<Box<[u8]>>,
}

impl InputEventList {
    /// Creates an empty input event list.
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            order: StableOrder::default(),
            param_room: usize::MAX,
            param_points_dropped: 0,
            midi_dropped: 0,
            bounded: false,
            notes: Self::per_note_state(1),
            narrower: Narrower::new(),
            ports: 1,
            midi_ports: 0,
            groups_dropped: 0,
            sysex: SysexAssembler::new(Box::default(), 0, 0),
        }
    }

    /// The per-note state of `ports` note ports, each channel's bend
    /// sensitivity at the MPE default, ±48 (RP-053), until the stream sets
    /// another.
    fn per_note_state(ports: usize) -> PerNoteState {
        PerNoteState::new(
            ports,
            Semitones(PitchBendSensitivity::MPE_DEFAULT.as_semitones_f32()),
        )
    }

    /// Sets the note dialect of the plugin's input note port: `true` sends
    /// notes as `clap_event_midi` (a port that takes MIDI but not CLAP note
    /// events), `false` as CLAP note events. While set, a MIDI event's per-note
    /// expression is sent as its MIDI 1.0 bytes where it has them (poly
    /// pressure), and [`add_note_expressions`](Self::add_note_expressions)
    /// adds nothing. Takes effect for events added after.
    ///
    /// This is [`set_note_ports`](Self::set_note_ports) for one port.
    pub fn set_midi_notes(&mut self, midi_notes: bool) {
        self.set_note_ports(&[midi_notes]);
    }

    /// Sets the plugin's input note ports, one entry each (at most 16, the
    /// Groups of MIDI 2.0), `true` for a port that takes the MIDI dialect
    /// only (see [`set_midi_notes`](Self::set_midi_notes)). A UMP Group is
    /// the note port of its number: Group 0 reaches port 0, Group 1 port 1,
    /// and a Group past the last port is dropped and counted
    /// ([`groups_dropped`](Self::groups_dropped)), never folded onto another
    /// Group's channels (M2-104 section 2.1.2). An empty list is one port
    /// that takes CLAP note events, as a plugin with no note port is sent.
    ///
    /// Forgets the per-note state and every MIDI 1.0 selection, and allocates
    /// it (about 270 KiB for each port): off the audio thread, at activation.
    /// The dialect is read at activation and re-activation only: a plugin that
    /// changes its note ports (`poll_note_ports_changed`) is not heard
    /// differently until the embedder re-activates it.
    pub fn set_note_ports(&mut self, midi: &[bool]) {
        let ports = midi
            .len()
            .clamp(1, usize::from(tutti_ump::MidiGroup::COUNT));
        self.ports = ports;
        self.midi_ports = midi
            .iter()
            .take(ports)
            .enumerate()
            .fold(0, |mask, (port, &midi)| mask | (u16::from(midi) << port));
        self.notes = Self::per_note_state(ports);
        self.narrower.reset();
    }

    /// Whether notes to the first input note port go out as `clap_event_midi`:
    /// see [`set_midi_notes`](Self::set_midi_notes).
    pub fn supports_midi_notes(&self) -> bool {
        self.midi_ports & 1 != 0
    }

    /// MIDI messages of a Group the plugin has no input note port for, since
    /// the list was built: each is dropped whole (a Note On, a controller, a
    /// per-note message alike), never heard on another Group's channels. Zero
    /// while the plugin has a port for every Group the host sends.
    /// System Real Time and Utility messages name no Group's channels and
    /// are not counted.
    pub fn groups_dropped(&self) -> u64 {
        self.groups_dropped
    }

    /// Creates an input event list pre-populated with the given events.
    pub fn from_events(events: Vec<ClapEvent>) -> Self {
        Self {
            // Reserved for the list's whole capacity, as `reserve` does, so
            // `sort_by_time` stays `n log n` (`flush_params` sorts one).
            order: StableOrder::with_capacity(events.capacity()),
            events,
            param_room: usize::MAX,
            param_points_dropped: 0,
            midi_dropped: 0,
            bounded: false,
            notes: Self::per_note_state(1),
            narrower: Narrower::new(),
            ports: 1,
            midi_ports: 0,
            groups_dropped: 0,
            sysex: SysexAssembler::new(Box::default(), 0, 0),
        }
    }

    /// Reserve heap capacity for at least `n` events. Off-RT only; call
    /// once during plugin activation so steady-state `add_*` calls don't
    /// touch the allocator. `clear()` keeps the reserved capacity.
    pub fn reserve(&mut self, n: usize) {
        self.bounded = true;
        self.events.reserve(n);
        self.order.reserve(self.events.capacity());
    }

    /// Bound the parameter events one
    /// [`add_param_changes`](Self::add_param_changes) pushes to `points`,
    /// the room the caller reserved for them (with [`reserve`](Self::reserve),
    /// on top of the room for MIDI and note expressions), so a block carrying
    /// more never grows the list on the audio thread. Off-RT; call it at
    /// activation, with the points the host's chunk can arrive with:
    /// [`ParameterChanges::point_budget`](tutti_plugin_types::ParameterChanges::point_budget)
    /// for the plugin's parameter count and the points a chunk can carry for
    /// one parameter
    /// ([`ParameterQueue::chunk_room`](tutti_plugin_types::ParameterQueue::chunk_room)).
    pub fn set_param_point_room(&mut self, points: usize) {
        self.param_room = points;
    }

    /// How many more MIDI events fit without growing a reserved list: its
    /// room less what is set aside for parameter points.
    fn midi_room(&self) -> usize {
        if !self.bounded {
            return usize::MAX;
        }
        let aside = if self.param_room == usize::MAX {
            0
        } else {
            self.param_room
        };
        self.events
            .capacity()
            .saturating_sub(aside)
            .saturating_sub(self.events.len())
    }

    /// MIDI events refused since the list was built, for lack of reserved
    /// room (a Program Change with a bank counts its three).
    pub fn midi_dropped(&self) -> u64 {
        self.midi_dropped
    }

    /// Parameter points [`add_param_changes`](Self::add_param_changes) could
    /// not hand on whole, past the room set with
    /// [`set_param_point_room`](Self::set_param_point_room): one that
    /// replaced its parameter's previous point (so the parameter still ends
    /// the block on its latest value), or one with no room at all. Counted
    /// since the list was built; zero while the host stays within the room
    /// it was activated for.
    pub fn param_points_dropped(&self) -> u64 {
        self.param_points_dropped
    }

    /// Reserve the SysEx reassembly buffer
    /// ([`sysex_assembler`], about 132 KiB). Off-RT; call once at
    /// activation. Until then every SysEx message is dropped and counted
    /// ([`sysex_dropped`](Self::sysex_dropped)). A list already reserved
    /// keeps its buffer, and any message open in it.
    pub fn reserve_sysex(&mut self) {
        if self.sysex.max_message() < SYSEX_MESSAGE_CAPACITY {
            self.sysex = sysex_assembler();
        }
    }

    /// SysEx messages dropped whole since the list was built: longer than
    /// [`SYSEX_MESSAGE_CAPACITY`], left without their `END` by a later
    /// `START` or a `SINGLE` on their group, past the buffer's room in one
    /// block, open at a [`reset_sysex`](Self::reset_sysex), or any message
    /// before [`reserve_sysex`](Self::reserve_sysex). Neither call zeroes
    /// it. See [`SysexAssembler`] for the policy.
    pub fn sysex_dropped(&self) -> u64 {
        self.sysex.dropped()
    }

    /// The plugin is being reset or re-activated: drop the open SysEx,
    /// counted, so its remaining packets do not complete it on the far side
    /// of the reset (the plugin has forgotten its start). Never allocates.
    pub fn reset_sysex(&mut self) {
        self.sysex.reset();
    }

    /// The plugin is being reset: it has forgotten its notes and its MIDI 1.0
    /// state. Drops what was held per note (every value, every note that still
    /// responds, every bend sensitivity), forgets which RPN, NRPN and bank each
    /// channel has had selected so the next controller selects in full, and
    /// drops the open SysEx ([`reset_sysex`](Self::reset_sysex)). Never
    /// allocates.
    pub fn reset_midi(&mut self) {
        self.notes.clear();
        self.narrower.reset();
        self.sysex.reset();
    }

    /// The plugin reported the note `note_id` ended (`CLAP_EVENT_NOTE_END`):
    /// the per-note controllers no longer reach it. A Note Off does not end a
    /// note, since its tail still responds. An id this list did not mint (a
    /// wildcard, a plugin's own) is ignored.
    pub fn note_ended(&mut self, note_id: i32) {
        if let Ok(id) = u32::try_from(note_id) {
            self.notes.end(NoteId::from_raw(id));
        }
    }

    /// Converts an `(Offset, Ump)` event to the [`ClapEvent`]s it means and
    /// appends them, at the per-note state the stream so far left
    /// (M2-104 section 7.4).
    ///
    /// **A Group is a note port** ([`set_note_ports`](Self::set_note_ports)):
    /// every channel voice message goes to the port of its Group, and one
    /// for a Group with no port is dropped and counted
    /// ([`groups_dropped`](Self::groups_dropped)). A System message names no
    /// Group's channels and goes to port 0.
    ///
    /// **Notes and per-note controllers** are the list's [`PerNoteState`]:
    /// a Note On is a `NOTE_ON` with a note id of its own, then the values the
    /// Group, channel and key held before it, then its Attribute Pitch, as
    /// note expressions; a per-note controller reaches every note of its key
    /// that has not been detached, and a Per-Note Management Detach or Reset
    /// ends or resets them. A Note Off names no id (CLAP's `-1`): the plugin
    /// matches it by port, channel and key. A per-note bend, a Registered
    /// Per-Note Controller #3 Pitch and an Attribute Pitch are one `TUNING`,
    /// in semitones from the key: the note's sounding pitch
    /// ([`PerNoteState`]'s sum of the told pitch and the bend over the
    /// channel's RPN #00/07 sensitivity), sent when it changes; a Reset puts
    /// it back to 0, and a note told nothing has none.
    ///
    /// A per-note bend sensitivity (RPN #00/07) is consumed: CLAP has no
    /// event for it. Every other CLAP-less message with a MIDI 1.0 form (a
    /// controller, channel pitch bend and pressure, a program change, an RPN
    /// or NRPN) is `clap_event_midi` bytes through the list's
    /// [`Narrower`]: an RPN or NRPN is the Control Changes of M2-104
    /// Appendix D.2.3 and a bank is sent only when it changes. UMP variants
    /// without a CLAP or MIDI 1.0 form are silently skipped. Add a block's MIDI
    /// in frame order, so a sensitivity applies to the bends after it.
    ///
    /// A SysEx7 packet goes through the list's reassembler: a multi-packet
    /// message (which may span blocks) is appended as one
    /// [`ClapEvent::MidiSysex`], `0xF0 … 0xF7`, at its `END` packet's time.
    /// Allocation-free once [`reserve_sysex`](Self::reserve_sysex)ed.
    pub fn add_midi(&mut self, event: &UmpAt) -> &mut Self {
        let (offset, ump) = *event;
        match self.sysex.push(ump) {
            SysexStep::NotSysex => {
                if let Ok(message) = decode(ump) {
                    self.add_message(offset.get(), message);
                }
            }
            SysexStep::Held => {}
            SysexStep::Complete(message) => {
                let bytes = self.sysex.message(message).bytes();
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "a message is at most SYSEX_MESSAGE_CAPACITY bytes"
                )]
                let size = bytes.len() as u32;
                let inner = clap_event_midi_sysex {
                    header: clap_event_header {
                        size: header_size::<clap_event_midi_sysex>(),
                        time: offset.get(),
                        space_id: CLAP_CORE_EVENT_SPACE_ID,
                        type_: CLAP_EVENT_MIDI_SYSEX,
                        flags: 0,
                    },
                    port_index: 0,
                    // The reassembler's buffer never grows and nothing in it
                    // moves until `clear` (`SysexAssembler::begin_block`),
                    // which empties `events` in the same call.
                    buffer: bytes.as_ptr(),
                    size,
                };
                self.events.push(ClapEvent::MidiSysex {
                    inner,
                    _data: Vec::new(),
                });
            }
        }
        self
    }

    /// One decoded message, at `time`.
    fn add_message(&mut self, time: u32, message: Message) {
        match message {
            Message::Midi1(address, voice) => match voice {
                // A note or poly pressure is widened and handled as MIDI 2.0;
                // the rest keep their 7- and 14-bit bytes.
                Voice1::NoteOn { .. } | Voice1::NoteOff { .. } | Voice1::PolyPressure { .. } => {
                    self.add_voice2(time, address, widen(voice));
                }
                Voice1::ControlChange { .. }
                | Voice1::ProgramChange(_)
                | Voice1::ChannelPressure(_)
                | Voice1::PitchBend(_) => self.add_bytes(time, address, message),
            },
            Message::Midi2(address, voice) => self.add_voice2(time, address, voice),
            Message::System(_, system) => {
                self.push_bytes(time, 0, &[system.bytes().padded()]);
            }
            // Not a note port's traffic: a CLAP plugin has no event for a clock
            // reference, a Flex Data or a Stream message. SysEx7 is assembled
            // before it gets here, and SysEx8 and Mixed Data have no CLAP
            // event. (A packet `decode` cannot frame never gets here; this host
            // sends no `CLAP_EVENT_MIDI2`.)
            Message::Utility(_)
            | Message::Sysex7(..)
            | Message::Data128(..)
            | Message::Flex(..)
            | Message::Stream(_) => {}
        }
    }

    /// The note port of `address`'s Group, or `None` (counted) when the plugin
    /// has none.
    fn port_of(&mut self, address: Address) -> Option<u16> {
        let port = u16::from(address.group.get());
        if usize::from(port) < self.ports {
            Some(port)
        } else {
            self.groups_dropped += 1;
            None
        }
    }

    /// A MIDI 2.0 channel voice message.
    fn add_voice2(&mut self, time: u32, address: Address, voice: Voice2) {
        let sensitivity = matches!(
            voice,
            Voice2::Registered { bank, index, .. }
                if Rpn::new(bank, index) == Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY
        );
        match voice {
            Voice2::NoteOn { .. }
            | Voice2::RegisteredPerNote { .. }
            | Voice2::AssignablePerNote { .. }
            | Voice2::PerNotePitchBend { .. }
            | Voice2::PolyPressure { .. }
            | Voice2::PerNoteManagement { .. } => self.add_per_note(time, address, voice),
            Voice2::Registered { .. } if sensitivity => self.add_per_note(time, address, voice),
            // CLAP note events carry no attribute, so the note plays whatever its
            // attribute type (M2-104 Table 8 reserves 0x04 to 0xFF and gives a
            // receiver no rule for them; the pitch attribute is the note's tuning).
            // A Note Off names no note: CLAP's `-1`, matched by port, channel
            // and key, as MIDI's is. Its release velocity is kept.
            Voice2::NoteOff { note, velocity, .. } => {
                let Some(port) = self.port_of(address) else {
                    return;
                };
                let off = ClapEvent::note_off(
                    time,
                    i16::from(address.channel.get()),
                    i16::from(note.get()),
                    f64::from(u16_to_unit_f32(velocity)),
                )
                .addressed(port, -1);
                let off = if self.midi_ports >> port & 1 != 0 {
                    off.into_midi_dialect(port, None)
                } else {
                    Some(off)
                };
                self.push_event(off);
            }
            // CC, channel pitch bend, channel pressure, program change (its bank
            // as Control Changes), and every RPN and NRPN: MIDI 1.0 bytes.
            // CLAP has no event for a relative controller; the narrower sends
            // nothing for it.
            Voice2::ControlChange { .. }
            | Voice2::PitchBend(_)
            | Voice2::ChannelPressure(_)
            | Voice2::ProgramChange { .. }
            | Voice2::Registered { .. }
            | Voice2::Assignable { .. }
            | Voice2::RelativeRegistered { .. }
            | Voice2::RelativeAssignable { .. } => {
                self.add_bytes(time, address, Message::Midi2(address, voice));
            }
        }
    }

    /// `message` as `clap_event_midi` on its Group's note port, through the
    /// narrower: all of its messages fit the list's room or none is staged (a
    /// bank is not sent without its program), and a refusal forgets what the
    /// narrower thinks it has selected, so the next one is sent in full.
    fn add_bytes(&mut self, time: u32, address: Address, message: Message) {
        let Some(port) = self.port_of(address) else {
            return;
        };
        // At most four: an RPN or NRPN's select and data.
        let mut staged = [[0u8; 3]; 4];
        let mut n = 0;
        self.narrower.feed(message, |bytes| {
            if n < staged.len() {
                staged[n] = bytes.padded();
                n += 1;
            }
        });
        if !self.push_bytes(time, port, &staged[..n]) {
            self.narrower.reset();
        }
    }

    /// `bytes` as MIDI events on `port`, all or none; `false` when they did not
    /// fit.
    fn push_bytes(&mut self, time: u32, port: u16, bytes: &[[u8; 3]]) -> bool {
        if self.midi_room() < bytes.len() {
            self.midi_dropped += bytes.len() as u64;
            return false;
        }
        for &data in bytes {
            self.events.push(ClapEvent::midi(time, port, data));
        }
        true
    }

    /// `event`, if there is one and it fits.
    fn push_event(&mut self, event: Option<ClapEvent>) {
        let Some(event) = event else {
            return;
        };
        if self.midi_room() == 0 {
            self.midi_dropped += 1;
        } else {
            self.events.push(event);
        }
    }

    /// A message the per-note state reads: a Note On, a per-note controller,
    /// Per-Note Management or the bend sensitivity.
    fn add_per_note(&mut self, time: u32, address: Address, voice: Voice2) {
        let Some(port) = self.port_of(address) else {
            return;
        };
        let channel = address.channel;
        let (note, velocity) = match voice {
            Voice2::NoteOn { note, velocity, .. } => (Some(note), Some(velocity)),
            Voice2::RegisteredPerNote { note, .. }
            | Voice2::AssignablePerNote { note, .. }
            | Voice2::PerNotePitchBend { note, .. }
            | Voice2::PolyPressure { note, .. } => (Some(note), None),
            _ => (None, None),
        };
        if self.midi_ports >> port & 1 != 0 {
            // A port that takes the MIDI dialect has no per-note expression
            // but poly pressure; the state still follows the stream.
            let _ = self.notes.step(address, &voice);
            let event = match (voice, note) {
                (Voice2::NoteOn { .. }, Some(note)) => ClapEvent::note_on(
                    time,
                    i16::from(channel.get()),
                    i16::from(note.get()),
                    velocity.map_or(0.0, |v| f64::from(u16_to_unit_f32(v))),
                )
                .into_midi_dialect(port, None),
                (Voice2::PolyPressure { .. }, _) => narrow(voice)
                    .map(|voice| ClapEvent::midi(time, port, voice.bytes(channel).padded())),
                _ => None,
            };
            self.push_event(event);
            return;
        }

        let mut room = self.midi_room();
        let mut dropped = 0u64;
        let mut push = |events: &mut Vec<ClapEvent>, event: Option<ClapEvent>| {
            if let Some(event) = event {
                if room == 0 {
                    dropped += 1;
                } else {
                    room -= 1;
                    events.push(event);
                }
            }
        };
        for event in self.notes.step(address, &voice) {
            match *event {
                NoteEvent::Started { id, note, .. } => {
                    let on = ClapEvent::note_on(
                        time,
                        i16::from(channel.get()),
                        i16::from(note.get()),
                        velocity.map_or(0.0, |v| f64::from(u16_to_unit_f32(v))),
                    )
                    .addressed(port, plugin_note_id(id));
                    push(&mut self.events, Some(on));
                }
                NoteEvent::Control {
                    id,
                    note: key,
                    control,
                } => {
                    // A Pitch is the note's sounding pitch, already the told
                    // pitch with its bend: CLAP's one tuning is its distance
                    // from the key.
                    let expression = control_expression(&control, key).and_then(|(ty, value)| {
                        Self::expression(time, ty, id, port, channel, Some(key), value)
                    });
                    push(&mut self.events, expression);
                }
                NoteEvent::Reset { id } => {
                    for (ty, value) in EXPRESSION_DEFAULTS {
                        let reset = Self::expression(time, ty, id, port, channel, note, value);
                        push(&mut self.events, reset);
                    }
                }
            }
        }
        self.midi_dropped += dropped;
    }

    /// A note expression of the note `id`.
    fn expression(
        time: u32,
        ty: NoteExpressionType,
        id: NoteId,
        port: u16,
        channel: MidiChannel,
        key: Option<NoteNumber>,
        value: f64,
    ) -> Option<ClapEvent> {
        ClapEvent::per_note_expression(time, ty, plugin_note_id(id), port, channel, key, value)
    }

    /// Batch version of [`add_midi`](Self::add_midi).
    pub fn add_midi_events(&mut self, events: &[UmpAt]) -> &mut Self {
        for event in events {
            self.add_midi(event);
        }
        self
    }

    /// Flatten every [`ParameterPoint`] in `changes` into a CLAP `PARAM_VALUE`
    /// event and append.
    ///
    /// Host-side automation authors values normalized `0..1`, but CLAP events
    /// carry the plugin's **plain** value (CLAP has no normalization). Each
    /// point is therefore denormalized against `ranges` (`param_id → (min,
    /// max)`) as `min + v·(max - min)`, clamped to `[min, max]`.
    ///
    /// An id missing from `ranges` passes through unchanged when
    /// `plugin_claims_params` is false — no params extension, so `0..1` is the
    /// safe reading — but is **dropped** when it is true: `parameters()`
    /// truncated at a `get_info` hole, and forwarding raw `0..1` to a param
    /// expecting, say, `100..1100` is silent and audible. Losing the automation
    /// is the lesser harm. The flag is needed because `ranges.is_empty()`
    /// cannot tell the two apart — a hole at index 0 empties the map while the
    /// plugin still claims parameters.
    ///
    /// `ranges` is **sorted by id** (each id is found by binary search, so a
    /// block with a change for every parameter costs `n log n`, not `n²`).
    ///
    /// At most the [room](Self::set_param_point_room) of points is pushed;
    /// past it, a point replaces its parameter's previous one, or is dropped
    /// when its parameter has none in the list, and either is counted
    /// ([`param_points_dropped`](Self::param_points_dropped)).
    pub fn add_param_changes(
        &mut self,
        changes: &ParameterChanges,
        ranges: &[(u32, f32, f32)],
        plugin_claims_params: bool,
    ) -> &mut Self {
        let mut pushed = 0usize;
        for queue in &changes.queues {
            let param_id = queue.param_id.get();
            let range = ranges
                .binary_search_by_key(&param_id, |&(id, _, _)| id)
                .ok()
                .map(|i| &ranges[i]);
            if range.is_none() && plugin_claims_params {
                continue;
            }
            let mut pushed_here = false;
            for point in &queue.points {
                let value = match range {
                    Some(&(_, min, max)) => {
                        // CLAP normalizes automation to f64 `0..=1`; the plain
                        // range is f32, so the product is computed in f32.
                        #[allow(
                            clippy::cast_possible_truncation,
                            reason = "normalized 0..=1 from CLAP; the plain range is f32"
                        )]
                        let normalized = point.value.get() as f32;
                        let plain = min + normalized * (max - min);
                        // `f32::clamp` panics when `lo > hi`, and `min`/`max`
                        // come straight from the plugin's own reported
                        // `min_value`/`max_value` (lifecycle.rs:93). A plugin
                        // reporting inverted bounds is malformed, but it must
                        // not panic the audio thread — order the bounds first.
                        plain.clamp(min.min(max), min.max(max)) as f64
                    }
                    None => point.value.get(),
                };
                let event = ClapEvent::param_value(
                    // `sample_offset` is `i32`; a bare `as u32` turns a
                    // negative offset into ~4.29 billion, which sorts last and
                    // is handed to the plugin as a buffer index. Saturate at 0
                    // — the change is simply already due. The upper bound is
                    // enforced once for the whole list by `clamp_times`, which
                    // is the only place `frames_count` is known.
                    point.sample_offset.max(0) as u32,
                    param_id,
                    value,
                );
                if pushed < self.param_room {
                    self.events.push(event);
                    pushed += 1;
                    pushed_here = true;
                    continue;
                }
                // Past the room: the parameter's previous point gives way, so
                // it still ends on its latest value; with none, the point is
                // lost. Counted either way.
                self.param_points_dropped += 1;
                if pushed_here && let Some(last) = self.events.last_mut() {
                    *last = event;
                }
            }
        }
        self
    }

    /// Appends one `PARAM_VALUE` event at the head of the block (time 0) for a
    /// parameter edit the Controller queued: `value` is normalized `0..=1` and
    /// is denormalized against `ranges` exactly as
    /// [`add_param_changes`](Self::add_param_changes) does a point, including
    /// its rule for an id the table lacks.
    ///
    /// An edit does not count against the parameter room: the ring that carried
    /// it is bounded, and the list reserves room for as many on top of it.
    pub fn add_param_edit(
        &mut self,
        param_id: u32,
        value: f64,
        ranges: &[(u32, f32, f32)],
        plugin_claims_params: bool,
    ) -> &mut Self {
        let range = ranges
            .binary_search_by_key(&param_id, |&(id, _, _)| id)
            .ok()
            .map(|i| &ranges[i]);
        let plain = match range {
            Some(&(_, min, max)) => {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "normalized 0..=1; the plain range is f32"
                )]
                let normalized = value as f32;
                let plain = min + normalized * (max - min);
                // Ordered bounds: the plugin's own `min`/`max` may be inverted,
                // and `f32::clamp` panics when `lo > hi`.
                plain.clamp(min.min(max), min.max(max)) as f64
            }
            // No table entry and the plugin claims parameters: the table is
            // short, and raw `0..1` into an unknown range is silent and audible.
            None if plugin_claims_params => return self,
            None => value,
        };
        self.events.push(ClapEvent::param_value(0, param_id, plain));
        self
    }

    /// Appends each [`ClapNoteExpression`] as a CLAP `NOTE_EXPRESSION` event.
    /// A `Custom` dimension is skipped — CLAP has no id for one, and
    /// [`ClapEvent::note_expression`] says why substituting would be worse.
    pub fn add_note_expressions(&mut self, expressions: &[ClapNoteExpression]) -> &mut Self {
        if self.supports_midi_notes() {
            return self;
        }
        for expr in expressions {
            if let Some(event) = ClapEvent::note_expression(
                // Same signed→unsigned trap as `add_param_changes`.
                expr.sample_offset.max(0) as u32,
                expr.expression_type,
                expr.note_id,
                expr.value,
            ) {
                self.events.push(event);
            }
        }
        self
    }

    /// Appends a `CLAP_EVENT_TRANSPORT` at frame `time` of the block: a
    /// transport change inside it, which the plugin applies from that frame
    /// on (the block's own transport, `clap_process.transport`, is the one at
    /// frame 0). Added before [`sort_by_time`](Self::sort_by_time), it takes
    /// its place in time order among the block's other events, after those
    /// already added at the same frame. Room is the caller's
    /// ([`reserve`](Self::reserve)): at most the graph's
    /// `MAX_TRANSPORT_CHANGES` a block.
    pub fn add_transport(&mut self, time: u32, mut event: clap_event_transport) -> &mut Self {
        event.header.time = time;
        self.events.push(ClapEvent::Transport(event));
        self
    }

    /// Clamps every event's `header.time` into `0..frames_count`.
    ///
    /// CLAP hands `time` to the plugin as a sample index into the block, and
    /// plugins routinely use it to split the buffer — an out-of-range value is
    /// an out-of-bounds read/write in the *plugin*. The host must not emit one,
    /// so this is the boundary check, applied to every source (MIDI frame
    /// offsets, automation `sample_offset`, note expressions) just before the
    /// list is sorted and handed over.
    ///
    /// **Clamp, not drop.** A dropped event is not a neutral outcome here: a
    /// lost NOTE_OFF is a stuck note that rings until the transport stops, and
    /// a lost PARAM_VALUE leaves the plugin on a stale value indefinitely,
    /// because both carry *state* rather than an impulse. Clamping mistimes the
    /// event by at most one block (sub-millisecond at any realistic block size)
    /// and preserves the state transition. An out-of-range time is a caller
    /// bug either way; this bounds its blast radius to timing rather than
    /// correctness.
    ///
    /// `frames_count == 0` maps everything to 0 (the block has no valid index,
    /// and the plugin will process nothing).
    pub fn clamp_times(&mut self, frames_count: u32) -> &mut Self {
        let last = frames_count.saturating_sub(1);
        for event in &mut self.events {
            let header = event.header_mut();
            if header.time > last {
                header.time = last;
            }
        }
        self
    }

    /// Stable sort events by their `header.time`. CLAP requires inputs to
    /// be in non-decreasing time order, and events on one frame keep the
    /// order they were added in (a note-off before the note-on that reuses
    /// its key, a parameter's points).
    ///
    /// **Allocation-free.** A block can carry a point for every parameter
    /// many times over, and std's stable sort allocates a scratch buffer
    /// past a small input. So the sort is a [`StableOrder`], whose scratch
    /// is reserved with the list ([`reserve`](Self::reserve)): `n log n`
    /// within that room, and past it an insertion sort rather than a grown
    /// scratch. A list already in order (the common case) is left as it is.
    pub fn sort_by_time(&mut self) -> &mut Self {
        self.order
            .sort_by_key(&mut self.events, |e| e.header().time);
        self
    }

    /// Borrows the events currently in the list.
    pub fn events(&self) -> &[ClapEvent] {
        &self.events
    }

    /// Moves the events out of the list, leaving it empty.
    pub fn take_events(&mut self) -> Vec<ClapEvent> {
        std::mem::take(&mut self.events)
    }
}

impl Default for InputEventList {
    fn default() -> Self {
        Self::new()
    }
}

impl EventList for InputEventList {
    fn len(&self) -> usize {
        self.events.len()
    }

    /// Empties the list and forgets the block's completed SysEx messages
    /// (their events go with it), keeping a message still open.
    fn clear(&mut self) {
        self.events.clear();
        self.sysex.begin_block();
    }
}

impl tutti_clap_ffi::EventSource for InputEventList {
    fn len(&self) -> usize {
        self.events.len()
    }

    /// Event `index` as the FFI crate lends it to the plugin. A SysEx
    /// message's bytes live in the reassembly buffer (or, for one a caller
    /// built, in the event's own `_data`); the FFI crate checks the event's
    /// pointer lies inside whichever it is handed.
    fn get(&self, index: usize) -> Option<EventRef<'_>> {
        Some(match self.events.get(index)? {
            ClapEvent::NoteOn(e)
            | ClapEvent::NoteOff(e)
            | ClapEvent::NoteChoke(e)
            | ClapEvent::NoteEnd(e) => EventRef::Note(e),
            ClapEvent::Midi(e) => EventRef::Midi(e),
            ClapEvent::NoteExpression(e) => EventRef::NoteExpression(e),
            ClapEvent::Transport(e) => EventRef::Transport(e),
            ClapEvent::ParamValue(e) => EventRef::ParamValue(e),
            ClapEvent::ParamMod(e) => EventRef::ParamMod(e),
            ClapEvent::ParamGestureBegin(e) | ClapEvent::ParamGestureEnd(e) => {
                EventRef::ParamGesture(e)
            }
            ClapEvent::MidiSysex { inner, _data } => EventRef::Sysex {
                event: inner,
                backing: if _data.is_empty() {
                    self.sysex.bytes()
                } else {
                    _data
                },
            },
        })
    }
}

/// How many SysEx payload buffers [`OutputEventList::reserve`] pre-warms.
/// Far below the event reserve: SysEx is rare, so one buffer per *possible*
/// event would be 256 allocations against a case that emits one. Past this the
/// pool grows itself once and then recycles.
const SYSEX_POOL_PREWARM: usize = 8;

/// Capacity each pre-warmed SysEx buffer starts with — enough for the common
/// short messages (identity replies, GS/XG mode sets, MPE config) without a
/// grow. A bulk dump grows its buffer once and keeps that capacity in the pool.
const SYSEX_PREWARM_BYTES: usize = 64;

/// Owned list that collects events produced by the plugin during
/// `clap_plugin.process()`.
///
/// As with [`InputEventList`], the plugin pushes through the FFI crate's
/// `clap_output_events` into this list
/// ([`tutti_clap_ffi::EventSink`]) for the span of the call.
pub struct OutputEventList {
    pub(crate) events: Vec<ClapEvent>,
    /// Recycled SysEx payload buffers.
    ///
    /// CLAP's `try_push` buffer is plugin-owned and valid only for that call, so
    /// the host must copy — on the audio thread, per event, per block.
    /// [`Self::clear`] moves each payload `Vec` here instead of dropping it and
    /// `try_push` refills one, so the allocator is touched only for a payload
    /// larger than any buffer yet seen.
    ///
    /// Whole `Vec`s rather than one arena: `ClapEvent::MidiSysex`'s `buffer`
    /// pointer aliases its own payload, and an arena reallocating mid-fill would
    /// dangle every pointer already handed out.
    pub(crate) sysex_pool: Vec<Vec<u8>>,
    /// The events other than `PARAM_VALUE` `try_push` accepts in one block:
    /// the room reserved with [`reserve`](Self::reserve). Unbounded until set.
    /// Parameter events never take from it, so a parameter flood cannot
    /// refuse a note-off.
    room: usize,
    /// The `PARAM_VALUE` events `try_push` keeps in the list in one block:
    /// the room set with [`set_param_room`](Self::set_param_room). Unbounded
    /// until set.
    pub(crate) param_room: usize,
    /// `PARAM_VALUE` events in the list this block.
    param_pushed: usize,
    /// The plugin's parameter ids, sorted: the keys of `latest`.
    latest_ids: Vec<u32>,
    /// Per parameter (index-parallel to `latest_ids`), the latest
    /// `PARAM_VALUE` pushed this block past `param_room`, so a flood still
    /// delivers each parameter's final value.
    latest: Vec<Option<clap_event_param_value>>,
    /// The `latest` entries filled this block, in the order first filled.
    /// Reserved for every parameter, so pushing never grows it.
    touched: Vec<u32>,
    /// `latest` values overwritten by a later one this block: points lost
    /// in a flood, which [`fill_param_changes`](Self::fill_param_changes)
    /// adds to the count it returns.
    coalesced: u64,
    /// Pushes `try_push` refused since the list was built: see
    /// [`events_refused`](Self::events_refused).
    refused: u64,
}

impl std::fmt::Debug for OutputEventList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputEventList")
            .field("events", &self.events)
            .field("room", &self.room)
            .field("param_room", &self.param_room)
            .field("param_pushed", &self.param_pushed)
            .field("latest", &self.latest)
            .field("coalesced", &self.coalesced)
            .field("refused", &self.refused)
            .finish_non_exhaustive()
    }
}

impl OutputEventList {
    /// Creates an empty output event list.
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            sysex_pool: Vec::new(),
            room: usize::MAX,
            param_room: usize::MAX,
            param_pushed: 0,
            latest_ids: Vec::new(),
            latest: Vec::new(),
            touched: Vec::new(),
            coalesced: 0,
            refused: 0,
        }
    }

    /// Borrows the events the plugin has pushed.
    pub fn events(&self) -> &[ClapEvent] {
        &self.events
    }

    /// Pushes the plugin's `try_push` made past their room, each refused
    /// (the plugin was told `false`) rather than grown into: an event other
    /// than `PARAM_VALUE` past the room set with [`reserve`](Self::reserve),
    /// or a `PARAM_VALUE` past the room set with
    /// [`set_param_room`](Self::set_param_room) whose id is not one of the
    /// plugin's parameters (a listed one keeps its latest value instead), or
    /// an event of another space or smaller than its type's struct.
    /// Counted since the list was built; zero while the plugin stays within
    /// the room.
    pub fn events_refused(&self) -> u64 {
        self.refused
    }

    /// Move all events out of the list, leaving it empty.
    pub fn take_events(&mut self) -> Vec<ClapEvent> {
        std::mem::take(&mut self.events)
    }

    /// Reserve room for `n` events other than `PARAM_VALUE` (notes, MIDI,
    /// note expressions, SysEx, gestures), and **bound the plugin's pushes
    /// of them to it**: once the block holds `n` of them, `try_push` refuses
    /// (returns `false`, as CLAP lets it, and a plugin must handle) and
    /// counts the refusal ([`events_refused`](Self::events_refused)), so the
    /// plugin's callback never grows the list on the audio thread. Parameter
    /// events have their own room ([`set_param_room`](Self::set_param_room))
    /// and never take from this one. Off-RT only; call it at activation.
    ///
    /// Also pre-warms the SysEx payload pool: the pool is fed only by `clear`,
    /// so without priming, the first block emitting SysEx still allocates on
    /// the audio thread.
    pub fn reserve(&mut self, n: usize) {
        self.room = n;
        self.reserve_events();

        let wanted = n.min(SYSEX_POOL_PREWARM);
        self.sysex_pool.reserve(wanted);
        while self.sysex_pool.len() < wanted {
            self.sysex_pool
                .push(Vec::with_capacity(SYSEX_PREWARM_BYTES));
        }
    }

    /// Bound the `PARAM_VALUE` events the plugin's `try_push` keeps in the
    /// list in one block to `points`, and key the latest-value table by
    /// `param_ids` (the plugin's parameter ids). Off-RT; call it at
    /// activation, with the point budget
    /// ([`ParameterChanges::point_budget`]) for the plugin's parameter count
    /// and the points a block can carry for one parameter
    /// ([`ParameterQueue::chunk_room`]), and again when the block size
    /// changes. **Grows only:** a smaller `points` keeps the larger room.
    ///
    /// Past the room, a push for one of `param_ids` is accepted into the
    /// table instead of the list: the table holds each parameter's latest
    /// such value, and [`fill_param_changes`](Self::fill_param_changes) folds
    /// it in as that parameter's last point, so a flood loses middle points
    /// (counted), never a parameter's final value. A push for any other id
    /// is refused and counted ([`events_refused`](Self::events_refused)).
    pub fn set_param_room(&mut self, points: usize, param_ids: &[u32]) {
        self.param_room = if self.param_room == usize::MAX {
            points
        } else {
            self.param_room.max(points)
        };
        let mut ids = param_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        self.latest = vec![None; ids.len()];
        self.touched = Vec::with_capacity(ids.len());
        self.latest_ids = ids;
        self.coalesced = 0;
        self.reserve_events();
    }

    /// Capacity for both rooms, once both are bounded; grows only.
    fn reserve_events(&mut self) {
        if self.room != usize::MAX {
            let params = if self.param_room == usize::MAX {
                0
            } else {
                self.param_room
            };
            let wanted = self.room.saturating_add(params);
            self.events
                .reserve(wanted.saturating_sub(self.events.len()));
        }
    }

    /// A `PARAM_VALUE` the plugin pushed past the parameter room: kept as its
    /// parameter's latest value when the id is one of the plugin's (a binary
    /// search, no scan), refused otherwise.
    fn coalesce_param(&mut self, e: &clap_event_param_value) -> bool {
        let Ok(i) = self.latest_ids.binary_search(&e.param_id) else {
            self.refused += 1;
            return false;
        };
        if self.latest[i].replace(*e).is_some() {
            self.coalesced += 1;
        } else {
            // Each index enters `touched` once a block, and `touched` is
            // reserved for every parameter: this push never grows it.
            #[allow(
                clippy::cast_possible_truncation,
                reason = "an index into the plugin's parameter list, which CLAP counts in u32"
            )]
            self.touched.push(i as u32);
        }
        true
    }

    /// Extracts MIDI events from the output as UMP packets with the time the
    /// plugin reported ([`WireMidi`]),
    /// dropping non-MIDI events.
    #[must_use]
    pub fn to_midi_events(&self) -> Vec<WireMidi> {
        let mut out = Vec::new();
        for e in &self.events {
            match e.sysex_bytes() {
                // A message with a payload byte of 128 or more is malformed: dropped.
                Some((bytes, time)) => out.extend(
                    sysex_packets(bytes)
                        .into_iter()
                        .flatten()
                        .map(|ump| WireMidi { offset: time, ump }),
                ),
                None => out.extend(e.to_midi()),
            }
        }
        out
    }

    /// RT-safe variant of [`Self::to_midi_events`] that drains into a
    /// caller-supplied [`RtMidiEvents`]. Clears `out` first. `out` is an
    /// `RtVec`, whose capacity is inline and fixed, so this never allocates:
    /// events past that capacity are dropped and `out` reports the overflow.
    /// A plugin's SysEx arrives as UMP SysEx7 packets
    /// ([`push_sysex`]): any length the pool
    /// has room for, whole or dropped.
    pub fn fill_midi_events(&self, out: &mut RtMidiEvents) {
        out.clear();
        for e in &self.events {
            if let Some((bytes, time)) = e.sysex_bytes() {
                // Whole or not at all: a message that does not fit is dropped
                // (and reported through `is_overflowed`), and later events
                // still get the room that is left.
                push_sysex(out, time, bytes);
            } else if let Some(midi) = e.to_midi()
                && !out.push(midi)
            {
                break;
            }
        }
    }

    /// Extracts parameter-value events into a [`ParameterChanges`] grouped
    /// by parameter ID. Allocates as it needs to and keeps every point, each
    /// queue sized to its points; off the audio thread only.
    #[must_use]
    pub fn to_param_changes(&self) -> ParameterChanges {
        let mut grouped: Vec<(ParamId, Vec<ParameterPoint>)> = Vec::new();
        for (addr, point) in self.param_points() {
            match grouped.iter_mut().find(|(a, _)| *a == addr) {
                Some((_, points)) => points.push(point),
                None => grouped.push((addr, vec![point])),
            }
        }
        let mut changes = ParameterChanges::new();
        for (addr, points) in grouped {
            changes.add_queue(ParameterQueue::from_points(addr, points));
        }
        changes
    }

    /// RT-safe variant of [`Self::to_param_changes`] that drains into a
    /// caller-supplied `ParameterChanges`, **bounded by the room `out` was
    /// reserved with**: build it once, off the audio thread, with
    /// [`ParameterChanges::reserved`] (the plugin's parameter count, and
    /// [`ParameterQueue::chunk_room`] for the activated block size).
    ///
    /// `out` is emptied with [`ParameterChanges::clear`], which keeps every
    /// queue's point storage as a spare, and each parameter's queue is taken
    /// back with [`ParameterChanges::open_queue`], so a block within the room
    /// neither allocates nor frees. Groups events by `param_id` with a linear
    /// scan over the open queues.
    ///
    /// Past the room nothing grows. A point whose queue is full (its room, or
    /// its own few points with `out`'s shared point pool spent,
    /// [`ParameterChanges::make_room`]) replaces that queue's last point, so
    /// the parameter still ends the block on its latest value; a point for a
    /// parameter with no queue left to open is lost.
    /// Each parameter's latest value pushed past the list's own parameter
    /// room ([`set_param_room`](Self::set_param_room)) is folded in after the
    /// list's points, as its last. Returns how many points were lost any of
    /// those ways (or overwritten in that table), for the caller to count.
    #[must_use = "the points lost past the room; a caller counts them"]
    pub fn fill_param_changes(&self, out: &mut ParameterChanges) -> u64 {
        debug_assert!(
            out.capacity() > 0 || self.param_points().next().is_none(),
            "fill_param_changes into a set with no reserved room drops every \
             parameter event: reserve it with ParameterChanges::reserved"
        );
        out.clear();
        let mut dropped = self.coalesced;
        for (addr, point) in self.param_points() {
            // Linear scan: distinct param_ids per block are typically few, and
            // the scan touches only the queues opened this block.
            let i = match out.queues.iter().position(|q| q.param_id == addr) {
                Some(i) => i,
                None if out.open_queue(addr) => out.queues.len() - 1,
                None => {
                    dropped += 1;
                    continue;
                }
            };
            // Room of its own, or a block from `out`'s shared point pool;
            // past both the queue keeps its latest value.
            if !out.push_point(i, point.sample_offset, point.value.get()) {
                dropped += 1;
            }
        }
        dropped
    }

    /// Each `PARAM_VALUE` event in the list as the queue it belongs to and
    /// the point it adds, in list order, then each parameter's latest value
    /// pushed past the parameter room.
    fn param_points(&self) -> impl Iterator<Item = (ParamId, ParameterPoint)> + '_ {
        let listed = self.events.iter().filter_map(|event| match event {
            ClapEvent::ParamValue(e) => Some(e),
            _ => None,
        });
        let latest = self
            .touched
            .iter()
            .filter_map(|&i| self.latest[i as usize].as_ref());
        listed.chain(latest).map(|e| {
            let point = ParameterPoint {
                // Saturate rather than wrap: `header.time` is `u32` and
                // `ParameterPoint::sample_offset` is `i32`, so a bare cast turns
                // a large offset negative and the automation point lands before
                // the block.
                sample_offset: i32::try_from(e.header.time).unwrap_or(i32::MAX),
                // Inbound from the plugin: clamped at the boundary, since a
                // plugin's output event is as untrusted as any other input.
                value: Normalized::clamped(e.value),
            };
            // Outbound: a CLAP id is opaque, so it is tagged as such rather
            // than travelling as a number the receiver has to guess about.
            (e.param_id.into(), point)
        })
    }

    /// Extracts note-expression events into the safe
    /// [`ClapNoteExpression`] form, dropping other events.
    pub fn to_note_expressions(&self) -> Vec<ClapNoteExpression> {
        self.events
            .iter()
            .filter_map(clap_event_to_note_expression)
            .collect()
    }

    /// RT-safe variant of [`Self::to_note_expressions`] that drains into a
    /// caller-supplied [`RtNoteExpressions`], an `RtVec` of fixed inline
    /// capacity: it never allocates, and expressions past the capacity are
    /// dropped and reported by `out`.
    pub fn fill_note_expressions(&self, out: &mut RtNoteExpressions) {
        out.clear();
        for event in &self.events {
            if let Some(ne) = clap_event_to_note_expression(event)
                && !out.push(ne)
            {
                break;
            }
        }
    }

    /// Drains the plugin's output-side param *gestures* (begin/end) and
    /// param *modulation* events into a caller-supplied pooled `Vec`, clearing
    /// it first. These carry information (a knob-drag beginning/ending, or an
    /// output modulation) that the shared param-value vocabulary can't express,
    /// so rather than drop them silently (`fill_param_changes` only matches
    /// `ParamValue`) they are surfaced here for CLAP-aware callers. The gesture
    /// and mod C structs are POD, so this reconstructs (not clones) each event.
    pub fn fill_gestures(&self, out: &mut Vec<ClapEvent>) {
        out.clear();
        for event in &self.events {
            match event {
                ClapEvent::ParamGestureBegin(e) => out.push(ClapEvent::ParamGestureBegin(*e)),
                ClapEvent::ParamGestureEnd(e) => out.push(ClapEvent::ParamGestureEnd(*e)),
                ClapEvent::ParamMod(e) => out.push(ClapEvent::ParamMod(*e)),
                _ => {}
            }
        }
    }
}

/// Decodes a single [`ClapEvent`] into a [`ClapNoteExpression`]. Returns
/// `None` for unsupported expression ids or non-NoteExpression events.
fn clap_event_to_note_expression(event: &ClapEvent) -> Option<ClapNoteExpression> {
    let ClapEvent::NoteExpression(ne) = event else {
        return None;
    };
    let expression_type = match ne.expression_id {
        id if id == CLAP_NOTE_EXPRESSION_VOLUME => NoteExpressionType::Volume,
        id if id == CLAP_NOTE_EXPRESSION_PAN => NoteExpressionType::Pan,
        id if id == CLAP_NOTE_EXPRESSION_TUNING => NoteExpressionType::Tuning,
        id if id == CLAP_NOTE_EXPRESSION_VIBRATO => NoteExpressionType::Vibrato,
        id if id == CLAP_NOTE_EXPRESSION_BRIGHTNESS => NoteExpressionType::Brightness,
        id if id == CLAP_NOTE_EXPRESSION_PRESSURE => NoteExpressionType::Pressure,
        id if id == CLAP_NOTE_EXPRESSION_EXPRESSION => NoteExpressionType::Expression,
        _ => return None,
    };
    Some(ClapNoteExpression {
        // As in the parameter path above: `u32` time into an `i32` offset
        // saturates rather than wrapping negative.
        sample_offset: i32::try_from(ne.header.time).unwrap_or(i32::MAX),
        note_id: ne.note_id,
        port_index: ne.port_index,
        channel: ne.channel,
        key: ne.key,
        expression_type,
        value: ne.value,
    })
}

impl Default for OutputEventList {
    fn default() -> Self {
        Self::new()
    }
}

impl EventList for OutputEventList {
    fn len(&self) -> usize {
        self.events.len()
    }

    /// Empty the list, **recycling** SysEx payload buffers into the internal
    /// `sysex_pool` rather than freeing them. `process` calls this every
    /// block; a plain `events.clear()` would drop the `Vec<u8>` inside each
    /// `MidiSysex`, so every block would free here what `try_push` allocates.
    fn clear(&mut self) {
        self.param_pushed = 0;
        self.coalesced = 0;
        for &i in &self.touched {
            self.latest[i as usize] = None;
        }
        self.touched.clear();
        for event in self.events.drain(..) {
            if let ClapEvent::MidiSysex { _data, .. } = event {
                // `_data`'s capacity is what is worth keeping; the length is
                // reset so `try_push` sees an empty buffer to extend into.
                let mut buf = _data;
                buf.clear();
                self.sysex_pool.push(buf);
            }
        }
    }
}

impl EventSink for OutputEventList {
    fn refuse_malformed(&mut self) {
        self.refused += 1;
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "SysEx payloads are bounded well below u32::MAX"
    )]
    fn push(&mut self, event: PluginEvent<'_>) -> bool {
        if let PluginEvent::ParamValue(e) = &event {
            if self.param_pushed >= self.param_room {
                return self.coalesce_param(e);
            }
        } else if self.events.len().saturating_sub(self.param_pushed) >= self.room {
            self.refused += 1;
            return false;
        }

        match event {
            PluginEvent::Note(e) => match e.header.type_ {
                CLAP_EVENT_NOTE_ON => self.events.push(ClapEvent::NoteOn(e)),
                CLAP_EVENT_NOTE_OFF => self.events.push(ClapEvent::NoteOff(e)),
                CLAP_EVENT_NOTE_CHOKE => self.events.push(ClapEvent::NoteChoke(e)),
                CLAP_EVENT_NOTE_END => self.events.push(ClapEvent::NoteEnd(e)),
                _ => return false,
            },
            PluginEvent::Midi(e) => self.events.push(ClapEvent::Midi(e)),
            PluginEvent::NoteExpression(e) => self.events.push(ClapEvent::NoteExpression(e)),
            PluginEvent::ParamValue(e) => {
                self.events.push(ClapEvent::ParamValue(e));
                self.param_pushed += 1;
            }
            PluginEvent::ParamMod(e) => self.events.push(ClapEvent::ParamMod(e)),
            PluginEvent::ParamGesture(e) => match e.header.type_ {
                CLAP_EVENT_PARAM_GESTURE_BEGIN => self.events.push(ClapEvent::ParamGestureBegin(e)),
                CLAP_EVENT_PARAM_GESTURE_END => self.events.push(ClapEvent::ParamGestureEnd(e)),
                _ => return false,
            },
            PluginEvent::Sysex { event, data } => {
                if !data.is_empty() {
                    let mut payload = self.sysex_pool.pop().unwrap_or_default();
                    payload.clear();
                    payload.extend_from_slice(data);
                    // The buffer pointer aliases `payload`; the `ClapEvent`
                    // owns it, and it is never moved out of its list.
                    let inner = clap_event_midi_sysex {
                        header: event.header,
                        port_index: event.port_index,
                        buffer: payload.as_ptr(),
                        size: payload.len() as u32,
                    };
                    self.events.push(ClapEvent::MidiSysex {
                        inner,
                        _data: payload,
                    });
                }
            }
        }
        true
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
#[cfg(test)]
mod tests {
    use super::*;
    use tutti_fixtures::midi as fx;

    use tutti_graph::Offset;
    use tutti_plugin_types::{MIDI_STACK_CAPACITY, RT_MIDI_CAPACITY};
    use tutti_types::Samples;
    use tutti_ump::{Data128, Sysex8Packet, SysexStatus, System};

    /// A block long enough for every offset these tests use (the largest is
    /// 9,999).
    const BLOCK: usize = 10_000;

    /// A note started by a Note On has an id of its own, above every
    /// `(channel, note)` pair's (`tutti_ump::NoteId::MAX_CHANNEL_NOTE`).
    fn assert_minted(note_id: i32) {
        assert!(
            note_id > tutti_ump::NoteId::MAX_CHANNEL_NOTE as i32,
            "note id {note_id} is a (channel, key) pair's or a wildcard"
        );
    }

    /// The offset `k` frames into the block.
    fn at(k: u32) -> Offset {
        Offset::new(k as usize, Samples(BLOCK)).expect("inside the block")
    }

    /// The packet of `words`, zero-padded to four, at offset `k`.
    fn packet(k: u32, words: &[u32]) -> UmpAt {
        let mut w = [0u32; 4];
        w[..words.len()].copy_from_slice(words);
        (at(k), Ump(w))
    }

    /// The MIDI 1.0 bytes as a packet at offset 0.
    fn midi1_event(bytes: &[u8]) -> UmpAt {
        (
            at(0),
            fx::from_midi1_bytes(bytes).expect("a MIDI 1.0 message"),
        )
    }

    /// A note expression for the legacy `(channel, note)` id, on port 0.
    fn per_note_expression(
        time: u32,
        ty: NoteExpressionType,
        channel: MidiChannel,
        note: NoteNumber,
        value: f64,
    ) -> Option<ClapEvent> {
        ClapEvent::per_note_expression(
            time,
            ty,
            note_id_for(channel, note),
            0,
            channel,
            Some(note),
            value,
        )
    }

    /// The first event `event` adds to a fresh list: the one CLAP event a
    /// packet is, at the default per-note state.
    fn from_midi(event: &UmpAt) -> Option<ClapEvent> {
        let mut list = InputEventList::new();
        list.add_midi(event);
        list.events.into_iter().next()
    }

    /// A Note On at the address and key of the per-note `event`, at velocity
    /// 0x8000.
    fn note_on_for(event: &UmpAt) -> Option<UmpAt> {
        let Ok(Message::Midi2(address, voice)) = decode(event.1) else {
            return None;
        };
        let note = match voice {
            Voice2::PolyPressure { note, .. }
            | Voice2::PerNotePitchBend { note, .. }
            | Voice2::RegisteredPerNote { note, .. }
            | Voice2::AssignablePerNote { note, .. } => note,
            _ => return None,
        };
        let on = Message::Midi2(address, Voice2::note_on(note, 0x8000));
        Some((event.0, Ump::from(on)))
    }

    /// [`from_midi`] for a per-note message, with its note already sounding:
    /// the first event the message adds once a Note On on its key has
    /// started (a per-note controller of a number with no note is held, and
    /// makes none: M2-104 Appendix C.3).
    fn from_midi_live(event: &UmpAt) -> Option<ClapEvent> {
        let mut list = InputEventList::new();
        if let Some(on) = note_on_for(event) {
            list.add_midi(&on);
            list.take_events();
        }
        list.add_midi(event);
        list.events.into_iter().next()
    }

    /// The message `midi` carries.
    fn message(midi: &WireMidi) -> Message {
        decode(midi.ump).expect("a defined message")
    }

    /// A huge event time must saturate, not wrap negative.
    ///
    /// `clap_event_header::time` is `u32` and `ParameterPoint::sample_offset` is
    /// `i32`, so a bare `as i32` sends a large offset negative — the automation
    /// point then sorts before the block instead of after it.
    ///
    /// `u32::MAX` is not a realistic block offset, but it is the value that
    /// distinguishes saturation from wrapping, which is the whole property.
    #[test]
    fn a_huge_event_time_saturates_rather_than_going_negative() {
        let mut list = OutputEventList::new();
        list.events.push(ClapEvent::param_value(u32::MAX, 7, 0.5));

        // `fill_param_changes` fills within the room the set was reserved
        // with (an unreserved set has none); one queue is all this needs.
        let mut changes = ParameterChanges::reserved(1, 10);
        assert_eq!(
            list.fill_param_changes(&mut changes),
            0,
            "nothing past the room"
        );

        let point = changes
            .queues
            .iter()
            .find(|q| q.param_id == 7u32.into())
            .and_then(|q| q.points.first())
            .expect("the param value must survive into a queue");
        assert!(
            point.sample_offset >= 0,
            "sample_offset wrapped negative ({}) — the automation point sorts \
             before the block it belongs to",
            point.sample_offset
        );
        assert_eq!(
            point.sample_offset,
            i32::MAX,
            "an unrepresentable offset must saturate"
        );
    }

    // (`use super::*` is already in scope from the top of this module.)
    use tutti_midi_protocol::{signed_f32_to_bend_u32, unit_f32_to_u32};

    // --- Param-change denormalization (host 0..1 → CLAP plain) ---

    fn first_param_value(list: &InputEventList) -> f64 {
        list.events()
            .iter()
            .find_map(|e| match e {
                ClapEvent::ParamValue(v) => Some(v.value),
                _ => None,
            })
            .expect("a PARAM_VALUE event")
    }

    /// **Every parameter is found in a sorted range list**, not only the
    /// first: the ranges are binary-searched by id, so each queue
    /// denormalizes against its own parameter's range.
    #[test]
    fn add_param_changes_finds_each_range_in_a_sorted_list() {
        let ranges = [(1, 0.0, 1.0), (5, 0.0, 10.0), (9, 100.0, 1100.0)];
        let mut changes = ParameterChanges::reserved(2, 1);
        changes.add_change(9u32.into(), 0, 0.5);
        changes.add_change(5u32.into(), 1, 0.25);
        let mut list = InputEventList::new();
        list.add_param_changes(&changes, &ranges, true);
        let got: Vec<(u32, f64)> = list
            .events()
            .iter()
            .filter_map(|e| match e {
                ClapEvent::ParamValue(v) => Some((v.param_id, v.value)),
                _ => None,
            })
            .collect();
        assert_eq!(got, vec![(9, 600.0), (5, 2.5)]);
    }

    /// A banked Program Change reaches the plugin as CC 0, CC 32 and PC, at one
    /// time, in that order.
    #[test]
    fn a_banked_program_change_is_three_midi_events() {
        let pc = (
            at(5),
            fx::program_change(MidiGroup::FIRST, MidiChannel::of(1), 9, Some(0x0085)),
        );
        let mut list = InputEventList::new();
        list.reserve(8);
        list.add_midi(&pc);
        let got: Vec<(u32, [u8; 3])> = list
            .events()
            .iter()
            .map(|e| match e {
                ClapEvent::Midi(m) => (m.header.time, m.data),
                _ => panic!("not a MIDI event"),
            })
            .collect();
        assert_eq!(
            got,
            vec![(5, [0xB1, 0, 1]), (5, [0xB1, 32, 5]), (5, [0xC1, 9, 0])]
        );
        // Room for two: none of the three is staged, and all are counted.
        let mut tight = InputEventList::new();
        tight.reserve(2);
        let room = tight.events.capacity();
        for _ in 0..room.saturating_sub(1) {
            tight.add_midi(&(
                at(0),
                fx::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, fx::MID_VELOCITY),
            ));
        }
        let before = tight.events().len();
        tight.add_midi(&pc);
        assert_eq!(tight.events().len(), before, "all or none");
        assert_eq!(tight.midi_dropped(), 3);
        // A plain event past the room is refused too, never grown into.
        let cap = tight.events.capacity();
        while tight.events().len() < cap {
            tight.add_midi(&(
                at(0),
                fx::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 61, fx::MID_VELOCITY),
            ));
        }
        tight.add_midi(&(
            at(0),
            fx::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 62, fx::MID_VELOCITY),
        ));
        assert_eq!(tight.events().len(), cap);
        assert_eq!(tight.events.capacity(), cap, "did not grow");
        assert_eq!(tight.midi_dropped(), 4);
    }

    /// A Program Change packet written from M2-104 Figure 64's words (bank MSB
    /// 1 at bits 14-8, LSB 5 at bits 6-0) reaches the plugin as CC 0 = 1,
    /// CC 32 = 5, then the program. Unlike the builder test above, no builder
    /// is in the path.
    #[test]
    fn a_spec_word_banked_program_change_is_three_midi_events() {
        let pc = packet(5, &[0x40C1_0001, (9 << 24) | (1 << 8) | 5]);
        let mut list = InputEventList::new();
        list.reserve(8);
        list.add_midi(&pc);
        let got: Vec<[u8; 3]> = list
            .events()
            .iter()
            .map(|e| match e {
                ClapEvent::Midi(m) => m.data,
                _ => panic!("not a MIDI event"),
            })
            .collect();
        assert_eq!(got, vec![[0xB1, 0, 1], [0xB1, 32, 5], [0xC1, 9, 0]]);
    }

    /// **A MIDI 1.0 Note On at velocity 0 is a Note Off at release velocity
    /// 0x8000** (M2-104 Appendix D.3.1), so the plugin hears a release at
    /// `0x8000 / 0xFFFF`, not 0. A native MIDI 2.0 Note On at velocity 0 is a
    /// note-on at 0.0 (M2-104 §7.4.2: it "does not function as a Note Off").
    #[test]
    fn a_midi1_note_on_at_velocity_zero_releases_at_half() {
        let midi1 = packet(0, &[0x2091_3C00]);
        let Some(ClapEvent::NoteOff(e)) = from_midi(&midi1) else {
            panic!("a MIDI 1.0 Note On at velocity 0 is a Note Off");
        };
        assert_eq!((e.channel, e.key), (1, 60));
        assert!(
            (e.velocity - 32768.0 / 65535.0).abs() < 1e-6,
            "{}",
            e.velocity
        );

        let midi2 = packet(0, &[0x4091_3C00, 0]);
        let Some(ClapEvent::NoteOn(e)) = from_midi(&midi2) else {
            panic!("a MIDI 2.0 Note On at velocity 0 is a note-on");
        };
        assert_eq!((e.channel, e.key, e.velocity), (1, 60, 0.0));
    }

    /// **A reserved attribute type does not stop the note** (M2-104 Table 8,
    /// section 7.4.14: reserved for senders, no receiver rule), which plays at
    /// its velocity.
    #[test]
    fn a_note_with_a_reserved_attribute_type_still_plays() {
        for attribute_type in [0x00, 0x01, 0x02, 0x03, 0x04, 0x81, 0xFF] {
            for (opcode, on) in [(0x9, true), (0x8, false)] {
                let midi = packet(
                    0,
                    &[
                        0x4000_0000 | (opcode << 20) | (2 << 16) | (60 << 8) | attribute_type,
                        0xFFFF_1234,
                    ],
                );
                match from_midi(&midi) {
                    Some(ClapEvent::NoteOn(e)) if on => assert_eq!(e.velocity, 1.0),
                    Some(ClapEvent::NoteOff(e)) if !on => assert_eq!(e.velocity, 1.0),
                    other => panic!("attribute {attribute_type:#x}: got {other:?}"),
                }
            }
        }
    }

    /// **`Unknown` is not forwarded**, because the host sends no
    /// `CLAP_EVENT_MIDI2`: a reserved message type, and a defined type with an
    /// undefined status, yield nothing, in either note dialect, and are not
    /// counted as dropped for want of room.
    #[test]
    fn an_unknown_packet_yields_no_clap_event() {
        for midi_notes in [false, true] {
            let mut list = InputEventList::new();
            list.reserve(8);
            list.set_midi_notes(midi_notes);
            for words in [
                [0x6000_0000u32, 0, 0, 0],
                [0x7000_0000, 0, 0, 0],
                [0x0FFF_0000, 0, 0, 0],
                [0x4000_0000 | (0x7 << 20), 0, 0, 0],
            ] {
                assert!(decode(Ump(words)).is_err(), "{words:08x?} is framed");
                list.add_midi(&packet(3, &words));
            }
            assert!(list.events().is_empty(), "{:?}", list.events());
            assert_eq!(list.midi_dropped(), 0);
        }
    }

    /// **A plugin's MIDI 1.0 Note On at velocity 0 reaches the engine as a Note
    /// Off at 0x8000**, the same widening as on the way in.
    #[test]
    fn a_plugin_midi1_note_on_at_velocity_zero_is_a_note_off_at_half() {
        let midi = ClapEvent::midi(4, 0, [0x93, 61, 0])
            .to_midi()
            .expect("converts");
        assert_eq!(midi.offset, 4);
        match message(&midi) {
            Message::Midi2(address, Voice2::NoteOff { note, velocity, .. }) => {
                assert_eq!((address.channel.get(), note.get()), (3, 61));
                assert_eq!(velocity, fx::MID_VELOCITY);
            }
            other => panic!("expected a MIDI 2.0 Note Off, got {other:?}"),
        }
    }

    /// **Every kind of message decodes and converts without allocating**, in
    /// both note dialects: each channel voice message of MIDI 1.0 and 2.0, a
    /// banked Program Change, the per-note bend sensitivity, System, Utility,
    /// SysEx7, SysEx8, Flex, Stream, and a packet `decode` cannot frame.
    #[test]
    fn every_message_kind_converts_without_allocating() {
        let sysex8 = Ump::from(Message::Data128(
            MidiGroup::FIRST,
            Data128::Sysex8(Sysex8Packet::new(SysexStatus::Complete, 1, &[1, 2, 3]).unwrap()),
        ))
        .0;
        let table: Vec<(&str, [u32; 4])> = vec![
            ("utility", [0x0000_0000, 0, 0, 0]),
            ("system", [0x10F8_0000, 0, 0, 0]),
            ("system", [0x10F2_0102, 0, 0, 0]),
            ("midi1", [0x2091_3C40, 0, 0, 0]),
            ("midi1", [0x2081_3C40, 0, 0, 0]),
            ("midi1", [0x20A1_3C40, 0, 0, 0]),
            ("midi1", [0x20B1_0740, 0, 0, 0]),
            ("midi1", [0x20C1_0500, 0, 0, 0]),
            ("midi1", [0x20D1_4000, 0, 0, 0]),
            ("midi1", [0x20E1_0040, 0, 0, 0]),
            ("sysex7", [0x3001_0102, 0x0304_0506, 0, 0]),
            ("midi2", [0x4091_3C00, 0xC000_0000, 0, 0]),
            ("midi2", [0x4081_3C00, 0x4000_0000, 0, 0]),
            ("midi2", [0x40A1_3C00, 0x8000_0000, 0, 0]),
            ("midi2", [0x40B1_0700, 0x8000_0000, 0, 0]),
            ("midi2", [0x40C1_0001, (5 << 24) | (1 << 8) | 2, 0, 0]),
            ("midi2", [0x40D1_0000, 0x8000_0000, 0, 0]),
            ("midi2", [0x40E1_0000, 0x8000_0000, 0, 0]),
            ("midi2", [0x4061_3C00, 0x8000_0000, 0, 0]),
            ("midi2", [0x4001_3C07, 0x8000_0000, 0, 0]),
            ("midi2", [0x4011_3C4A, 0x8000_0000, 0, 0]),
            ("midi2", [0x4021_0007, 24 << 25, 0, 0]),
            ("midi2", [0x4031_0102, 0x8000_0000, 0, 0]),
            ("midi2", [0x40F1_3C01, 0, 0, 0]),
            ("data128", sysex8),
            ("flex", [0xD010_0000, 0, 0, 0]),
            ("stream", [0xF000_0000, 0, 0, 0]),
            ("unknown", [0x6000_0000, 0, 0, 0]),
        ];
        // Every packet is the kind it claims, so none tests nothing.
        for (claimed, words) in &table {
            let kind = match decode(Ump(*words)) {
                Ok(Message::Utility(_)) => "utility",
                Ok(Message::System(..)) => "system",
                Ok(Message::Midi1(..)) => "midi1",
                Ok(Message::Sysex7(..)) => "sysex7",
                Ok(Message::Midi2(..)) => "midi2",
                Ok(Message::Data128(..)) => "data128",
                Ok(Message::Flex(..)) => "flex",
                Ok(Message::Stream(_)) => "stream",
                Err(_) => "unknown",
            };
            assert_eq!(kind, *claimed, "{words:08x?}");
        }
        let packets: Vec<UmpAt> = table.iter().map(|(_, words)| packet(2, words)).collect();
        for midi_notes in [false, true] {
            let mut list = InputEventList::new();
            list.reserve(64);
            list.reserve_sysex();
            list.set_midi_notes(midi_notes);
            list.add_midi_events(&packets);
            assert!(
                list.len() >= 10,
                "{} events from {} packets",
                list.len(),
                packets.len()
            );
            // The per-note state fills as notes start and settles at its bound
            // (a key keeps `LIVE_NOTES` of them): the passes after that all
            // make the same events.
            for _ in 0..tutti_voicing::LIVE_NOTES + 1 {
                list.clear();
                list.add_midi_events(&packets);
            }
            let converted = list.len();
            assert_no_alloc::assert_no_alloc(|| {
                for _ in 0..4 {
                    list.clear();
                    list.add_midi_events(&packets);
                }
            });
            assert_eq!(list.len(), converted);
            assert_eq!(list.midi_dropped(), 0);
        }
    }

    /// **The plugin-output path converts without allocating**: every MIDI 1.0
    /// channel voice and System status a plugin can emit as `clap_event_midi`,
    /// plus typed notes and expressions, drained into the RT pool.
    #[test]
    fn the_plugin_output_path_converts_without_allocating() {
        let mut output = OutputEventList::new();
        for status in (0x80..=0xE0u8).step_by(0x10).chain(0xF0..=0xFFu8) {
            output.events.push(ClapEvent::midi(1, 0, [status, 60, 90]));
        }
        output.events.push(ClapEvent::note_on(2, 1, 60, 0.75));
        output.events.push(ClapEvent::note_off(3, 1, 60, 0.5));
        let mut pool = RtMidiEvents::new();
        output.fill_midi_events(&mut pool);
        let filled = pool.len();
        assert!(filled > 15, "{filled} events");
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..4 {
                output.fill_midi_events(&mut pool);
            }
        });
        assert_eq!(pool.len(), filled);
    }

    /// The `clap_event_midi` bytes `add_midi` stages for `words`, as
    /// `(time, bytes)`.
    fn staged_midi(words: &[u32], midi_notes: bool) -> Vec<(u32, [u8; 3])> {
        let mut list = InputEventList::new();
        list.reserve(8);
        list.set_midi_notes(midi_notes);
        list.add_midi(&packet(11, words));
        list.events()
            .iter()
            .map(|e| match e {
                ClapEvent::Midi(m) => (m.header.time, m.data),
                other => panic!("not a MIDI event: {other:?}"),
            })
            .collect()
    }

    /// **The bytes a plugin is sent, from spec-literal packets** (M2-104
    /// Figures 8 to 10 for System and MIDI 1.0 channel voice, and Figures 54 to
    /// 66 for MIDI 2.0), with unequal LSB and MSB.
    #[test]
    fn spec_packets_stage_exact_midi_bytes() {
        let at = |bytes| vec![(11, bytes)];
        // System: time code, song position (LSB 0x2A, MSB 0x05), song select.
        assert_eq!(staged_midi(&[0x10F1_3500], false), at([0xF1, 0x35, 0]));
        assert_eq!(staged_midi(&[0x10F2_2A05], false), at([0xF2, 0x2A, 0x05]));
        assert_eq!(staged_midi(&[0x10F3_2A00], false), at([0xF3, 0x2A, 0]));
        assert_eq!(staged_midi(&[0x10F8_0000], false), at([0xF8, 0, 0]));
        // MIDI 1.0 on channel 3: pitch bend LSB 0x2A MSB 0x05, program,
        // channel pressure, control change 7 = 0x2A.
        assert_eq!(staged_midi(&[0x20E3_2A05], false), at([0xE3, 0x2A, 0x05]));
        assert_eq!(staged_midi(&[0x20C3_2A00], false), at([0xC3, 0x2A, 0]));
        assert_eq!(staged_midi(&[0x20D3_2A00], false), at([0xD3, 0x2A, 0]));
        assert_eq!(staged_midi(&[0x20B3_072A], false), at([0xB3, 7, 0x2A]));
        // MIDI 2.0 on channel 3, narrowed to 7 and 14 bits: bend 14-bit
        // (0x05 << 7 | 0x2A) in the top bits, the rest low-bit noise.
        assert_eq!(
            staged_midi(&[0x40E3_0000, 0x0AAB_FFFF], false),
            at([0xE3, 0x2A, 0x05])
        );
        assert_eq!(
            staged_midi(&[0x40D3_0000, 0x5400_0000], false),
            at([0xD3, 0x2A, 0])
        );
        assert_eq!(
            staged_midi(&[0x40B3_0700, 0x5400_0000], false),
            at([0xB3, 7, 0x2A])
        );
        assert_eq!(
            staged_midi(&[0x40C3_0000, 9 << 24], false),
            at([0xC3, 9, 0])
        );
    }

    /// **Poly pressure on a MIDI-dialect note port** reaches the plugin as
    /// `0xAn key pressure` at the packet's time, narrowed to 7 bits for MIDI 2.0.
    #[test]
    fn poly_pressure_on_a_midi_dialect_port_stages_exact_bytes() {
        assert_eq!(
            staged_midi(&[0x20A3_3C2A], true),
            vec![(11, [0xA3, 0x3C, 0x2A])]
        );
        assert_eq!(
            staged_midi(&[0x40A3_3C00, 0x5400_0000], true),
            vec![(11, [0xA3, 0x3C, 0x2A])]
        );
    }

    /// **A note on a MIDI-dialect note port is `clap_event_midi` bytes built
    /// from its typed message**: the status of its kind and channel, the key,
    /// and the velocity at 7 bits by rounding (not the truncation of
    /// `narrow`), a Note On never below 1 (0 is a Note Off in MIDI 1.0), a
    /// Note Off's own velocity kept.
    #[test]
    fn notes_on_a_midi_dialect_port_stage_exact_bytes() {
        let staged = |words: &[u32]| staged_midi(words, true);
        // Note On, channel 3, key 60, velocity 0x8000: 63.5 rounds to 64.
        assert_eq!(
            staged(&[0x4093_3C00, 0x8000_0000]),
            vec![(11, [0x93, 60, 64])]
        );
        // A velocity that rounds to 0 is raised to 1.
        assert_eq!(
            staged(&[0x4093_3C00, 0x0001_0000]),
            vec![(11, [0x93, 60, 1])]
        );
        // The top of both ranges.
        assert_eq!(
            staged(&[0x409F_7F00, 0xFFFF_0000]),
            vec![(11, [0x9F, 127, 127])]
        );
        // Note Off keeps its release velocity, zero included.
        assert_eq!(
            staged(&[0x4083_3C00, 0xFFFF_0000]),
            vec![(11, [0x83, 60, 127])]
        );
        assert_eq!(
            staged(&[0x4083_3C00, 0x0000_0000]),
            vec![(11, [0x83, 60, 0])]
        );
    }

    /// **A CLAP note event's `channel` and `key` are masked to 4 and 7 bits**
    /// on the way to a packet, and a negative wildcard is not masked but
    /// resolved through the `note_id`.
    #[test]
    fn a_note_events_channel_and_key_are_masked_to_the_wire_widths() {
        // Channel 19 is 3 in 4 bits, key 130 is 2 in 7 bits.
        let wire = ClapEvent::note_on(5, 19, 130, 0.5).to_midi().unwrap();
        assert_eq!(
            fx::key_of(&wire.ump),
            Some((MidiChannel::of(3), NoteNumber::of(2), true))
        );
        let wire = ClapEvent::note_off(5, 31, 255, 0.5).to_midi().unwrap();
        assert_eq!(
            fx::key_of(&wire.ump),
            Some((MidiChannel::of(15), NoteNumber::of(127), false))
        );
    }

    /// **A registered per-note controller maps to its own CLAP expression**:
    /// Volume (7) to volume at its gain, Pan (10) to pan, Brightness (74) to
    /// brightness, each at its own value.
    #[test]
    fn registered_per_note_controllers_map_to_their_own_expressions() {
        let expression = |index: u32, value: u32| {
            let mut list = InputEventList::new();
            list.reserve(4);
            // The note sounds first: a controller with no note is held.
            list.add_midi(&packet(4, &[0x4093_3C00, 0x8000_0000]));
            list.clear();
            list.add_midi(&packet(4, &[0x4003_3C00 | index, value]));
            match list.events() {
                [ClapEvent::NoteExpression(e)] => (e.expression_id, e.value),
                other => panic!("index {index}: {other:?}"),
            }
        };
        let (id, v) = expression(7, 0x4000_0000);
        assert_eq!(id, CLAP_NOTE_EXPRESSION_VOLUME);
        assert!((v - 0.25).abs() < 1e-3, "{v}");
        let (id, v) = expression(10, 0x8000_0000);
        assert_eq!(id, CLAP_NOTE_EXPRESSION_PAN);
        assert!((v - 0.5).abs() < 1e-3, "{v}");
        let (id, v) = expression(74, 0xC000_0000);
        assert_eq!(id, CLAP_NOTE_EXPRESSION_BRIGHTNESS);
        assert!((v - 0.75).abs() < 1e-3, "{v}");
    }

    /// **A dense block sorts without allocating, and ties keep their
    /// insertion order.** 200 parameters each carrying a chunk's room of
    /// points (`ParameterQueue::chunk_room(512)`, 273), plus a note-off and a
    /// note-on for one key on frames the points also land on: sorted inside
    /// the no-alloc gate, the result is exactly std's stable sort of the
    /// same list (computed outside the gate), so a note-off stays before the
    /// note-on that reuses its key and a parameter's points keep their order.
    #[test]
    fn a_dense_block_sorts_stably_without_allocating() {
        use tutti_plugin_types::ParameterQueue;
        let points = ParameterQueue::chunk_room(512);
        let mut changes = ParameterChanges::reserved(200, points);
        for id in 0..200u32 {
            for k in 0..points {
                // Every parameter on the same frames: many ties.
                changes.add_change(
                    id.into(),
                    (k * 25) as i32,
                    (id as f64 + k as f64) / 1000.0 % 1.0,
                );
            }
        }
        let build = || {
            let mut list = InputEventList::new();
            list.reserve(200 * points + 16);
            list.add_param_changes(&changes, &[], false);
            for frame in [0u32, 100, 475] {
                list.add_midi(&(
                    at(frame),
                    fx::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
                ));
                list.add_midi(&(
                    at(frame),
                    fx::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, fx::MID_VELOCITY),
                ));
            }
            list
        };
        let print = |list: &InputEventList| -> Vec<(u32, u16, u32, u64)> {
            list.events()
                .iter()
                .map(|e| {
                    let h = e.header();
                    let (id, v) = match e {
                        ClapEvent::ParamValue(p) => (p.param_id, p.value.to_bits()),
                        ClapEvent::NoteOn(n) | ClapEvent::NoteOff(n) => (n.key as u32, 0),
                        _ => (u32::MAX, 0),
                    };
                    (h.time, h.type_, id, v)
                })
                .collect()
        };
        let mut reference = build();
        reference.events.sort_by_key(|e| e.header().time);
        let mut list = build();
        assert_eq!(list.len(), 200 * points + 6);
        assert_no_alloc::assert_no_alloc(|| {
            list.sort_by_time();
        });
        assert_eq!(print(&list), print(&reference));
        // The note-off still precedes the note-on on each frame.
        let notes: Vec<u16> = list
            .events()
            .iter()
            .filter(|e| !matches!(e, ClapEvent::ParamValue(_)))
            .map(|e| e.header().type_)
            .collect();
        let pair = [CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON];
        assert_eq!(notes, [pair, pair, pair].concat());
    }

    /// **Past the reserved room a parameter's points give way to its latest,
    /// counted, and the list does not grow.** Room for 5 events: the first
    /// parameter's 4 points and the second's first fit; the second's next 2
    /// each replace its previous point (it still ends on 0.3); the third
    /// parameter has no room at all. Three points are counted.
    #[test]
    fn add_param_changes_stays_within_its_room() {
        let mut changes = ParameterChanges::reserved(3, 4);
        for k in 0..4 {
            changes.add_change(1u32.into(), k, 0.5);
        }
        for (k, v) in [0.1, 0.2, 0.3].into_iter().enumerate() {
            changes.add_change(2u32.into(), k as i32, v);
        }
        changes.add_change(3u32.into(), 0, 0.9);
        let mut list = InputEventList::new();
        list.reserve(5);
        list.set_param_point_room(5);
        list.add_param_changes(&changes, &[], false);
        let got: Vec<(u32, f64)> = list
            .events()
            .iter()
            .filter_map(|e| match e {
                ClapEvent::ParamValue(v) => Some((v.param_id, v.value)),
                _ => None,
            })
            .collect();
        assert_eq!(got.len(), 5);
        assert_eq!(got[4].0, 2);
        assert!(
            (got[4].1 - 0.3).abs() < 1e-12,
            "ends on the latest: {got:?}"
        );
        assert!(got.iter().all(|&(id, _)| id != 3));
        assert_eq!(list.param_points_dropped(), 3);
    }

    #[test]
    fn add_param_changes_denormalizes_against_range() {
        // Param 9 has plain range [100, 1100]; a normalized 0.25 → 350.
        let mut changes = ParameterChanges::reserved(1, 1);
        changes.add_change(9u32.into(), 0, 0.25);
        let mut list = InputEventList::new();
        list.add_param_changes(&changes, &[(9, 100.0, 1100.0)], true);
        assert!((first_param_value(&list) - 350.0).abs() < 1e-6);
    }

    #[test]
    fn add_param_changes_clamps_denormalized_value_into_range() {
        // A normalized 1.5 (over-range) must clamp to the plain max, not overshoot.
        let mut changes = ParameterChanges::reserved(1, 1);
        changes.add_change(9u32.into(), 0, 1.5);
        let mut list = InputEventList::new();
        list.add_param_changes(&changes, &[(9, 0.0, 10.0)], true);
        assert!((first_param_value(&list) - 10.0).abs() < 1e-6);
    }

    #[test]
    fn add_param_changes_survives_a_plugin_reporting_inverted_bounds() {
        // `f32::clamp` panics when `lo > hi`, and these bounds are whatever the
        // plugin reported. A malformed plugin must not take down the audio
        // thread — the value lands inside the range either way round.
        let mut changes = ParameterChanges::reserved(1, 1);
        changes.add_change(9u32.into(), 0, 0.5);
        let mut list = InputEventList::new();
        list.add_param_changes(&changes, &[(9, 10.0, 0.0)], true);
        let v = first_param_value(&list);
        assert!((0.0..=10.0).contains(&v), "value {v} escaped the range");
    }

    #[test]
    fn add_param_changes_passes_through_when_range_unknown() {
        // No range for this param id → value forwarded unchanged (safe fallback
        // for the common normalized-0..1 param).
        let mut changes = ParameterChanges::reserved(1, 1);
        changes.add_change(9u32.into(), 0, 0.42);
        let mut list = InputEventList::new();
        list.add_param_changes(&changes, &[], false);
        assert!((first_param_value(&list) - 0.42).abs() < 1e-6);
    }

    // --- event time is bounded to the block ---

    fn first_time(list: &InputEventList) -> u32 {
        list.events().first().expect("an event").header().time
    }

    /// A NEGATIVE `sample_offset` must not wrap.
    ///
    /// `sample_offset` is `i32` and `header.time` is `u32`; a bare
    /// `point.sample_offset as u32` turns -1 into 4_294_967_295, which sorts
    /// last and is handed to the plugin as a sample index into the block.
    /// Plugins split their buffer on `time`, so that is an out-of-bounds access
    /// inside the plugin.
    #[test]
    fn negative_sample_offset_does_not_wrap_to_four_billion_h3() {
        let mut changes = ParameterChanges::reserved(1, 1);
        changes.add_change(9u32.into(), -1, 0.5);
        let mut list = InputEventList::new();
        list.add_param_changes(&changes, &[], false);
        assert_eq!(
            first_time(&list),
            0,
            "a negative offset means 'already due', so it saturates at 0"
        );
    }

    /// The same trap on the note-expression path.
    #[test]
    fn negative_note_expression_offset_does_not_wrap_h3() {
        let mut list = InputEventList::new();
        let mut expr = ClapNoteExpression::new(NoteExpressionType::Pressure, 0, 0.5);
        expr.sample_offset = -32;
        list.add_note_expressions(&[expr]);
        assert_eq!(first_time(&list), 0);
    }

    /// An event past the end of the block is clamped to the last valid
    /// sample index, never handed through as-is.
    ///
    /// Clamping rather than dropping is deliberate: a NOTE_OFF or PARAM_VALUE
    /// carries *state*, so dropping one leaves a stuck note or a stale
    /// parameter forever, while clamping mistimes it by at most one block.
    #[test]
    fn event_time_past_the_block_is_clamped_to_the_last_sample_h3() {
        let mut list = InputEventList::new();
        list.add_midi(&(
            at(9_999),
            fx::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x4000),
        ));
        assert_eq!(list.len(), 1);

        list.clamp_times(64);
        assert_eq!(
            first_time(&list),
            63,
            "must land on the last valid index of a 64-frame block"
        );
        assert_eq!(
            list.len(),
            1,
            "the note-off must survive — dropping it would \
                                   leave a stuck note"
        );
    }

    /// An in-range time is untouched, and a zero-length block folds
    /// everything to 0 (there is no valid index at all).
    #[test]
    fn clamp_times_leaves_in_range_events_alone_h3() {
        let mut list = InputEventList::new();
        list.add_midi(&(
            at(17),
            fx::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, fx::MID_VELOCITY),
        ));
        list.clamp_times(64);
        assert_eq!(first_time(&list), 17);

        list.clamp_times(0);
        assert_eq!(first_time(&list), 0);
    }

    /// **A transport change is a `CLAP_EVENT_TRANSPORT` at its frame, in time
    /// order with the block's other input events**: two changes with a MIDI
    /// note between them, added the way `process_impl` adds them (MIDI first,
    /// then the changes), come out transport at 5, note at 10, transport at 20,
    /// each carrying its own tempo and slope, and the plugin is lent each as a
    /// transport event.
    #[test]
    fn transport_changes_are_events_at_their_frames_in_order_with_midi() {
        use tutti_clap_ffi::EventSource;
        let transport = |tempo: f64, inc: f64| clap_event_transport {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_transport>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: clap_sys::events::CLAP_EVENT_TRANSPORT,
                flags: 0,
            },
            flags: clap_sys::events::CLAP_TRANSPORT_HAS_TEMPO,
            song_pos_beats: 0,
            song_pos_seconds: 0,
            tempo,
            tempo_inc: inc,
            loop_start_beats: 0,
            loop_end_beats: 0,
            loop_start_seconds: 0,
            loop_end_seconds: 0,
            bar_start: 0,
            bar_number: 0,
            tsig_num: 4,
            tsig_denom: 4,
        };
        let mut list = InputEventList::new();
        list.add_midi(&(
            at(10),
            fx::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, fx::MID_VELOCITY),
        ));
        list.add_transport(5, transport(100.0, 0.5));
        list.add_transport(20, transport(90.0, -0.25));
        list.clamp_times(64);
        list.sort_by_time();
        let shape: Vec<_> = list
            .events()
            .iter()
            .map(|e| match e {
                ClapEvent::Transport(t) => (t.header.time, Some((t.tempo, t.tempo_inc))),
                other => (other.header().time, None),
            })
            .collect();
        assert_eq!(
            shape,
            [
                (5, Some((100.0, 0.5))),
                (10, None),
                (20, Some((90.0, -0.25)))
            ]
        );
        // The plugin is lent a transport event for each.
        assert!(matches!(
            EventSource::get(&list, 0),
            Some(tutti_clap_ffi::EventRef::Transport(_))
        ));
        assert!(matches!(
            EventSource::get(&list, 1),
            Some(tutti_clap_ffi::EventRef::Midi(_) | tutti_clap_ffi::EventRef::Note(_))
        ));
    }

    // --- CLAP VOLUME is a gain in `0 < x <= 4`, not a unit fraction ---

    /// `clap/events.h` defines
    /// `CLAP_NOTE_EXPRESSION_VOLUME` as "with 0 < x <= 4, plain = 20 * log(x)"
    /// — a gain where 1.0 is unity and 0 is *excluded*. A bare `0..1` unit
    /// value would turn a MIDI volume of 0 into an out-of-range 0.0.
    #[test]
    fn per_note_volume_maps_into_claps_gain_range_l6() {
        // Full-scale MIDI volume is unity gain, not "the top of 0..1".
        let full = (
            at(0),
            fx::per_note_controller(MidiGroup::FIRST, MidiChannel::of(2), 64, 7, u32::MAX, false),
        );
        let ClapEvent::NoteExpression(e) = from_midi_live(&full).expect("converts") else {
            panic!("expected NoteExpression");
        };
        assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_VOLUME);
        assert!(
            (e.value - 1.0).abs() < 1e-3,
            "full MIDI volume must be CLAP unity (1.0), got {}",
            e.value
        );

        // Zero MIDI volume must stay inside the OPEN interval: `0 < x`.
        let zero = (
            at(0),
            fx::per_note_controller(MidiGroup::FIRST, MidiChannel::of(2), 64, 7, 0, false),
        );
        let ClapEvent::NoteExpression(e) = from_midi_live(&zero).expect("converts") else {
            panic!("expected NoteExpression");
        };
        assert!(
            e.value > 0.0,
            "CLAP VOLUME excludes 0 (20*log(0) is -inf); got {}",
            e.value
        );
        assert!(e.value < 1e-3, "silence must still be inaudible");
    }

    /// The attenuating half round-trips exactly, and a plugin-emitted
    /// boost (`1 < x <= 4`, legal in CLAP) saturates at MIDI full scale instead
    /// of being reported as some arbitrary rescaled value.
    /// CLAP declines a dimension it has no id for, rather than substituting.
    ///
    /// `clap_note_expression` is a closed set of seven ids with no vendor
    /// range, so a VST3 plugin's custom dimension has nothing to map onto.
    /// Encoding it as the nearest CLAP id would deliver a plugin-specific value
    /// as brightness or tuning — audible, and wrong.
    #[test]
    fn a_custom_dimension_is_declined_not_substituted() {
        assert!(
            ClapEvent::note_expression(0, NoteExpressionType::Custom(100_000), 1, 0.5).is_none()
        );
        // The named dimensions still encode, so the guard did not cost them.
        for ty in [
            NoteExpressionType::Volume,
            NoteExpressionType::Pan,
            NoteExpressionType::Tuning,
            NoteExpressionType::Vibrato,
            NoteExpressionType::Brightness,
            NoteExpressionType::Pressure,
            NoteExpressionType::Expression,
        ] {
            assert!(
                ClapEvent::note_expression(0, ty, 1, 0.5).is_some(),
                "{ty:?} is a named CLAP dimension"
            );
        }
    }

    /// A batch containing a custom dimension drops only that entry.
    #[test]
    fn add_note_expressions_skips_only_the_custom_entry() {
        let exprs = [
            ClapNoteExpression::new(NoteExpressionType::Volume, 1, 0.5),
            ClapNoteExpression::new(NoteExpressionType::Custom(100_000), 1, 0.5),
        ];
        let mut list = InputEventList::new();
        list.add_note_expressions(&exprs);
        assert_eq!(
            list.events.len(),
            1,
            "the named dimension must survive alongside the dropped custom one"
        );
    }

    /// **A plugin's Volume, Pan and Brightness expressions go back as the
    /// assignable per-note controllers 7, 10 and 74** (the spec's numbers, not
    /// read from the host's table), and the dimensions with none are dropped.
    #[test]
    fn expressions_return_as_their_per_note_controller_numbers() {
        let controller = |ty| {
            let clap = per_note_expression(0, ty, MidiChannel::of(1), NoteNumber::of(64), 0.5)?;
            match message(&clap.to_midi()?) {
                Message::Midi2(_, Voice2::AssignablePerNote { controller, .. }) => Some(controller),
                other => panic!("expected a per-note controller, got {other:?}"),
            }
        };
        assert_eq!(controller(NoteExpressionType::Volume), Some(7));
        assert_eq!(controller(NoteExpressionType::Pan), Some(10));
        assert_eq!(controller(NoteExpressionType::Brightness), Some(74));
        assert_eq!(controller(NoteExpressionType::Expression), None);
    }

    #[test]
    fn clap_volume_round_trips_and_saturates_boost_l6() {
        use tutti_midi_protocol::u32_to_unit_f32;

        let unit_of = |gain: f64| -> f32 {
            let clap = per_note_expression(
                0,
                NoteExpressionType::Volume,
                MidiChannel::of(1),
                NoteNumber::of(64),
                gain,
            )
            .expect("Volume is a named CLAP dimension");
            let midi = clap.to_midi().expect("volume -> midi");
            match message(&midi) {
                Message::Midi2(_, Voice2::AssignablePerNote { value, .. }) => {
                    u32_to_unit_f32(value)
                }
                other => panic!("expected a per-note controller, got {other:?}"),
            }
        };

        // Unity gain is MIDI full scale — the anchor the whole mapping hangs on.
        assert!((unit_of(1.0) - 1.0).abs() < 1e-3, "{}", unit_of(1.0));
        // Half gain (-6 dB) round-trips to half scale.
        assert!((unit_of(0.5) - 0.5).abs() < 1e-3, "{}", unit_of(0.5));
        // Boost is legal in CLAP but unrepresentable in MIDI: saturate at full
        // scale rather than wrapping or rescaling the whole range.
        assert!((unit_of(4.0) - 1.0).abs() < 1e-3, "{}", unit_of(4.0));
    }

    // --- MIDI 2.0 per-note ↔ CLAP note-expression ---

    #[test]
    fn per_note_pitch_bend_becomes_tuning_note_expression() {
        let bend = signed_f32_to_bend_u32(0.5);
        let midi = (
            at(11),
            fx::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::of(3), 60, bend),
        );
        match from_midi_live(&midi).expect("per-note bend converts") {
            ClapEvent::NoteExpression(e) => {
                assert_eq!(e.header.time, 11);
                assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_TUNING);
                assert_minted(e.note_id);
                assert_eq!(e.channel, 3);
                assert_eq!(e.key, 60);
                let expected = 0.5 * emitted_bend_range_semitones();
                assert!(
                    (e.value - expected).abs() < 0.1,
                    "tuning {} vs {expected}",
                    e.value
                );
            }
            _ => panic!("expected NoteExpression(Tuning)"),
        }
    }

    /// **The input list follows the stream's per-note bend range** (RPN
    /// #00/07) per channel, across blocks, and consumes it (CLAP has no event
    /// for it): ±12 on channel 3 makes a half-scale bend there 6 semitones,
    /// in this block and after a `clear`, while channel 4 stays at ±48 (24).
    #[test]
    fn the_input_list_follows_the_per_note_bend_range() {
        let half = signed_f32_to_bend_u32(0.5);
        let bend = |channel: u8| {
            (
                at(0),
                fx::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::of(channel), 60, half),
            )
        };
        let tunings = |list: &InputEventList| -> Vec<(i16, f64)> {
            list.events
                .iter()
                .map(|e| match e {
                    ClapEvent::NoteExpression(e) => {
                        assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_TUNING);
                        (e.channel, e.value)
                    }
                    _ => panic!("expected a tuning note expression"),
                })
                .collect()
        };
        let near = |got: &[(i16, f64)], want: &[(i16, f64)]| {
            assert_eq!(got.len(), want.len(), "{got:?}");
            for ((gc, gv), (wc, wv)) in got.iter().zip(want) {
                assert_eq!(gc, wc);
                assert!((gv - wv).abs() < 0.01, "{got:?} vs {want:?}");
            }
        };

        let mut list = InputEventList::new();
        // A note sounds on each channel: a bend with no note is held.
        for channel in [3, 4] {
            list.add_midi(&(
                at(0),
                fx::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::of(channel),
                    60,
                    fx::MID_VELOCITY,
                ),
            ));
        }
        list.take_events();
        list.add_midi(&(
            at(0),
            fx::registered_rpn(
                MidiGroup::FIRST,
                MidiChannel::of(3),
                tutti_ump::Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
                PitchBendSensitivity::of(12).to_rpn_bits(),
            ),
        ));
        list.add_midi(&bend(3)).add_midi(&bend(4));
        near(&tunings(&list), &[(3, 6.0), (4, 24.0)]);

        list.clear();
        list.add_midi(&bend(3));
        near(&tunings(&list), &[(3, 6.0)]);
    }

    #[test]
    fn tuning_note_expression_round_trips_to_per_note_pitch_bend() {
        use tutti_midi_protocol::bend_u32_to_signed_f32;

        let value_semitones = 0.5 * emitted_bend_range_semitones();
        let clap = per_note_expression(
            7,
            NoteExpressionType::Tuning,
            MidiChannel::of(5),
            NoteNumber::of(67),
            value_semitones,
        )
        .expect("Tuning is a named CLAP dimension");
        let midi = clap.to_midi().expect("tuning -> midi");
        assert_eq!(midi.offset, 7);
        match message(&midi) {
            Message::Midi2(address, Voice2::PerNotePitchBend { note, value }) => {
                assert_eq!(address.channel.get(), 5);
                assert_eq!(note.get(), 67);
                let value = bend_u32_to_signed_f32(value);
                assert!((value - 0.5).abs() < 0.01, "bend {value}");
            }
            other => panic!("expected PerNotePitchBend, got {other:?}"),
        }
    }

    #[test]
    fn key_pressure_becomes_pressure_note_expression() {
        let midi = (
            at(0),
            fx::poly_pressure(
                MidiGroup::FIRST,
                MidiChannel::of(2),
                48,
                unit_f32_to_u32(0.75),
            ),
        );
        match from_midi_live(&midi).expect("poly pressure converts") {
            ClapEvent::NoteExpression(e) => {
                assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_PRESSURE);
                assert_minted(e.note_id);
                assert!((e.value - 0.75).abs() < 0.01, "pressure {}", e.value);
            }
            _ => panic!("expected NoteExpression(Pressure)"),
        }
    }

    #[test]
    fn assignable_per_note_controller_maps_to_brightness() {
        // Assignable per-note controller index 74 (CC74) → CLAP _BRIGHTNESS.
        let midi = (
            at(0),
            fx::per_note_controller(
                MidiGroup::FIRST,
                MidiChannel::of(1),
                64,
                74,
                unit_f32_to_u32(0.6),
                false,
            ),
        );
        match from_midi_live(&midi).expect("per-note cc converts") {
            ClapEvent::NoteExpression(e) => {
                assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_BRIGHTNESS);
                assert_minted(e.note_id);
                assert!((e.value - 0.6).abs() < 0.01, "brightness {}", e.value);
            }
            _ => panic!("expected NoteExpression(Brightness)"),
        }
    }

    #[test]
    fn per_note_controller_without_counterpart_yields_no_event() {
        // Per-note CC index 20 has no CLAP expression dimension and no MIDI 1.0
        // form, so nothing is sent.
        let midi = (
            at(0),
            fx::per_note_controller(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                20,
                unit_f32_to_u32(0.5),
                false,
            ),
        );
        assert!(from_midi(&midi).is_none());
    }

    #[test]
    fn from_midi_event_note_on_decodes_to_typed_note_on() {
        let midi = (
            at(7),
            fx::note_on(MidiGroup::FIRST, MidiChannel::of(1), 60, fx::MID_VELOCITY),
        );
        match from_midi(&midi).expect("note on converts") {
            ClapEvent::NoteOn(e) => {
                assert_eq!(e.header.time, 7);
                assert_eq!(e.channel, 1);
                assert_eq!(e.key, 60);
                assert!((e.velocity - 0.5).abs() < 0.01, "velocity {}", e.velocity);
                // The note_id must match what the expression path mints.
                assert_minted(e.note_id);
            }
            _ => panic!("expected NoteOn"),
        }
    }

    /// The `note_id` a NOTE_ON carries must equal the one a later
    /// NOTE_EXPRESSION for the same note carries, and the NOTE_OFF names no id
    /// (CLAP's `-1`: the plugin matches it by port, channel and key).
    ///
    /// If `note_on` used `note_id: -1` while the expression carried a real id,
    /// a plugin that keys its voice map on `note_id` — the normal MPE-capable
    /// design — would find no voice and **silently drop every per-note
    /// expression**. This asserts the *pairing*, which a per-event round-trip
    /// test cannot. The id is the note's own, minted above the ids of
    /// (channel, key) pairs (M2-104 section 7.4.5: a Detach ends one note on a
    /// key, the next has another).
    #[test]
    fn note_on_and_expression_agree_on_note_id_h2() {
        const CH: u8 = 3;
        const KEY: u8 = 60;

        let mut list = InputEventList::new();
        list.add_midi(&(
            at(0),
            fx::note_on(MidiGroup::FIRST, MidiChannel::of(CH), KEY, fx::MID_VELOCITY),
        ));
        list.add_midi(&(
            at(1),
            fx::poly_pressure(
                MidiGroup::FIRST,
                MidiChannel::of(CH),
                KEY,
                unit_f32_to_u32(0.5),
            ),
        ));
        list.add_midi(&(
            at(2),
            fx::note_off(MidiGroup::FIRST, MidiChannel::of(CH), KEY, 0x4000),
        ));

        let [
            ClapEvent::NoteOn(on),
            ClapEvent::NoteExpression(expr),
            ClapEvent::NoteOff(off),
        ] = list.events()
        else {
            panic!(
                "expected NoteOn / NoteExpression / NoteOff, got {:?}",
                list.events()
            );
        };

        assert!(
            on.note_id > tutti_ump::NoteId::MAX_CHANNEL_NOTE as i32,
            "NOTE_ON must carry a minted id, not a (channel, key) pair's or the -1 wildcard"
        );
        assert_eq!(
            on.note_id, expr.note_id,
            "the expression targets a voice the note-on never opened — a plugin \
             keying on note_id drops it"
        );
        assert_eq!(
            off.note_id, -1,
            "a Note Off is matched by port, channel and key"
        );
        assert_eq!((off.channel, off.key), (3, 60));
    }

    /// `note_id_for` only covers channels 0..16 and keys 0..128. A
    /// CLAP wildcard (`-1`) or out-of-range field has no voice to name, so it
    /// must fall back to CLAP's `-1` "unspecified" rather than minting a
    /// nonsense id from a negative number.
    #[test]
    fn note_on_wildcard_fields_yield_unspecified_note_id_h2() {
        let ClapEvent::NoteOn(e) = ClapEvent::note_on(0, -1, 60, 1.0) else {
            panic!("expected NoteOn");
        };
        assert_eq!(e.note_id, -1);

        let ClapEvent::NoteOn(e) = ClapEvent::note_on(0, 0, -1, 1.0) else {
            panic!("expected NoteOn");
        };
        assert_eq!(e.note_id, -1);
    }

    #[test]
    fn note_off_release_velocity_is_threaded_through() {
        // A MIDI-2 NoteOff carries a release velocity; it must reach the
        // CLAP NoteOff's `velocity`, and round-trip back.
        use tutti_midi_protocol::u16_to_unit_f32;
        let rel = 0x6000u16; // ~0.375 of full scale
        let midi = (
            at(9),
            fx::note_off(MidiGroup::FIRST, MidiChannel::of(4), 55, rel),
        );
        match from_midi(&midi).expect("note off converts") {
            ClapEvent::NoteOff(e) => {
                assert_eq!(e.key, 55);
                let expected = f64::from(u16_to_unit_f32(rel));
                assert!(
                    (e.velocity - expected).abs() < 0.01,
                    "release velocity {} vs {expected}",
                    e.velocity
                );
                assert!(e.velocity > 0.0, "release velocity must not collapse to 0");
            }
            _ => panic!("expected NoteOff"),
        }
    }

    #[test]
    fn note_off_release_velocity_round_trips() {
        // The CLAP → MIDI-2 direction must preserve release velocity too.
        use tutti_midi_protocol::u16_to_unit_f32;

        let clap = ClapEvent::note_off(2, 1, 48, 0.5);
        let midi = clap.to_midi().expect("note off -> midi");
        match message(&midi) {
            Message::Midi2(_, Voice2::NoteOff { velocity, .. }) => {
                let vel = u16_to_unit_f32(velocity);
                assert!((vel - 0.5).abs() < 0.01, "release velocity {vel}");
            }
            other => panic!("expected CV2 NoteOff, got {other:?}"),
        }
    }

    #[test]
    fn from_midi_event_velocity_zero_note_on_becomes_note_off() {
        // The MIDI velocity-0 NoteOn quirk: widening reads it as a NoteOff, so
        // a raw MIDI-1 velocity-0 NoteOn converts to a CLAP NoteOff.
        let midi = midi1_event(&[0x90, 60, 0]);
        match from_midi(&midi).expect("converts") {
            ClapEvent::NoteOff(e) => {
                assert_eq!(e.key, 60);
            }
            _ => panic!("expected NoteOff for vel-0 NoteOn"),
        }
    }

    #[test]
    fn from_midi_event_cc_forwards_as_generic_midi() {
        let midi = (
            at(0),
            fx::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::VOLUME,
                0x8000_0000,
            ),
        ); // volume, ~half
        match from_midi(&midi).expect("cc converts") {
            ClapEvent::Midi(e) => {
                assert_eq!(e.data[0] & 0xF0, 0xB0, "status should be CC");
                assert_eq!(e.data[1], 7, "controller number");
            }
            _ => panic!("expected generic Midi for CC"),
        }
    }

    #[test]
    fn inbound_generic_midi_cc_promotes_to_cv2() {
        // A plugin-emitted generic MIDI-1 CC must decode to MIDI-2 Channel Voice
        // 2, not CV1 — the engine sees one vocabulary regardless of source
        // (mirrors the hardware input path's promotion).

        // A generic CLAP Midi event carrying raw MIDI-1 CC bytes (0xB1, 74, 100).
        let clap = ClapEvent::midi(0, 0, [0xB1, 74, 100]);
        let midi = clap.to_midi().expect("cc decodes");
        match message(&midi) {
            Message::Midi2(address, Voice2::ControlChange { controller, value }) => {
                assert_eq!(address.channel.get(), 1);
                assert_eq!(controller.get(), 74);
                assert_eq!(value, tutti_fixtures::midi::wide_value(100));
            }
            other => panic!("expected CV2 ControlChange, got {other:?}"),
        }
    }

    #[test]
    fn inbound_system_message_passes_through() {
        // A System real-time message (Timing Clock 0xF8) has no CV form and must
        // pass through `widened` unchanged, not be dropped or promoted.
        let clap = ClapEvent::midi(0, 0, [0xF8, 0, 0]);
        let midi = clap.to_midi().expect("clock decodes");
        assert!(
            matches!(message(&midi), Message::System(_, System::TimingClock)),
            "timing clock should stay a System Real-Time message"
        );
    }

    #[test]
    fn note_event_round_trips_to_full_width_cv2() {
        // ClapEvent::NoteOn -> WireMidi builds a native MIDI-2 NoteOn preserving
        // the note and full-width velocity (no 7-bit squash).
        use tutti_midi_protocol::u16_to_unit_f32;

        let clap = ClapEvent::note_on(3, 2, 64, 0.75);
        let midi = clap.to_midi().expect("note on -> midi");
        assert_eq!(midi.offset, 3);
        match message(&midi) {
            Message::Midi2(address, Voice2::NoteOn { note, velocity, .. }) => {
                assert_eq!(address.channel.get(), 2);
                assert_eq!(note.get(), 64);
                let velocity = u16_to_unit_f32(velocity);
                assert!((velocity - 0.75).abs() < 0.01, "velocity {velocity}");
            }
            other => panic!("expected CV2 NoteOn, got {other:?}"),
        }
    }

    /// A plugin emitting more events than the return pool holds must not grow
    /// it — `try_push` is the *plugin's* callback, so an unbounded pool would
    /// let a plugin provoke a `malloc` inside the audio callback. Events past
    /// the cap are dropped and reported through `is_overflowed()`.
    ///
    /// Before the pool was an `RtVec`, the bound existed only as a `reserve`
    /// performed off-RT in `activate()` and then trusted; nothing stopped this.
    #[test]
    fn midi_pool_caps_a_flood_of_plugin_emitted_events() {
        let mut output = OutputEventList::new();
        let over = RT_MIDI_CAPACITY * 3;
        for i in 0..over {
            output
                .events
                .push(ClapEvent::midi(i as u32, 0, [0x90, 60, 100]));
        }

        let mut pool = RtMidiEvents::new();
        output.fill_midi_events(&mut pool);

        assert_eq!(
            pool.len(),
            RT_MIDI_CAPACITY,
            "the pool fills to its cap and no further"
        );
        assert!(
            pool.is_overflowed(),
            "dropping events must be reported, not silent"
        );
        assert_eq!(
            pool.capacity(),
            RT_MIDI_CAPACITY,
            "capacity is fixed at the type level, so it cannot have grown"
        );
    }

    /// Refilling from an over-long source every block must stay at the cap —
    /// the steady-state version of the check above.
    #[test]
    fn refilling_the_midi_pool_repeatedly_never_grows_it() {
        let mut output = OutputEventList::new();
        for i in 0..(RT_MIDI_CAPACITY * 2) {
            output
                .events
                .push(ClapEvent::midi(i as u32, 0, [0x90, 60, 100]));
        }

        let mut pool = RtMidiEvents::new();
        for _ in 0..256 {
            output.fill_midi_events(&mut pool);
            assert_eq!(pool.len(), RT_MIDI_CAPACITY);
            assert_eq!(pool.remaining(), 0);
        }
    }

    /// A note goes out as `clap_event_midi` when the port takes the MIDI
    /// dialect only; per-note expression adds nothing.
    #[test]
    fn midi_dialect_sends_notes_as_midi_events() {
        let ch = MidiChannel::of(9);
        let on = (at(0), fx::note_on(MidiGroup::FIRST, ch, 60, 0xFFFF));
        // 7-bit velocity 64, widened (M2-115).
        let mid = (
            at(0),
            fx::note_on(
                MidiGroup::FIRST,
                ch,
                62,
                tutti_fixtures::midi::wide_velocity(64),
            ),
        );
        let quiet = (at(0), fx::note_on(MidiGroup::FIRST, ch, 61, 1));
        let off = (at(0), fx::note_off(MidiGroup::FIRST, ch, 60, 0));

        let mut list = InputEventList::new();
        list.set_midi_notes(true);
        list.add_midi(&on)
            .add_midi(&mid)
            .add_midi(&quiet)
            .add_midi(&off);
        let sent: Vec<(u16, [u8; 3])> = list
            .events()
            .iter()
            .map(|e| match e {
                ClapEvent::Midi(m) => (m.port_index, m.data),
                other => panic!("expected a MIDI event, got {:?}", other.header().type_),
            })
            .collect();
        assert_eq!(
            sent,
            [
                (0, [0x99, 60, 127]),
                (0, [0x99, 62, 64]),
                (0, [0x99, 61, 1]),
                (0, [0x89, 60, 0])
            ],
            "note-on never rounds to velocity 0, which would be a note-off"
        );

        let mut list = InputEventList::new();
        list.add_midi(&on);
        assert!(matches!(list.events()[0], ClapEvent::NoteOn(_)));

        let expr = [ClapNoteExpression {
            sample_offset: 0,
            note_id: 1,
            port_index: 0,
            channel: -1,
            key: -1,
            expression_type: NoteExpressionType::Volume,
            value: 0.5,
        }];
        let mut list = InputEventList::new();
        list.set_midi_notes(true);
        list.add_note_expressions(&expr);
        assert!(list.events().is_empty());
    }

    /// Poly pressure (`0xAn`) is a per-note expression to a CLAP-dialect port
    /// and raw MIDI to a MIDI-dialect one.
    #[test]
    fn midi_dialect_keeps_poly_pressure_as_midi_bytes() {
        let event = midi1_event(&[0xA3, 60, 90]);

        let mut list = InputEventList::new();
        list.set_midi_notes(true);
        list.add_midi(&event);
        assert_eq!(list.events().len(), 1);
        let ClapEvent::Midi(m) = &list.events()[0] else {
            panic!("expected a MIDI event");
        };
        assert_eq!((m.port_index, m.data), (0, [0xA3, 60, 90]));

        let mut list = InputEventList::new();
        // The note sounds first: a pressure with no note is held.
        list.add_midi(&midi1_event(&[0x93, 60, 100]));
        list.take_events();
        list.add_midi(&event);
        assert!(matches!(list.events()[0], ClapEvent::NoteExpression(_)));
    }

    /// The parameter queues a pooled chunk carries at its budget: `PARAMS`
    /// parameters with a room of `room`, the first
    /// `ParameterChanges::POOLED_QUEUES` at full room and every other one at
    /// its floor, points for different parameters arriving interleaved in
    /// time (frame by frame, every parameter), as the host's node fills it.
    /// Point `k` of parameter `id` is on frame `k` (every parameter on the
    /// same frames: many ties) with value `(id + k) % 7 / 8`.
    fn pooled_chunk(params: u32, room: usize) -> ParameterChanges {
        let mut changes = ParameterChanges::reserved(params as usize, room);
        for id in 0..params {
            assert!(changes.open_queue(id.into()));
        }
        for k in 0..room {
            for id in 0..params {
                let i = id as usize;
                if k >= ParameterChanges::FLOOR_POINTS && i >= ParameterChanges::POOLED_QUEUES {
                    continue;
                }
                assert!(changes.make_room(i), "within the budget");
                // A frame a point: a full room (273 at 512 frames) fits the
                // block. (Changed in doc 014 §12.2: every third frame, which
                // put a 273-point room's points past a 512-frame block.)
                changes.queues[i].add_point(k as i32, pooled_value(id, k));
            }
        }
        changes
    }

    fn pooled_value(id: u32, k: usize) -> f64 {
        ((id as usize + k) % 7) as f64 / 8.0
    }

    /// Each parameter's `(time, value)` events in list order.
    fn per_param(events: &[ClapEvent], params: u32) -> Vec<Vec<(u32, f64)>> {
        let mut out = vec![Vec::new(); params as usize];
        for e in events {
            if let ClapEvent::ParamValue(v) = e {
                out[v.param_id as usize].push((v.header.time, v.value));
            }
        }
        out
    }

    /// **A chunk at the pool budget reaches the plugin whole, time-sorted,
    /// without allocating; one point past it is counted.** 2 000 parameters
    /// at a 512-frame block (a room of 273): the input list is reserved at the
    /// chunk's pool budget (`ParameterChanges::point_budget`, as activation
    /// reserves it), and the host's pooled chunk, its
    /// points arriving interleaved, is flattened and sorted inside the
    /// no-alloc gate. Every event is in time order, and each parameter's
    /// points are its queue's, in order. Then a set past the budget (the last
    /// parameter carrying the list's remaining room and two more points,
    /// which the host's pooled chunk cannot): each surplus point replaces
    /// that parameter's previous one (it still ends on its latest value),
    /// counted.
    ///
    /// The reservation activation makes is pinned by
    /// `activation_reserves_the_pool_budget_not_params_times_room`.
    #[test]
    fn a_chunk_at_the_pool_budget_is_handed_on_sorted_without_allocating() {
        const PARAMS: u32 = 2_000;
        let room = ParameterQueue::chunk_room(512);
        let budget = ParameterChanges::point_budget(PARAMS as usize, room);
        let changes = pooled_chunk(PARAMS, room);
        // What the chunk carries: a moving parameter's floor sits idle while
        // it holds a block, so the budget is that much over what can arrive.
        let pooled = ParameterChanges::POOLED_QUEUES;
        let carried = (PARAMS as usize - pooled) * ParameterChanges::FLOOR_POINTS + pooled * room;
        let mut list = InputEventList::new();
        list.reserve(budget);
        list.set_param_point_room(budget);
        let fill = |list: &mut InputEventList, changes: &ParameterChanges| {
            list.clear();
            list.add_param_changes(changes, &[], false);
            list.clamp_times(512);
            list.sort_by_time();
        };
        fill(&mut list, &changes);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..3 {
                fill(&mut list, &changes);
            }
        });
        assert_eq!(list.len(), carried);
        assert_eq!(list.param_points_dropped(), 0);
        assert!(list.events().is_sorted_by_key(|e| e.header().time));
        for (id, got) in per_param(list.events(), PARAMS).into_iter().enumerate() {
            let q = &changes.queues[id];
            let want: Vec<(u32, f64)> = q
                .points
                .iter()
                .map(|p| (p.sample_offset as u32, p.value.get()))
                .collect();
            assert_eq!(got, want, "parameter {id}");
        }

        // One point past the budget (a set the pool did not bound, built by
        // hand: the last parameter given the rest of the list's room, and
        // one more). The last queue is built with room for them all
        // (`ParameterQueue::with_capacity`), as the host's pool never would.
        let mut over = ParameterChanges::new();
        for q in &changes.queues {
            over.add_queue(q.clone());
        }
        let extra = budget - carried + 1;
        let last = over.queues.last_mut().expect("a queue");
        let mut grown = ParameterQueue::with_capacity(last.param_id, last.len() + extra + 1);
        for p in &last.points {
            assert!(grown.points.try_push(*p).is_ok());
        }
        for k in 0..extra {
            assert!(grown.add_point(70 + k as i32 % 300, 0.5));
        }
        assert!(grown.add_point(400, 0.875));
        *last = grown;
        assert_no_alloc::assert_no_alloc(|| fill(&mut list, &over));
        assert_eq!(list.len(), budget);
        assert_eq!(list.param_points_dropped(), 2);
        let last = per_param(list.events(), PARAMS).pop().expect("a parameter");
        assert_eq!(last.last(), Some(&(400, 0.875)), "ends on its latest");
        assert_eq!(last.len(), ParameterChanges::FLOOR_POINTS + extra - 1);
    }

    /// Every SysEx event in `list`, as `(time, bytes)`, its bytes read
    /// through the `buffer` pointer the plugin is handed.
    fn sysex_read_by_plugin(list: &InputEventList) -> Vec<(u32, Vec<u8>)> {
        (0..list.len())
            .filter_map(|i| {
                let event = tutti_clap_ffi::EventSource::get(list, i)?;
                let bytes = event.sysex_payload()?;
                let time = list.events()[i].header().time;
                assert_eq!(list.events()[i].header().type_, CLAP_EVENT_MIDI_SYSEX);
                Some((time, bytes.to_vec()))
            })
            .collect()
    }

    /// The SysEx7 packets of `payload`, each `time` frames into the block.
    fn sysex_packets(payload: &[u8], time: u32) -> Vec<UmpAt> {
        fx::sysex7_packets(payload)
            .into_iter()
            .map(|ump| (at(time), ump))
            .collect()
    }

    fn framed(payload: &[u8]) -> Vec<u8> {
        tutti_ump::frame(payload).collect()
    }

    /// **A multi-packet SysEx reaches the plugin as one `MIDI_SYSEX` event**,
    /// `0xF0 … 0xF7`, and a SysEx spanning two blocks completes in the
    /// second; staging allocates nothing once reserved.
    #[test]
    fn a_multi_packet_sysex_reaches_the_plugin_whole() {
        let body = fx::sysex_payload(50);
        let long = fx::sysex_payload(MIDI_STACK_CAPACITY * 6 + 30);
        let long_packets = sysex_packets(&long, 1);
        let (first, second) = long_packets.split_at(MIDI_STACK_CAPACITY);
        let short = sysex_packets(&body, 3);
        let mut list = InputEventList::new();
        list.reserve(MIDI_STACK_CAPACITY);
        list.reserve_sysex();
        assert_no_alloc::assert_no_alloc(|| {
            list.clear();
            list.add_midi_events(&short);
        });
        assert_eq!(sysex_read_by_plugin(&list), vec![(3, framed(&body))]);
        assert_eq!(list.len(), 1);

        assert_no_alloc::assert_no_alloc(|| {
            list.clear();
            list.add_midi_events(first);
        });
        assert!(sysex_read_by_plugin(&list).is_empty());
        assert_no_alloc::assert_no_alloc(|| {
            list.clear();
            list.add_midi_events(second);
        });
        assert_eq!(sysex_read_by_plugin(&list), vec![(1, framed(&long))]);
        assert_eq!(list.sysex_dropped(), 0);
    }

    /// **Past capacity, or left open by a later `START`, a SysEx is dropped
    /// whole and counted; before `reserve_sysex` every one is.**
    #[test]
    fn sysex_drops_are_whole_and_counted() {
        let mut unreserved = InputEventList::new();
        unreserved.add_midi_events(&sysex_packets(&[1, 2, 3], 0));
        assert!(unreserved.events().is_empty());
        assert_eq!(unreserved.sysex_dropped(), 1);

        let mut list = InputEventList::new();
        list.reserve_sysex();
        let over = fx::sysex_payload(SYSEX_MESSAGE_CAPACITY - 1); // framed: one past
        for block in sysex_packets(&over, 0).chunks(MIDI_STACK_CAPACITY) {
            list.clear();
            list.add_midi_events(block);
            assert!(sysex_read_by_plugin(&list).is_empty());
        }
        assert_eq!(list.sysex_dropped(), 1);

        let a = fx::sysex_payload(30);
        let b = fx::sysex_payload(12);
        let mut midi = sysex_packets(&a, 0);
        midi.truncate(2);
        midi.extend(sysex_packets(&b, 2));
        list.clear();
        list.add_midi_events(&midi);
        assert_eq!(sysex_read_by_plugin(&list), vec![(2, framed(&b))]);
        assert_eq!(list.sysex_dropped(), 2);
    }
}
