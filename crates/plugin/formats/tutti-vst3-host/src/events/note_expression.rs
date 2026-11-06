//! Note-expression events and their type-id mapping.

use tutti_plugin_types::{NoteExpressionType, NoteExpressionValue, is_usable};
use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;

use super::{Event, ExpressionInt, ExpressionValue, NoteExpressionIntValue, Payload, ToEvent};

const VOLUME: u32 = Ids::kVolumeTypeID;
const PAN: u32 = Ids::kPanTypeID;
const TUNING: u32 = Ids::kTuningTypeID;
const VIBRATO: u32 = Ids::kVibratoTypeID;
const EXPRESSION: u32 = Ids::kExpressionTypeID;
const BRIGHTNESS: u32 = Ids::kBrightnessTypeID;
const TEXT: u32 = Ids::kTextTypeID;
const PHONEME: u32 = Ids::kPhonemeTypeID;

/// The `typeId` VST3 uses on the wire for `ty` (`ivstnoteexpression.h`:
/// volume 0, pan 1, tuning 2, vibrato 3, expression 4, brightness 5).
///
/// `None` for `Pressure`, which VST3 sends as `kPolyPressureEvent` instead, and
/// for a custom id in one of the two text slots: those carry a string on
/// `NoteExpressionTextEvent`, and a value event there would be malformed. A
/// custom id otherwise goes back out unchanged, as it came from a plugin.
pub fn expression_type_id(ty: NoteExpressionType) -> Option<u32> {
    match ty {
        NoteExpressionType::Volume => Some(VOLUME),
        NoteExpressionType::Pan => Some(PAN),
        NoteExpressionType::Tuning => Some(TUNING),
        NoteExpressionType::Vibrato => Some(VIBRATO),
        NoteExpressionType::Expression => Some(EXPRESSION),
        NoteExpressionType::Brightness => Some(BRIGHTNESS),
        NoteExpressionType::Pressure => None,
        NoteExpressionType::Custom(id) => (!is_text_type_id(id)).then_some(id),
    }
}

/// Whether `id` is a slot that carries text, not an `f64` value.
fn is_text_type_id(id: u32) -> bool {
    id == TEXT || id == PHONEME
}

/// The [`NoteExpressionType`] for a VST3 `typeId`.
///
/// The six standard dimensions have named variants; anything else, `kCustomStart`
/// upward, is [`Custom`](NoteExpressionType::Custom) so a plugin's own dimensions
/// stay visible. `None` for the text slots, which have no `f64` value, and
/// `Pressure` is never produced.
pub fn expression_type_from_id(id: u32) -> Option<NoteExpressionType> {
    match id {
        VOLUME => Some(NoteExpressionType::Volume),
        PAN => Some(NoteExpressionType::Pan),
        TUNING => Some(NoteExpressionType::Tuning),
        VIBRATO => Some(NoteExpressionType::Vibrato),
        EXPRESSION => Some(NoteExpressionType::Expression),
        BRIGHTNESS => Some(NoteExpressionType::Brightness),
        TEXT | PHONEME => None,
        other => Some(NoteExpressionType::Custom(other)),
    }
}

impl ToEvent for NoteExpressionValue {
    /// `None` for a dimension VST3 cannot encode ([`expression_type_id`]) or a
    /// non-finite value. The SDK says expression values are "always absolute
    /// normalized [0.0, 1.0]" and addresses the host, so a finite value outside
    /// the range is clamped here; a NaN has no nearest legal value, and
    /// `f64::clamp` passes it through, so it is dropped ([`is_usable`]).
    fn to_event(&self) -> Option<Event> {
        let type_id = expression_type_id(self.expression_type)?;
        if !is_usable(self.value) {
            return None;
        }
        Some(Event::at(
            self.sample_offset,
            Payload::ExpressionValue(ExpressionValue::new(
                type_id,
                self.note_id,
                self.value.clamp(0.0, 1.0),
            )),
        ))
    }
}

impl ToEvent for NoteExpressionIntValue {
    fn to_event(&self) -> Option<Event> {
        Some(Event::at(
            self.sample_offset,
            Payload::ExpressionInt(ExpressionInt::new(
                self.type_id,
                self.note_id,
                self.value.cast_unsigned(),
            )),
        ))
    }
}

pub(super) fn decode(event: &Event) -> Option<NoteExpressionValue> {
    let Payload::ExpressionValue(e) = &event.payload else {
        return None;
    };
    Some(NoteExpressionValue {
        sample_offset: event.sample_offset(),
        note_id: e.noteId,
        expression_type: expression_type_from_id(e.typeId)?,
        value: e.value,
    })
}

pub(super) fn decode_int(event: &Event) -> Option<NoteExpressionIntValue> {
    let Payload::ExpressionInt(e) = &event.payload else {
        return None;
    };
    Some(NoteExpressionIntValue {
        sample_offset: event.sample_offset(),
        note_id: e.noteId,
        type_id: e.typeId,
        // MIDI-2's 64 bits in VST3's unsigned slot: the same bits, read signed.
        value: e.value.cast_signed(),
    })
}
