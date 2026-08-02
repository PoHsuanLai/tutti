//! CLAP event list implementations.
//!
//! Events wrap the actual clap-sys C structs so that pointers returned by
//! `input_events_get` have the correct C memory layout for plugins to cast.

//! # Narrowing casts are denied in this module
//!
//! This file converts between our vocabulary and the CLAP C ABI, and its worst
//! bug was a cast that changed a value's meaning: CLAP's `-1` wildcard for
//! "all channels / all keys" masked into channel 15, note 127 — one phantom
//! voice, while every real voice kept ringing.
//!
//! So truncating and sign-changing casts are denied here. Where a cast is safe,
//! the `#[allow(..., reason = "...")]` says why; the justification is the point,
//! not the lint. Keep those allows as narrow as the cast they cover — a
//! function-wide allow in the sibling VST3 module silently re-permitted the very
//! wrap that module exists to prevent.
#![deny(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use crate::types::{
    ClapNoteExpression, MidiEvent, NoteExpressionType, ParameterChanges, ParameterPoint,
    ParameterQueue,
};
use clap_sys::events::{
    clap_event_header, clap_event_midi, clap_event_midi_sysex, clap_event_note,
    clap_event_note_expression, clap_event_param_gesture, clap_event_param_mod,
    clap_event_param_value, clap_input_events, clap_output_events, CLAP_CORE_EVENT_SPACE_ID,
    CLAP_EVENT_MIDI, CLAP_EVENT_MIDI_SYSEX, CLAP_EVENT_NOTE_CHOKE, CLAP_EVENT_NOTE_END,
    CLAP_EVENT_NOTE_EXPRESSION, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON,
    CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD,
    CLAP_EVENT_PARAM_VALUE, CLAP_NOTE_EXPRESSION_BRIGHTNESS, CLAP_NOTE_EXPRESSION_EXPRESSION,
    CLAP_NOTE_EXPRESSION_PAN, CLAP_NOTE_EXPRESSION_PRESSURE, CLAP_NOTE_EXPRESSION_TUNING,
    CLAP_NOTE_EXPRESSION_VIBRATO, CLAP_NOTE_EXPRESSION_VOLUME,
};
use std::ptr;
use tutti_plugin_types::{note_id_for, note_id_to_channel_note, ParamAddress, RtMidiEvents};

use crate::types::RtNoteExpressions;
use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};

/// A single CLAP event, wrapping the underlying `#[repr(C)]` `clap_sys`
/// struct so a pointer to its `header` field can be cast back by the plugin.
///
/// Construct via the `note_on`/`note_off`/`midi`/`param_value`/`note_expression`
/// helpers, or from [`MidiEvent`] via [`ClapEvent::from_midi`].
pub enum ClapEvent {
    NoteOn(clap_event_note),
    NoteOff(clap_event_note),
    NoteChoke(clap_event_note),
    NoteEnd(clap_event_note),
    Midi(clap_event_midi),
    NoteExpression(clap_event_note_expression),
    ParamValue(clap_event_param_value),
    ParamMod(clap_event_param_mod),
    ParamGestureBegin(clap_event_param_gesture),
    ParamGestureEnd(clap_event_param_gesture),
    /// Sysex event that owns its buffer. The inner C struct's `buffer`
    /// pointer aliases `_data`, so this variant must not be moved out of
    /// its containing [`InputEventList`]/[`OutputEventList`].
    MidiSysex {
        inner: clap_event_midi_sysex,
        _data: Vec<u8>,
    },
}

// Events contain only POD and owned `Vec<u8>`. The plugin cookie pointer
// is opaque and only ever passed back through to the plugin.
unsafe impl Send for ClapEvent {}
unsafe impl Sync for ClapEvent {}

/// Per-note pitch-bend range, in semitones, used to convert a MIDI-2 per-note
/// pitch bend (signed `[-1, 1]`) to CLAP's `CLAP_NOTE_EXPRESSION_TUNING` value
/// (defined *in semitones*). Matches the synth's MPE default (48-semitone
/// per-note bend range) so a note bent to full scale here lands where the
/// engine's own MPE voices would put it.
const PER_NOTE_PITCH_BEND_RANGE_SEMITONES: f64 = 48.0;

/// Map a MIDI-2 per-note controller index to the CLAP note-expression it
/// corresponds to (volume = 7, pan = 10, brightness = 74), or `None` when
/// there's no standard counterpart.
fn per_note_controller_expression(index: u8) -> Option<NoteExpressionType> {
    match index {
        7 => Some(NoteExpressionType::Volume),
        10 => Some(NoteExpressionType::Pan),
        74 => Some(NoteExpressionType::Brightness),
        _ => None,
    }
}

/// CLAP's upper bound for `CLAP_NOTE_EXPRESSION_VOLUME` (L6).
///
/// The spec (`clap/events.h`) defines VOLUME as a **gain**, not a unit
/// fraction: "with 0 < x <= 4, plain = 20 * log(x)". So 1.0 is unity, 4.0 is
/// +12 dB, and 0 is excluded — a strict inequality, because 20·log(0) is −∞.
const CLAP_VOLUME_MAX_GAIN: f64 = 4.0;

/// Smallest VOLUME gain the host will emit (L6).
///
/// CLAP excludes 0 from the VOLUME range, so a MIDI volume of 0 cannot be sent
/// verbatim. −120 dB is inaudible at any practical bit depth and is what a
/// fader's "−∞" position resolves to in practice, so it stands in for silence
/// while staying inside the legal open interval.
const CLAP_VOLUME_MIN_GAIN: f64 = 1e-6;

/// MIDI-2 per-note volume (unit `0..=1`, full scale = unity) → a CLAP VOLUME
/// gain in the spec's `0 < x <= 4` (L6).
///
/// Unity is preserved: MIDI full scale maps to gain 1.0, matching CLAP's
/// "1.0 = unity". A MIDI controller cannot express boost, so the `1..4` half of
/// the CLAP range is unreachable *from MIDI* — that is correct, not a loss. The
/// bug this replaces was emitting a bare 0.0 at the bottom, which is outside
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

/// Scale a MIDI-2 unit `0..=1` controller value into the CLAP value range of
/// `ty` (L6). Only VOLUME differs from the identity: Pan (`0` left, `0.5`
/// centre, `1` right), Brightness, Expression, Vibrato and Pressure are all
/// genuinely `0..1` per `clap/events.h`.
fn expression_value_from_unit(ty: NoteExpressionType, unit: f32) -> f64 {
    match ty {
        NoteExpressionType::Volume => unit_to_clap_volume(unit),
        _ => f64::from(unit),
    }
}

/// Inverse of [`expression_value_from_unit`] (L6).
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
    match ty {
        NoteExpressionType::Volume => Some(7),
        NoteExpressionType::Pan => Some(10),
        NoteExpressionType::Brightness => Some(74),
        _ => None,
    }
}

/// Resolve a CLAP note event's `(channel, key)` into a concrete MIDI address,
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
/// Shared by the note-on, note-off and note-expression paths. It previously
/// existed only inside the expression path, whose comment already named the
/// phantom-voice hazard while the two note paths masked unguarded.
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

fn note_address(channel: i16, key: i16, note_id: i32) -> Option<(u8, u8)> {
    if channel >= 0 && key >= 0 {
        // Both are `>= 0` in this branch (the wildcard case returned above),
        // and each is masked to the width MIDI wants, so neither narrowing can
        // drop a bit the mask would have kept.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "non-negative in this branch and masked to 4/7 bits"
        )]
        Some((channel as u8 & 0x0F, key as u8 & 0x7F))
    } else {
        note_id_to_channel_note(note_id)
    }
}

// C-4 (deferred): host→plugin events built below leave `header.flags = 0`, so
// `CLAP_EVENT_IS_LIVE` is never set. IS_LIVE marks an event as originating from
// live hardware interaction (a physical knob/key) rather than sequencer
// playback, letting a plugin treat the two differently (e.g. smoothing). The
// live-vs-playback distinction is not threaded from the caller today — the
// `MidiEvent` → `ClapEvent` conversion has no such signal, and neither does the
// process/flush entry path. Rather than invent a bogus source flag, we leave
// IS_LIVE unset until the frontend plumbs a real live/playback origin through.
impl ClapEvent {
    /// Borrow the common CLAP event header (time, type, space ID, flags).
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
            ClapEvent::MidiSysex { inner, .. } => &inner.header,
        }
    }

    /// Mint the host's `note_id` for a `(channel, key)` voice, matching what
    /// [`per_note_expression`](Self::per_note_expression) stamps (H2).
    ///
    /// [`note_id_for`] takes `u8`s; note/off events carry CLAP's signed
    /// `i16` wildcard-capable fields. A wildcard (`< 0`) or out-of-range value
    /// has no `(channel, key)` voice to name, so it yields CLAP's `-1`
    /// "unspecified note id" — which the spec permits and plugins fall back
    /// from by matching on the rest of the (port, channel, key, note_id) tuple.
    fn voice_note_id(channel: i16, key: i16) -> i32 {
        match (u8::try_from(channel), u8::try_from(key)) {
            (Ok(ch), Ok(k)) if ch < 16 && k < 128 => note_id_for(ch, k),
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
            ClapEvent::MidiSysex { inner, .. } => &mut inner.header,
        }
    }

    /// Build a CLAP note-on event. `velocity` is normalized to `[0.0, 1.0]`.
    ///
    /// H2: the `note_id` is minted from `(channel, key)` with the *same*
    /// [`note_id_for`] the note-expression path uses, so a plugin keying its
    /// voice map on `note_id` — the normal MPE-capable design — finds the voice
    /// a later `NOTE_EXPRESSION` refers to. Previously this hardcoded `-1`
    /// while expressions carried a real id, so every per-note expression was
    /// silently dropped by such a plugin.
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

    /// Build a CLAP note-off event. `velocity` is normalized to `[0.0, 1.0]`.
    ///
    /// H2: carries the same `(channel, key)`-derived `note_id` as the matching
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

    /// Build a generic 3-byte MIDI-1 event (status + two data bytes).
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

    /// Build a parameter-value event. Targets every note/port/channel/key
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

    /// Build a note-expression event targeting a note by its CLAP `note_id`.
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

    /// Build a `ClapEvent` from a Tutti UMP [`MidiEvent`].
    ///
    /// Notes are matched on `midi2`'s Channel Voice 2 vocabulary via
    /// [`tutti_midi_types::normalize`] (which folds velocity-0 NoteOn → NoteOff
    /// and promotes any inbound MIDI 1.0), so the 16-bit velocity arrives here at
    /// full width and is normalized to CLAP's `0.0..=1.0` at this edge. Every
    /// other channel-voice message (CC, pitch bend, pressure, program change,
    /// per-note) is forwarded as a generic MIDI-1 `Midi` event — the form CLAP
    /// plugins consume when the host has no parameter mapping for it. Returns
    /// `None` for UMP variants with no MIDI-1 form (SysEx, utility).
    pub fn from_midi(event: &MidiEvent) -> Option<Self> {
        use tutti_midi_types::convert::{bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32};
        use tutti_midi_types::midi2::channel_voice2::{ChannelVoice2 as Cv2, Controller};
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        let time = event.frame_offset;
        let normalized = tutti_midi_types::normalize(event);
        // Raw-bytes fallback for channel-voice messages CLAP has no typed slot
        // for (CC, channel pitch bend/pressure, program, unmapped per-note CCs).
        let as_generic_midi = || -> Option<Self> {
            let (bytes, _len) = event.to_midi1_bytes()?;
            Some(ClapEvent::midi(time, 0, bytes))
        };
        let Ok(UmpMessage::ChannelVoice2(cv2)) = UmpMessage::try_from(normalized.data_words())
        else {
            let (bytes, _len) = event.to_midi1_bytes()?;
            return Some(ClapEvent::midi(time, 0, bytes));
        };
        let channel = u8::from(cv2.channel());
        match cv2 {
            Cv2::NoteOn(m) => Some(ClapEvent::note_on(
                time,
                i16::from(channel),
                i16::from(u8::from(m.note_number())),
                f64::from(u16_to_unit_f32(m.velocity())),
            )),
            Cv2::NoteOff(m) => Some(ClapEvent::note_off(
                time,
                i16::from(channel),
                i16::from(u8::from(m.note_number())),
                // M3: thread the MIDI-2 release velocity through, matching the
                // NoteOn path — a hardcoded 0 discarded the release dynamic.
                f64::from(u16_to_unit_f32(m.velocity())),
            )),
            // MIDI-2 per-note pitch bend → CLAP tuning expression. Signed
            // [-1, 1] scales to semitones by the per-note bend range.
            Cv2::PerNotePitchBend(m) => Self::per_note_expression(
                time,
                NoteExpressionType::Tuning,
                channel,
                u8::from(m.note_number()),
                f64::from(bend_u32_to_signed_f32(m.pitch_bend_data()))
                    * PER_NOTE_PITCH_BEND_RANGE_SEMITONES,
            ),
            // Poly (per-key) pressure → CLAP pressure expression, unit [0, 1].
            Cv2::KeyPressure(m) => Self::per_note_expression(
                time,
                NoteExpressionType::Pressure,
                channel,
                u8::from(m.note_number()),
                f64::from(u32_to_unit_f32(m.key_pressure_data())),
            ),
            // Assignable per-note controllers map to CLAP note-expression for the
            // indices with a standard counterpart (7/10/74); others fall through.
            Cv2::AssignablePerNoteController(m) => {
                match per_note_controller_expression(m.index()) {
                    Some(ty) => Self::per_note_expression(
                        time,
                        ty,
                        channel,
                        u8::from(m.note_number()),
                        // L6: VOLUME is a gain in `0 < x <= 4`, not a unit
                        // fraction — it needs its own scale. Pan/Brightness
                        // really are `0..1`.
                        expression_value_from_unit(ty, u32_to_unit_f32(m.controller_data())),
                    ),
                    None => as_generic_midi(),
                }
            }
            // Registered per-note controllers carry spec meaning by name.
            Cv2::RegisteredPerNoteController(m) => {
                let mapped = match m.controller() {
                    Controller::Volume(d) => Some((NoteExpressionType::Volume, d)),
                    Controller::Pan(d) => Some((NoteExpressionType::Pan, d)),
                    Controller::Brightness(d)
                    | Controller::SoundController { index: 5, data: d } => {
                        Some((NoteExpressionType::Brightness, d))
                    }
                    _ => None,
                };
                match mapped {
                    Some((ty, data)) => Self::per_note_expression(
                        time,
                        ty,
                        channel,
                        u8::from(m.note_number()),
                        // L6: VOLUME uses CLAP's gain range, not `0..1`.
                        expression_value_from_unit(ty, u32_to_unit_f32(data)),
                    ),
                    None => as_generic_midi(),
                }
            }
            // CC / channel pitch-bend / channel pressure / program: raw MIDI-1
            // bytes (or dropped when there's no 3-byte form).
            _ => as_generic_midi(),
        }
    }

    /// Convert a `ClapEvent` back to a Tutti UMP [`MidiEvent`].
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
    pub fn to_midi(&self) -> Option<MidiEvent> {
        use tutti_midi_types::convert::unit_f32_to_u16;
        match self {
            ClapEvent::NoteOn(e) => {
                let (channel, note) = note_address(e.channel, e.key, e.note_id)?;
                Some(
                    MidiEvent::note_on(
                        MidiGroup::FIRST,
                        MidiChannel::new(channel),
                        note,
                        unit_f32_to_u16(e.velocity as f32),
                    )
                    .with_frame_offset(e.header.time),
                )
            }
            ClapEvent::NoteOff(e) => {
                let (channel, note) = note_address(e.channel, e.key, e.note_id)?;
                Some(
                    // Preserve the plugin's release velocity on the way back to
                    // MIDI-2, mirroring the NoteOn path (was hardcoded 0).
                    MidiEvent::note_off(
                        MidiGroup::FIRST,
                        MidiChannel::new(channel),
                        note,
                        unit_f32_to_u16(e.velocity as f32),
                    )
                    .with_frame_offset(e.header.time),
                )
            }
            ClapEvent::NoteExpression(e) => Self::note_expression_to_midi(e),
            // Promote the MIDI-1 bytes to Channel Voice 2 at this edge, so the
            // engine sees one vocabulary regardless of source — matching the
            // hardware input path. System / SysEx messages pass through unchanged.
            ClapEvent::Midi(e) => MidiEvent::from_midi1_bytes(e.header.time, &e.data)
                .map(|m| tutti_midi_types::normalize(&m)),
            _ => None,
        }
    }

    /// Rebuild the MIDI-2 per-note [`MidiEvent`] a CLAP note-expression stands
    /// for: Tuning → per-note pitch bend, Pressure → poly pressure,
    /// Volume/Pan/Brightness → the matching per-note controller. `(channel, note)`
    /// come from the event's `channel`/`key` when the host stamped them, else
    /// from decoding its `note_id`. Returns `None` for dimensions with no MIDI-2
    /// per-note counterpart (Vibrato, Expression).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "CLAP expression values are f64; the MIDI-2 helpers take f32"
    )]
    fn note_expression_to_midi(e: &clap_event_note_expression) -> Option<MidiEvent> {
        use tutti_midi_types::convert::{signed_f32_to_bend_u32, unit_f32_to_u32};

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
                let signed = (e.value / PER_NOTE_PITCH_BEND_RANGE_SEMITONES) as f32;
                MidiEvent::per_note_pitch_bend(
                    MidiGroup::FIRST,
                    MidiChannel::new(channel),
                    note,
                    signed_f32_to_bend_u32(signed),
                )
            }
            NoteExpressionType::Pressure => MidiEvent::poly_pressure(
                MidiGroup::FIRST,
                MidiChannel::new(channel),
                note,
                unit_f32_to_u32(e.value as f32),
            ),
            other => {
                let index = expression_to_per_note_controller_index(other)?;
                MidiEvent::per_note_controller(
                    MidiGroup::FIRST,
                    MidiChannel::new(channel),
                    note,
                    index,
                    // L6: VOLUME arrives as a CLAP gain (`0 < x <= 4`), not a
                    // unit fraction — convert before the unit encoding, or a
                    // plugin's unity 1.0 and its +12 dB 4.0 both saturate to
                    // MIDI full scale indistinguishably.
                    unit_f32_to_u32(expression_value_to_unit(other, e.value)),
                    false,
                )
            }
        };
        Some(event.with_frame_offset(time))
    }

    /// Build a note-expression event bound to a `(channel, note)` voice via
    /// [`note_id_for`], also stamping `channel`/`key` so a plugin can match on
    /// those as well as `note_id`.
    fn per_note_expression(
        time: u32,
        expression_type: NoteExpressionType,
        channel: u8,
        note: u8,
        value: f64,
    ) -> Option<Self> {
        let mut event =
            Self::note_expression(time, expression_type, note_id_for(channel, note), value)?;
        if let ClapEvent::NoteExpression(ne) = &mut event {
            ne.channel = channel as i16;
            ne.key = note as i16;
        }
        Some(event)
    }
}

/// Common interface shared by [`InputEventList`] and [`OutputEventList`].
pub trait EventList {
    /// Number of events currently held in the list.
    fn len(&self) -> usize;

    /// Whether the list has no events.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove all events.
    fn clear(&mut self);
}

/// Owned list of input events passed into `clap_plugin.process()`.
///
/// The `#[repr(C)]` layout places `clap_input_events` first so that
/// [`Self::as_raw`] pointers are valid for the plugin's FFI expectations.
#[repr(C)]
pub struct InputEventList {
    pub(crate) list: clap_input_events,
    pub(crate) events: Vec<ClapEvent>,
}

impl InputEventList {
    /// Create an empty input event list.
    pub fn new() -> Self {
        Self {
            list: clap_input_events {
                ctx: ptr::null_mut(),
                size: Some(input_events_size),
                get: Some(input_events_get),
            },
            events: Vec::new(),
        }
    }

    /// Create an input event list pre-populated with the given events.
    pub fn from_events(events: Vec<ClapEvent>) -> Self {
        Self {
            list: clap_input_events {
                ctx: ptr::null_mut(),
                size: Some(input_events_size),
                get: Some(input_events_get),
            },
            events,
        }
    }

    /// Reserve heap capacity for at least `n` events. Off-RT only; call
    /// once during plugin activation so steady-state `add_*` calls don't
    /// touch the allocator. `clear()` keeps the reserved capacity.
    pub fn reserve(&mut self, n: usize) {
        self.events.reserve(n);
    }

    /// Convert a [`MidiEvent`] to a [`ClapEvent`] and append it. UMP
    /// variants without a MIDI-1 form are silently skipped.
    pub fn add_midi(&mut self, event: &MidiEvent) -> &mut Self {
        if let Some(clap_event) = ClapEvent::from_midi(event) {
            self.events.push(clap_event);
        }
        self
    }

    /// Batch version of [`add_midi`](Self::add_midi).
    pub fn add_midi_events(&mut self, events: &[MidiEvent]) -> &mut Self {
        for event in events {
            if let Some(clap_event) = ClapEvent::from_midi(event) {
                self.events.push(clap_event);
            }
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
    pub fn add_param_changes(
        &mut self,
        changes: &ParameterChanges,
        ranges: &[(u32, f32, f32)],
        plugin_claims_params: bool,
    ) -> &mut Self {
        for queue in &changes.queues {
            // `clap_id` is opaque; a VST2 positional index addresses nothing
            // here. The queue used to carry a bare number, so this arm could
            // not exist — a wrong-model address was indistinguishable from a
            // real id and reached the plugin as one.
            let Some(param_id) = queue.param_id.opaque().map(|id| id.get()) else {
                continue;
            };
            let range = ranges.iter().find(|(id, _, _)| *id == param_id);
            if range.is_none() && plugin_claims_params {
                continue;
            }
            for point in &queue.points {
                let value = match range {
                    Some(&(_, min, max)) => {
                        // CLAP normalizes automation to f64 `0..=1`; the plain
                        // range is f32, so the product is computed in f32.
                        #[allow(
                            clippy::cast_possible_truncation,
                            reason = "normalized 0..=1 from CLAP; the plain range is f32"
                        )]
                        let normalized = point.value as f32;
                        let plain = min + normalized * (max - min);
                        // `f32::clamp` panics when `lo > hi`, and `min`/`max`
                        // come straight from the plugin's own reported
                        // `min_value`/`max_value` (lifecycle.rs:93). A plugin
                        // reporting inverted bounds is malformed, but it must
                        // not panic the audio thread — order the bounds first.
                        plain.clamp(min.min(max), min.max(max)) as f64
                    }
                    None => point.value,
                };
                self.events.push(ClapEvent::param_value(
                    // H3: `sample_offset` is `i32`; a bare `as u32` turns a
                    // negative offset into ~4.29 billion, which sorts last and
                    // is handed to the plugin as a buffer index. Saturate at 0
                    // — the change is simply already due. The upper bound is
                    // enforced once for the whole list by `clamp_times`, which
                    // is the only place `frames_count` is known.
                    point.sample_offset.max(0) as u32,
                    param_id,
                    value,
                ));
            }
        }
        self
    }

    /// Append each [`ClapNoteExpression`] as a CLAP `NOTE_EXPRESSION` event.
    /// A `Custom` dimension is skipped — CLAP has no id for one, and
    /// [`ClapEvent::note_expression`] says why substituting would be worse.
    pub fn add_note_expressions(&mut self, expressions: &[ClapNoteExpression]) -> &mut Self {
        for expr in expressions {
            if let Some(event) = ClapEvent::note_expression(
                // H3: same signed→unsigned trap as `add_param_changes`.
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

    /// Clamp every event's `header.time` into `0..frames_count` (H3).
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
    /// be in non-decreasing time order.
    pub fn sort_by_time(&mut self) -> &mut Self {
        self.events.sort_by_key(|e| e.header().time);
        self
    }

    /// Raw pointer to the `clap_input_events` struct for FFI. Valid only
    /// while `self` is not moved or dropped.
    pub fn as_raw(&self) -> *const clap_input_events {
        &self.list as *const _ as *const _
    }

    /// Borrow the events currently in the list.
    pub fn events(&self) -> &[ClapEvent] {
        &self.events
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

    fn clear(&mut self) {
        self.events.clear();
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "one block's event count; the CLAP vtable returns u32"
)]
unsafe extern "C" fn input_events_size(list: *const clap_input_events) -> u32 {
    let event_list = &*(list as *const InputEventList);
    event_list.events.len() as u32
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "one block's event count; the CLAP vtable indexes with u32"
)]
unsafe extern "C" fn input_events_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let event_list = &*(list as *const InputEventList);
    if index >= event_list.events.len() as u32 {
        return ptr::null();
    }
    event_list.events[index as usize].header() as *const _
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
/// As with [`InputEventList`], `#[repr(C)]` puts `clap_output_events` first
/// so the FFI pointer returned by [`Self::as_raw_mut`] has the correct shape.
#[repr(C)]
pub struct OutputEventList {
    pub(crate) list: clap_output_events,
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
}

impl OutputEventList {
    /// Create an empty output event list.
    pub fn new() -> Self {
        Self {
            list: clap_output_events {
                ctx: ptr::null_mut(),
                try_push: Some(output_events_try_push),
            },
            events: Vec::new(),
            sysex_pool: Vec::new(),
        }
    }

    /// Raw pointer to the `clap_output_events` struct for FFI. Valid only
    /// while `self` is not moved or dropped.
    pub fn as_raw_mut(&mut self) -> *mut clap_output_events {
        &mut self.list as *mut _ as *mut _
    }

    /// Borrow the events the plugin has pushed.
    pub fn events(&self) -> &[ClapEvent] {
        &self.events
    }

    /// Move all events out of the list, leaving it empty.
    pub fn take_events(&mut self) -> Vec<ClapEvent> {
        std::mem::take(&mut self.events)
    }

    /// Reserve heap capacity for at least `n` events. Off-RT only; call
    /// once during plugin activation so the plugin's `try_push` callback
    /// doesn't grow the inner Vec.
    ///
    /// Also pre-warms the SysEx payload pool: the pool is fed only by `clear`,
    /// so without priming, the first block emitting SysEx still allocates on
    /// the audio thread.
    pub fn reserve(&mut self, n: usize) {
        self.events.reserve(n);

        let wanted = n.min(SYSEX_POOL_PREWARM);
        self.sysex_pool.reserve(wanted);
        while self.sysex_pool.len() < wanted {
            self.sysex_pool
                .push(Vec::with_capacity(SYSEX_PREWARM_BYTES));
        }
    }

    /// Extract MIDI events from the output as UMP [`MidiEvent`]s,
    /// dropping non-MIDI events.
    pub fn to_midi_events(&self) -> Vec<MidiEvent> {
        self.events.iter().filter_map(|e| e.to_midi()).collect()
    }

    /// RT-safe variant of [`Self::to_midi_events`] that drains into a
    /// caller-supplied pooled `SmallVec`. Clears `out` first; reuses
    /// existing heap capacity.
    pub fn fill_midi_events(&self, out: &mut RtMidiEvents) {
        out.clear();
        for e in &self.events {
            if let Some(midi) = e.to_midi() {
                if !out.push(midi) {
                    break;
                }
            }
        }
    }

    /// Extract parameter-value events into a [`ParameterChanges`] grouped
    /// by parameter ID.
    pub fn to_param_changes(&self) -> ParameterChanges {
        let mut changes = ParameterChanges::new();
        self.fill_param_changes(&mut changes);
        changes
    }

    /// RT-safe variant of [`Self::to_param_changes`] that drains into a
    /// caller-supplied pooled `ParameterChanges`. Clears `out.queues` first
    /// and groups events by `param_id` via a linear scan over the inline
    /// `SmallVec<[ParameterQueue; 16]>` — at the single-digit-N typical of
    /// plugin param emission, this beats a `HashMap` and avoids the
    /// allocator entirely.
    pub fn fill_param_changes(&self, out: &mut ParameterChanges) {
        // Drop previous queues' points without freeing the queues' heap
        // capacity: clear in place, then the queue order is rebuilt below.
        for queue in &mut out.queues {
            queue.points.clear();
        }
        out.queues.clear();

        for event in &self.events {
            let ClapEvent::ParamValue(e) = event else {
                continue;
            };
            let point = ParameterPoint {
                // Saturate rather than wrap: `header.time` is `u32` and
                // `ParameterPoint::sample_offset` is `i32`, so a bare cast turns
                // a large offset negative and the automation point lands before
                // the block. Found by the module's cast deny; the identical bug
                // in the VST3 event path was fixed separately.
                sample_offset: i32::try_from(e.header.time).unwrap_or(i32::MAX),
                value: e.value,
            };
            // Linear scan: distinct param_ids per block are typically ≤8;
            // a SmallVec scan stays in cache and is fully branch-predicted.
            // Outbound: a CLAP id is opaque, so it is tagged as such rather
            // than travelling as a number the receiver has to guess about.
            let addr = ParamAddress::Opaque(e.param_id.into());
            if let Some(queue) = out.queues.iter_mut().find(|q| q.param_id == addr) {
                queue.points.push(point);
            } else {
                let mut queue = ParameterQueue::new(addr);
                queue.points.push(point);
                out.queues.push(queue);
            }
        }
    }

    /// Extract note-expression events into the safe
    /// [`ClapNoteExpression`] form, dropping other events.
    pub fn to_note_expressions(&self) -> Vec<ClapNoteExpression> {
        self.events
            .iter()
            .filter_map(clap_event_to_note_expression)
            .collect()
    }

    /// RT-safe variant of [`Self::to_note_expressions`] that drains into a
    /// caller-supplied pooled `SmallVec`.
    pub fn fill_note_expressions(&self, out: &mut RtNoteExpressions) {
        out.clear();
        for event in &self.events {
            if let Some(ne) = clap_event_to_note_expression(event) {
                if !out.push(ne) {
                    break;
                }
            }
        }
    }

    /// Drain the plugin's output-side param *gestures* (begin/end) and
    /// param *modulation* events into a caller-supplied pooled `Vec`, clearing
    /// it first. These carry information (a knob-drag beginning/ending, or an
    /// output modulation) that the shared param-value vocabulary can't express,
    /// so rather than drop them silently ([`fill_param_changes`] only matches
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

/// Decode a single [`ClapEvent`] into a [`ClapNoteExpression`]. Returns
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

    /// Empty the list, **recycling** SysEx payload buffers into
    /// [`Self::sysex_pool`] rather than freeing them. `process` calls this every
    /// block; a plain `events.clear()` dropped the `Vec<u8>` inside each
    /// `MidiSysex`, pairing a free here with an allocation in `try_push`.
    fn clear(&mut self) {
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

#[allow(
    clippy::cast_possible_truncation,
    reason = "SysEx payloads are bounded well below u32::MAX"
)]
unsafe extern "C" fn output_events_try_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    if event.is_null() || list.is_null() {
        return false;
    }

    let output_list = &mut *(list as *mut OutputEventList);
    let header = &*event;

    match header.type_ {
        CLAP_EVENT_NOTE_ON => {
            let e = &*(event as *const clap_event_note);
            output_list.events.push(ClapEvent::NoteOn(*e));
            true
        }
        CLAP_EVENT_NOTE_OFF => {
            let e = &*(event as *const clap_event_note);
            output_list.events.push(ClapEvent::NoteOff(*e));
            true
        }
        CLAP_EVENT_MIDI => {
            let e = &*(event as *const clap_event_midi);
            output_list.events.push(ClapEvent::Midi(*e));
            true
        }
        CLAP_EVENT_NOTE_EXPRESSION => {
            let e = &*(event as *const clap_event_note_expression);
            output_list.events.push(ClapEvent::NoteExpression(*e));
            true
        }
        CLAP_EVENT_NOTE_CHOKE => {
            let e = &*(event as *const clap_event_note);
            output_list.events.push(ClapEvent::NoteChoke(*e));
            true
        }
        CLAP_EVENT_NOTE_END => {
            let e = &*(event as *const clap_event_note);
            output_list.events.push(ClapEvent::NoteEnd(*e));
            true
        }
        CLAP_EVENT_PARAM_VALUE => {
            let e = &*(event as *const clap_event_param_value);
            output_list.events.push(ClapEvent::ParamValue(*e));
            true
        }
        CLAP_EVENT_PARAM_MOD => {
            let e = &*(event as *const clap_event_param_mod);
            output_list.events.push(ClapEvent::ParamMod(*e));
            true
        }
        CLAP_EVENT_PARAM_GESTURE_BEGIN => {
            let e = &*(event as *const clap_event_param_gesture);
            output_list.events.push(ClapEvent::ParamGestureBegin(*e));
            true
        }
        CLAP_EVENT_PARAM_GESTURE_END => {
            let e = &*(event as *const clap_event_param_gesture);
            output_list.events.push(ClapEvent::ParamGestureEnd(*e));
            true
        }
        CLAP_EVENT_MIDI_SYSEX => {
            let e = &*(event as *const clap_event_midi_sysex);
            if !e.buffer.is_null() && e.size > 0 {
                // Recycle a pooled buffer instead of `to_vec()`-ing a fresh
                // one: the plugin calls this from inside its own `process`, so
                // that was a per-event, per-block allocation on the audio
                // thread.
                let mut data = output_list.sysex_pool.pop().unwrap_or_default();
                data.clear();
                data.extend_from_slice(std::slice::from_raw_parts(e.buffer, e.size as usize));
                // The buffer pointer aliases `data`; the ClapEvent::MidiSysex
                // variant keeps both together and is never moved independently.
                // Taken *after* the extend, since that is the last thing that
                // can reallocate `data` and move its bytes.
                let inner = clap_event_midi_sysex {
                    header: *header,
                    port_index: e.port_index,
                    buffer: data.as_ptr(),
                    size: data.len() as u32,
                };
                output_list
                    .events
                    .push(ClapEvent::MidiSysex { inner, _data: data });
            }
            true
        }
        _ => false,
    }
}

/// Tests construct CLAP structs by hand and so repeat the bounded narrowings
/// justified above (event counts, SysEx sizes, small loop indices). Allowed at
/// module scope: a test that overflows one of these fails on its own assertion,
/// and keeping the deny tight in the ABI-crossing code is what makes it useful.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::RT_MIDI_CAPACITY;

    /// A huge event time must saturate, not wrap negative.
    ///
    /// `clap_event_header::time` is `u32` and `ParameterPoint::sample_offset` is
    /// `i32`, so a bare `as i32` sends a large offset negative — the automation
    /// point then sorts before the block instead of after it. Found by this
    /// module's cast deny, in two places; the identical bug on the VST3 event
    /// path was fixed separately.
    ///
    /// `u32::MAX` is not a realistic block offset, but it is the value that
    /// distinguishes saturation from wrapping, which is the whole property.
    #[test]
    fn a_huge_event_time_saturates_rather_than_going_negative() {
        let mut list = OutputEventList::new();
        list.events.push(ClapEvent::param_value(u32::MAX, 7, 0.5));

        let mut changes = ParameterChanges::default();
        list.fill_param_changes(&mut changes);

        let point = changes
            .queues
            .iter()
            .find(|q| q.param_id == ParamAddress::Opaque(7u32.into()))
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
    use tutti_midi_types::convert::{signed_f32_to_bend_u32, unit_f32_to_u32};

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

    /// A VST2 positional index addresses no CLAP parameter and must be dropped,
    /// not read as a `clap_id`.
    ///
    /// The two models are numerically identical — `Index(9)` and the opaque id
    /// `9` are the same number — so while the queue carried a bare `u32` this
    /// arm could not exist: a wrong-model address was indistinguishable from a
    /// real id and reached the plugin as one. Index 9 here would denormalize
    /// against param 9's `[100, 1100]` range and set a parameter the caller
    /// never named.
    ///
    /// Mutation that catches it: replacing the `opaque()` guard with
    /// `.unwrap_or(0)` — the index is then accepted and an event is emitted.
    #[test]
    fn add_param_changes_refuses_a_vst2_index() {
        // Index 0 rather than 9, and a declared range for param 0: an index
        // that fell through to `unwrap_or(0)` would find a real range and be
        // emitted. Picking an index with no matching range would let the
        // existing `plugin_claims_params` arm drop it, and the test would pass
        // without the model check ever running.
        let mut changes = ParameterChanges::new();
        changes.add_change(ParamAddress::Index(0), 0, 0.25);
        let mut list = InputEventList::new();
        list.add_param_changes(&changes, &[(0, 100.0, 1100.0)], true);
        assert!(
            list.events.is_empty(),
            "a VST2 index addresses nothing in CLAP; it must not become an event"
        );

        // The same number as an opaque id is a real address, so the refusal is
        // about the model and not about the value being rejected outright.
        let mut ok = ParameterChanges::new();
        ok.add_change(ParamAddress::Opaque(0u32.into()), 0, 0.25);
        let mut list2 = InputEventList::new();
        list2.add_param_changes(&ok, &[(0, 100.0, 1100.0)], true);
        assert!((first_param_value(&list2) - 350.0).abs() < 1e-6);
    }

    #[test]
    fn add_param_changes_denormalizes_against_range() {
        // Param 9 has plain range [100, 1100]; a normalized 0.25 → 350.
        let mut changes = ParameterChanges::new();
        changes.add_change(ParamAddress::Opaque(9u32.into()), 0, 0.25);
        let mut list = InputEventList::new();
        list.add_param_changes(&changes, &[(9, 100.0, 1100.0)], true);
        assert!((first_param_value(&list) - 350.0).abs() < 1e-6);
    }

    #[test]
    fn add_param_changes_clamps_denormalized_value_into_range() {
        // A normalized 1.5 (over-range) must clamp to the plain max, not overshoot.
        let mut changes = ParameterChanges::new();
        changes.add_change(ParamAddress::Opaque(9u32.into()), 0, 1.5);
        let mut list = InputEventList::new();
        list.add_param_changes(&changes, &[(9, 0.0, 10.0)], true);
        assert!((first_param_value(&list) - 10.0).abs() < 1e-6);
    }

    #[test]
    fn add_param_changes_survives_a_plugin_reporting_inverted_bounds() {
        // `f32::clamp` panics when `lo > hi`, and these bounds are whatever the
        // plugin reported. A malformed plugin must not take down the audio
        // thread — the value lands inside the range either way round.
        let mut changes = ParameterChanges::new();
        changes.add_change(ParamAddress::Opaque(9u32.into()), 0, 0.5);
        let mut list = InputEventList::new();
        list.add_param_changes(&changes, &[(9, 10.0, 0.0)], true);
        let v = first_param_value(&list);
        assert!((0.0..=10.0).contains(&v), "value {v} escaped the range");
    }

    #[test]
    fn add_param_changes_passes_through_when_range_unknown() {
        // No range for this param id → value forwarded unchanged (safe fallback
        // for the common normalized-0..1 param).
        let mut changes = ParameterChanges::new();
        changes.add_change(ParamAddress::Opaque(9u32.into()), 0, 0.42);
        let mut list = InputEventList::new();
        list.add_param_changes(&changes, &[], false);
        assert!((first_param_value(&list) - 0.42).abs() < 1e-6);
    }

    // --- H3: event time is bounded to the block ---

    fn first_time(list: &InputEventList) -> u32 {
        list.events().first().expect("an event").header().time
    }

    /// A NEGATIVE `sample_offset` must not wrap.
    ///
    /// `sample_offset` is `i32` and `header.time` is `u32`; the old bare
    /// `point.sample_offset as u32` turned -1 into 4_294_967_295, which sorted
    /// last and was handed to the plugin as a sample index into the block.
    /// Plugins split their buffer on `time`, so that is an out-of-bounds access
    /// inside the plugin.
    #[test]
    fn negative_sample_offset_does_not_wrap_to_four_billion_h3() {
        let mut changes = ParameterChanges::new();
        changes.add_change(ParamAddress::Opaque(9u32.into()), -1, 0.5);
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
        list.add_midi(
            &MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x4000)
                .with_frame_offset(9_999),
        );
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
        list.add_midi(
            &MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(17),
        );
        list.clamp_times(64);
        assert_eq!(first_time(&list), 17);

        list.clamp_times(0);
        assert_eq!(first_time(&list), 0);
    }

    // --- L6: CLAP VOLUME is a gain in `0 < x <= 4`, not a unit fraction ---

    /// `clap/events.h` defines
    /// `CLAP_NOTE_EXPRESSION_VOLUME` as "with 0 < x <= 4, plain = 20 * log(x)"
    /// — a gain where 1.0 is unity and 0 is *excluded*. The host used to emit a
    /// bare `0..1` unit value, so a MIDI volume of 0 produced an out-of-range
    /// 0.0.
    #[test]
    fn per_note_volume_maps_into_claps_gain_range_l6() {
        // Full-scale MIDI volume is unity gain, not "the top of 0..1".
        let full = MidiEvent::per_note_controller(
            MidiGroup::FIRST,
            MidiChannel::new(2),
            64,
            7,
            u32::MAX,
            false,
        );
        let ClapEvent::NoteExpression(e) = ClapEvent::from_midi(&full).expect("converts") else {
            panic!("expected NoteExpression");
        };
        assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_VOLUME);
        assert!(
            (e.value - 1.0).abs() < 1e-3,
            "full MIDI volume must be CLAP unity (1.0), got {}",
            e.value
        );

        // Zero MIDI volume must stay inside the OPEN interval: `0 < x`.
        let zero =
            MidiEvent::per_note_controller(MidiGroup::FIRST, MidiChannel::new(2), 64, 7, 0, false);
        let ClapEvent::NoteExpression(e) = ClapEvent::from_midi(&zero).expect("converts") else {
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

    #[test]
    fn clap_volume_round_trips_and_saturates_boost_l6() {
        use tutti_midi_types::convert::u32_to_unit_f32;
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::UmpMessage;

        let unit_of = |gain: f64| -> f32 {
            let clap = ClapEvent::per_note_expression(0, NoteExpressionType::Volume, 1, 64, gain)
                .expect("Volume is a named CLAP dimension");
            let midi = clap.to_midi().expect("volume -> midi");
            match UmpMessage::try_from(midi.data_words()).expect("UMP") {
                UmpMessage::ChannelVoice2(Cv2::AssignablePerNoteController(m)) => {
                    u32_to_unit_f32(m.controller_data())
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
        let midi = MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::new(3), 60, bend)
            .with_frame_offset(11);
        match ClapEvent::from_midi(&midi).expect("per-note bend converts") {
            ClapEvent::NoteExpression(e) => {
                assert_eq!(e.header.time, 11);
                assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_TUNING);
                assert_eq!(e.note_id, note_id_for(3, 60));
                assert_eq!(e.channel, 3);
                assert_eq!(e.key, 60);
                let expected = 0.5 * PER_NOTE_PITCH_BEND_RANGE_SEMITONES;
                assert!(
                    (e.value - expected).abs() < 0.1,
                    "tuning {} vs {expected}",
                    e.value
                );
            }
            _ => panic!("expected NoteExpression(Tuning)"),
        }
    }

    #[test]
    fn tuning_note_expression_round_trips_to_per_note_pitch_bend() {
        use tutti_midi_types::convert::bend_u32_to_signed_f32;
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        let value_semitones = 0.5 * PER_NOTE_PITCH_BEND_RANGE_SEMITONES;
        let clap =
            ClapEvent::per_note_expression(7, NoteExpressionType::Tuning, 5, 67, value_semitones)
                .expect("Tuning is a named CLAP dimension");
        let midi = clap.to_midi().expect("tuning -> midi");
        assert_eq!(midi.frame_offset, 7);
        match UmpMessage::try_from(midi.data_words()).expect("UMP") {
            UmpMessage::ChannelVoice2(Cv2::PerNotePitchBend(m)) => {
                assert_eq!(u8::from(m.channel()), 5);
                assert_eq!(u8::from(m.note_number()), 67);
                let value = bend_u32_to_signed_f32(m.pitch_bend_data());
                assert!((value - 0.5).abs() < 0.01, "bend {value}");
            }
            other => panic!("expected PerNotePitchBend, got {other:?}"),
        }
    }

    #[test]
    fn key_pressure_becomes_pressure_note_expression() {
        let midi = MidiEvent::poly_pressure(
            MidiGroup::FIRST,
            MidiChannel::new(2),
            48,
            unit_f32_to_u32(0.75),
        );
        match ClapEvent::from_midi(&midi).expect("poly pressure converts") {
            ClapEvent::NoteExpression(e) => {
                assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_PRESSURE);
                assert_eq!(e.note_id, note_id_for(2, 48));
                assert!((e.value - 0.75).abs() < 0.01, "pressure {}", e.value);
            }
            _ => panic!("expected NoteExpression(Pressure)"),
        }
    }

    #[test]
    fn assignable_per_note_controller_maps_to_brightness() {
        // Assignable per-note controller index 74 (CC74) → CLAP _BRIGHTNESS.
        let midi = MidiEvent::per_note_controller(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            64,
            74,
            unit_f32_to_u32(0.6),
            false,
        );
        match ClapEvent::from_midi(&midi).expect("per-note cc converts") {
            ClapEvent::NoteExpression(e) => {
                assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_BRIGHTNESS);
                assert_eq!(e.note_id, note_id_for(1, 64));
                assert!((e.value - 0.6).abs() < 0.01, "brightness {}", e.value);
            }
            _ => panic!("expected NoteExpression(Brightness)"),
        }
    }

    #[test]
    fn per_note_controller_without_counterpart_falls_through_to_midi() {
        // Per-note CC index 20 has no CLAP expression dimension → generic Midi.
        let midi = MidiEvent::per_note_controller(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            20,
            unit_f32_to_u32(0.5),
            false,
        );
        assert!(matches!(
            ClapEvent::from_midi(&midi),
            Some(ClapEvent::Midi(_)) | None
        ));
    }

    #[test]
    fn test_output_events_push_note_on() {
        let mut output = OutputEventList::new();
        let event = ClapEvent::note_on(0, 0, 60, 0.8);
        let header = event.header();

        let list_ptr = output.as_raw_mut();
        unsafe {
            let push_fn = (*list_ptr).try_push.unwrap();
            let result = push_fn(list_ptr, header as *const clap_event_header);
            assert!(result);
        }

        assert_eq!(output.events().len(), 1);
    }

    #[test]
    fn from_midi_event_note_on_decodes_to_typed_note_on() {
        let midi = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 60, 0x8000)
            .with_frame_offset(7);
        match ClapEvent::from_midi(&midi).expect("note on converts") {
            ClapEvent::NoteOn(e) => {
                assert_eq!(e.header.time, 7);
                assert_eq!(e.channel, 1);
                assert_eq!(e.key, 60);
                assert!((e.velocity - 0.5).abs() < 0.01, "velocity {}", e.velocity);
                // H2: the note_id must match what the expression path mints.
                assert_eq!(e.note_id, note_id_for(1, 60));
            }
            _ => panic!("expected NoteOn"),
        }
    }

    /// The `note_id` a NOTE_ON carries must equal the one a
    /// later NOTE_EXPRESSION for the same voice carries.
    ///
    /// `note_on`/`note_off` used to hardcode `note_id: -1` while
    /// `per_note_expression` minted a real id via `note_id_for`. A plugin that
    /// keys its voice map on `note_id` — the normal MPE-capable design — then
    /// found no voice with id 444 and **silently dropped every per-note
    /// expression**. This asserts the *pairing*, which neither half's existing
    /// round-trip test did: they each checked one event in isolation.
    #[test]
    fn note_on_and_expression_agree_on_note_id_h2() {
        const CH: u8 = 3;
        const KEY: u8 = 60;

        let on = ClapEvent::from_midi(
            &MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(CH), KEY, 0x8000)
                .with_frame_offset(0),
        )
        .expect("note on converts");
        let expr = ClapEvent::from_midi(
            &MidiEvent::poly_pressure(
                MidiGroup::FIRST,
                MidiChannel::new(CH),
                KEY,
                unit_f32_to_u32(0.5),
            )
            .with_frame_offset(1),
        )
        .expect("poly pressure converts");
        let off = ClapEvent::from_midi(
            &MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(CH), KEY, 0x4000)
                .with_frame_offset(2),
        )
        .expect("note off converts");

        let (ClapEvent::NoteOn(on), ClapEvent::NoteExpression(expr), ClapEvent::NoteOff(off)) =
            (&on, &expr, &off)
        else {
            panic!("expected NoteOn / NoteExpression / NoteOff");
        };

        assert_ne!(
            on.note_id, -1,
            "NOTE_ON must carry a real note_id, not the -1 wildcard"
        );
        assert_eq!(
            on.note_id, expr.note_id,
            "the expression targets a voice the note-on never opened — a plugin \
             keying on note_id drops it"
        );
        assert_eq!(
            off.note_id, on.note_id,
            "the note-off must release the voice the note-on opened"
        );
        assert_eq!(on.note_id, note_id_for(CH, KEY));
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
        // M3: a MIDI-2 NoteOff carries a release velocity; it must reach the
        // CLAP NoteOff's `velocity` (was hardcoded 0), and round-trip back.
        use tutti_midi_types::convert::u16_to_unit_f32;
        let rel = 0x6000u16; // ~0.375 of full scale
        let midi = MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(4), 55, rel)
            .with_frame_offset(9);
        match ClapEvent::from_midi(&midi).expect("note off converts") {
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
        use tutti_midi_types::convert::u16_to_unit_f32;
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::UmpMessage;

        let clap = ClapEvent::note_off(2, 1, 48, 0.5);
        let midi = clap.to_midi().expect("note off -> midi");
        match UmpMessage::try_from(midi.data_words()).expect("decodes") {
            UmpMessage::ChannelVoice2(Cv2::NoteOff(m)) => {
                let vel = u16_to_unit_f32(m.velocity());
                assert!((vel - 0.5).abs() < 0.01, "release velocity {vel}");
            }
            other => panic!("expected CV2 NoteOff, got {other:?}"),
        }
    }

    #[test]
    fn from_midi_event_velocity_zero_note_on_becomes_note_off() {
        // The MIDI velocity-0 NoteOn quirk: `normalize` folds it to a NoteOff, so
        // a raw MIDI-1 velocity-0 NoteOn converts to a CLAP NoteOff.
        let midi = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 0]).expect("builds");
        match ClapEvent::from_midi(&midi).expect("converts") {
            ClapEvent::NoteOff(e) => {
                assert_eq!(e.key, 60);
            }
            _ => panic!("expected NoteOff for vel-0 NoteOn"),
        }
    }

    #[test]
    fn from_midi_event_cc_forwards_as_generic_midi() {
        let midi = MidiEvent::cc(MidiGroup::FIRST, MidiChannel::FIRST, 7, 0x8000_0000); // volume, ~half
        match ClapEvent::from_midi(&midi).expect("cc converts") {
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
        // (mirrors the hardware input path's `normalize` promotion).
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        // A generic CLAP Midi event carrying raw MIDI-1 CC bytes (0xB1, 74, 100).
        let clap = ClapEvent::midi(0, 0, [0xB1, 74, 100]);
        let midi = clap.to_midi().expect("cc decodes");
        match UmpMessage::try_from(midi.data_words()).expect("valid UMP") {
            UmpMessage::ChannelVoice2(Cv2::ControlChange(m)) => {
                assert_eq!(u8::from(m.channel()), 1);
                assert_eq!(u8::from(m.control()), 74);
                assert_eq!(m.control_change_data(), midi1_cc_to_midi2(100));
            }
            other => panic!("expected CV2 ControlChange, got {other:?}"),
        }
    }

    #[test]
    fn inbound_system_message_passes_through() {
        // A System real-time message (Timing Clock 0xF8) has no CV form and must
        // pass through `normalize` unchanged, not be dropped or promoted.
        use tutti_midi_types::midi2::UmpMessage;

        let clap = ClapEvent::midi(0, 0, [0xF8, 0, 0]);
        let midi = clap.to_midi().expect("clock decodes");
        assert!(
            matches!(
                UmpMessage::try_from(midi.data_words()),
                Ok(UmpMessage::SystemCommon(_))
            ),
            "timing clock should stay a System Real-Time message"
        );
    }

    #[test]
    fn note_event_round_trips_to_full_width_cv2() {
        // ClapEvent::NoteOn -> MidiEvent builds a native MIDI-2 NoteOn preserving
        // the note and full-width velocity (no 7-bit squash).
        use tutti_midi_types::convert::u16_to_unit_f32;
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        let clap = ClapEvent::note_on(3, 2, 64, 0.75);
        let midi = clap.to_midi().expect("note on -> midi");
        assert_eq!(midi.frame_offset, 3);
        match UmpMessage::try_from(midi.data_words()).expect("decodes") {
            UmpMessage::ChannelVoice2(Cv2::NoteOn(m)) => {
                assert_eq!(u8::from(m.channel()), 2);
                assert_eq!(u8::from(m.note_number()), 64);
                let velocity = u16_to_unit_f32(m.velocity());
                assert!((velocity - 0.75).abs() < 0.01, "velocity {velocity}");
            }
            other => panic!("expected CV2 NoteOn, got {other:?}"),
        }
    }

    #[test]
    fn fill_gestures_captures_gesture_and_mod_events() {
        // H4: gesture begin/end + param-mod on the output side must no longer
        // be silently dropped. They land in the CLAP-private gesture list, and
        // `fill_param_changes` still ignores them (only PARAM_VALUE flows there).
        use clap_sys::events::{
            clap_event_param_gesture, clap_event_param_mod, CLAP_EVENT_PARAM_GESTURE_BEGIN,
            CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD,
        };

        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();

        let begin = clap_event_param_gesture {
            header: clap_event_header {
                size: header_size::<clap_event_param_gesture>(),
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_GESTURE_BEGIN,
                flags: 0,
            },
            param_id: 7,
        };
        let end = clap_event_param_gesture {
            header: clap_event_header {
                size: header_size::<clap_event_param_gesture>(),
                time: 4,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_GESTURE_END,
                flags: 0,
            },
            param_id: 7,
        };
        let modev = clap_event_param_mod {
            header: clap_event_header {
                size: header_size::<clap_event_param_mod>(),
                time: 2,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_MOD,
                flags: 0,
            },
            param_id: 9,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            amount: 0.25,
        };
        // Also push a plain PARAM_VALUE to prove the two paths stay separate.
        let value = ClapEvent::param_value(3, 5, 0.9);

        unsafe {
            let push = (*list_ptr).try_push.unwrap();
            assert!(push(list_ptr, &begin.header as *const _));
            assert!(push(list_ptr, &modev.header as *const _));
            assert!(push(list_ptr, &end.header as *const _));
            assert!(push(list_ptr, value.header() as *const _));
        }

        let mut gestures: Vec<ClapEvent> = Vec::new();
        output.fill_gestures(&mut gestures);
        assert_eq!(gestures.len(), 3, "begin + mod + end captured");
        assert!(gestures
            .iter()
            .any(|e| matches!(e, ClapEvent::ParamGestureBegin(_))));
        assert!(gestures
            .iter()
            .any(|e| matches!(e, ClapEvent::ParamGestureEnd(_))));
        assert!(gestures.iter().any(|e| matches!(e, ClapEvent::ParamMod(_))));

        // fill_param_changes only picks up the PARAM_VALUE, not gestures/mods.
        let mut changes = ParameterChanges::new();
        output.fill_param_changes(&mut changes);
        assert_eq!(changes.queues.len(), 1, "only the PARAM_VALUE queued");
        assert_eq!(
            changes.queues[0].param_id,
            ParamAddress::Opaque(5u32.into())
        );

        // Draining clears the pool.
        output.fill_gestures(&mut gestures);
        assert_eq!(gestures.len(), 3);
        gestures.clear();
        assert!(gestures.is_empty());
    }

    /// A plugin emitting more events than the return pool holds must not grow
    /// it — `try_push` is the *plugin's* callback, so an unbounded pool would
    /// let a plugin provoke a `malloc` inside the audio callback. Events past
    /// the cap are dropped and reported through `overflowed()`.
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
            pool.overflowed(),
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

    #[test]
    fn test_output_events_push_null_event() {
        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();
        unsafe {
            let push_fn = (*list_ptr).try_push.unwrap();
            let result = push_fn(list_ptr, std::ptr::null());
            assert!(!result);
        }
        assert!(output.events().is_empty());
    }

    #[test]
    fn test_output_events_push_null_list() {
        let event = ClapEvent::note_on(0, 0, 60, 0.8);
        let header = event.header();
        unsafe {
            let result =
                output_events_try_push(std::ptr::null(), header as *const clap_event_header);
            assert!(!result);
        }
    }

    #[test]
    fn test_output_events_push_unknown_event_type() {
        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();

        let header = clap_event_header {
            size: header_size::<clap_event_header>(),
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: 9999,
            flags: 0,
        };
        unsafe {
            let push_fn = (*list_ptr).try_push.unwrap();
            let result = push_fn(list_ptr, &header as *const clap_event_header);
            assert!(!result);
        }
        assert!(output.events().is_empty());
    }

    #[test]
    fn test_output_events_push_multiple() {
        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();

        for i in 0..5 {
            let event = ClapEvent::note_on(i, 0, 60 + i as i16, 0.5);
            let header = event.header();
            unsafe {
                let push_fn = (*list_ptr).try_push.unwrap();
                let result = push_fn(list_ptr, header as *const clap_event_header);
                assert!(result);
            }
        }

        assert_eq!(output.events().len(), 5);
    }

    #[test]
    fn test_output_events_push_sysex_with_data() {
        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();

        let sysex_data: Vec<u8> = vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7];
        let sysex = clap_event_midi_sysex {
            header: clap_event_header {
                size: header_size::<clap_event_midi_sysex>(),
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_MIDI_SYSEX,
                flags: 0,
            },
            port_index: 0,
            buffer: sysex_data.as_ptr(),
            size: sysex_data.len() as u32,
        };

        unsafe {
            let push_fn = (*list_ptr).try_push.unwrap();
            let result = push_fn(
                list_ptr,
                &sysex as *const clap_event_midi_sysex as *const clap_event_header,
            );
            assert!(result);
        }
        assert_eq!(output.events().len(), 1);
        match &output.events()[0] {
            ClapEvent::MidiSysex { _data, .. } => {
                assert_eq!(_data, &sysex_data);
            }
            _ => panic!("Expected MidiSysex event"),
        }
    }

    #[test]
    fn test_output_events_push_sysex_null_buffer() {
        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();

        let sysex = clap_event_midi_sysex {
            header: clap_event_header {
                size: header_size::<clap_event_midi_sysex>(),
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_MIDI_SYSEX,
                flags: 0,
            },
            port_index: 0,
            buffer: std::ptr::null(),
            size: 0,
        };

        unsafe {
            let push_fn = (*list_ptr).try_push.unwrap();
            let result = push_fn(
                list_ptr,
                &sysex as *const clap_event_midi_sysex as *const clap_event_header,
            );
            // try_push accepts the event but a null sysex buffer yields no stored event.
            assert!(result);
        }
        assert!(output.events().is_empty());
    }
}
