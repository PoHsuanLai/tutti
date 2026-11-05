//! note expression: part of the VST3 event vocabulary.

use super::*;

/// Per-note expression value. Specific to a note id and expression type
/// rather than a channel.
#[derive(Debug, Clone, Copy)]
pub struct NoteExpressionValueEvent {
    /// Bus, frame offset and event-type discriminant.
    pub header: EventHeader,
    /// Note id assigned by the originating note-on; expression is per-note, not
    /// per-channel, so this is the only addressing available.
    pub note_id: i32,
    /// VST3 `NoteExpressionTypeIDs`: 0=volume, 1=pan, 2=tuning, 3=vibrato,
    /// 4=expression, 5=brightness. See [`note_expression_type_to_id`].
    pub type_id: u32,
    /// 0.0 to 1.0, meaning depends on type_id.
    pub value: f64,
}

/// Per-note text annotation (e.g. lyric, ornament name). `text` indexes the
/// owning event list's arena; `type_id` is a VST3 note-expression type id.
#[derive(Debug, Clone, Copy)]
pub struct NoteExpressionTextEvent {
    /// Bus, frame offset and event-type discriminant.
    pub header: EventHeader,
    /// VST3 note-expression type id the text annotates.
    pub type_id: u32,
    /// Note id assigned by the originating note-on.
    pub note_id: i32,
    /// The annotation, as a slice into the event list's UTF-16 arena.
    pub text: TextRef,
}

/// Integer-valued per-note expression, the `i64` counterpart to
/// [`NoteExpressionValueEvent`] (used for stepped / enumerated dimensions).
#[derive(Debug, Clone, Copy)]
pub struct NoteExpressionIntValueEvent {
    /// Bus, frame offset and event-type discriminant.
    pub header: EventHeader,
    /// VST3 note-expression type id; its declared info states the valid range.
    pub type_id: u32,
    /// Note id assigned by the originating note-on.
    pub note_id: i32,
    /// The stepped value, in the type's own units rather than normalized.
    pub value: i64,
}

/// Encodes a [`NoteExpressionType`] as the integer `typeId` VST3 uses on the
/// wire.
///
/// The ids are the `NoteExpressionTypeIDs` enumerators from
/// `ivstnoteexpression.h`: `kVolumeTypeID = 0`, `kPanTypeID = 1`,
/// `kTuningTypeID = 2`, `kVibratoTypeID = 3`, `kExpressionTypeID = 4`,
/// `kBrightnessTypeID = 5`. Note that **Expression comes before Brightness** —
/// they are not in enum-declaration order here.
///
/// **Partial:** VST3 has no standard note-expression `typeId` for CLAP's
/// [`Pressure`](NoteExpressionType::Pressure) (poly aftertouch rides the
/// `kPolyPressureEvent` path instead), so that returns `None`. Callers must
/// handle the `None` (skip the event) rather than substitute a different
/// dimension.
pub fn note_expression_type_to_id(ty: NoteExpressionType) -> Option<u32> {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;
    #[allow(
        clippy::unnecessary_cast,
        reason = "`NoteExpressionTypeIDs_` is u32 on unix and c_int on Windows; \
                  each cast is a no-op on one target and load-bearing on the other"
    )]
    match ty {
        NoteExpressionType::Volume => Some(Ids::kVolumeTypeID as u32),
        NoteExpressionType::Pan => Some(Ids::kPanTypeID as u32),
        NoteExpressionType::Tuning => Some(Ids::kTuningTypeID as u32),
        NoteExpressionType::Vibrato => Some(Ids::kVibratoTypeID as u32),
        NoteExpressionType::Expression => Some(Ids::kExpressionTypeID as u32),
        NoteExpressionType::Brightness => Some(Ids::kBrightnessTypeID as u32),
        NoteExpressionType::Pressure => None,
        // A custom id came from a VST3 plugin in the first place, so it goes
        // back out unchanged. Text and phoneme ids are screened here rather
        // than trusted: those slots carry a string, and emitting one as a
        // value event would hand the plugin a malformed event.
        NoteExpressionType::Custom(id) => (!is_text_type_id(id)).then_some(id),
    }
}

/// Whether `id` is one of the two VST3 note-expression slots that carry text
/// rather than a `f64` value.
///
/// `kTextTypeID` / `kPhonemeTypeID` ride `NoteExpressionTextEvent`, a different
/// event struct. They must not round-trip through the value path in either
/// direction.
pub(super) fn is_text_type_id(id: u32) -> bool {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;
    #[allow(
        clippy::unnecessary_cast,
        reason = "`NoteExpressionTypeIDs_` is u32 on unix and c_int on Windows; \
                  each cast is a no-op on one target and load-bearing on the other"
    )]
    {
        id == Ids::kTextTypeID as u32 || id == Ids::kPhonemeTypeID as u32
    }
}

/// Decodes a VST3 `typeId` into a [`NoteExpressionType`].
///
/// The six standard value dimensions map to their named variants; anything else
/// becomes [`Custom`](NoteExpressionType::Custom) carrying the id, because VST3
/// reserves `kCustomStart` upward for plugin-defined dimensions declared through
/// `INoteExpressionController`. Returning `None` for those would have the
/// caller drop them, making a plugin's own expression dimensions invisible.
///
/// Still `None` for `kTextTypeID` / `kPhonemeTypeID`: those slots carry a string
/// on `NoteExpressionTextEvent`, so there is no `f64` value to decode and
/// admitting them here would invent one.
///
/// `Pressure` is never produced — VST3 has no id for it.
pub fn note_expression_type_from_id(id: u32) -> Option<NoteExpressionType> {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;
    #[allow(
        clippy::unnecessary_cast,
        reason = "`NoteExpressionTypeIDs_` is u32 on unix and c_int on Windows; \
                  each cast is a no-op on one target and load-bearing on the other"
    )]
    match id {
        i if i == Ids::kVolumeTypeID as u32 => Some(NoteExpressionType::Volume),
        i if i == Ids::kPanTypeID as u32 => Some(NoteExpressionType::Pan),
        i if i == Ids::kTuningTypeID as u32 => Some(NoteExpressionType::Tuning),
        i if i == Ids::kVibratoTypeID as u32 => Some(NoteExpressionType::Vibrato),
        i if i == Ids::kExpressionTypeID as u32 => Some(NoteExpressionType::Expression),
        i if i == Ids::kBrightnessTypeID as u32 => Some(NoteExpressionType::Brightness),
        i if is_text_type_id(i) => None,
        other => Some(NoteExpressionType::Custom(other)),
    }
}

/// Stage a [`NoteExpressionValue`] into the tagged-enum [`Vst3Event`] form
/// accepted by the event-list code. Returns `None` for a dimension VST3
/// cannot encode (`Pressure`) — see [`note_expression_type_to_id`];
/// the caller drops the event rather than substituting a different dimension.
///
/// # Range
///
/// `ivstnoteexpression.h:89` states expression events are *"always absolute
/// normalized values [0.0, 1.0]"*, and addresses the **host** — so bringing a
/// value into range is this function's job, not the plugin's.
///
/// The two ways to be out of range are not the same fact and are not handled
/// the same way:
///
/// - **Non-finite** — dropped, via [`is_usable`]. A NaN has no nearest legal
///   value, so clamping it would have to invent one; `f64::clamp` also panics
///   on a NaN bound and returns NaN for a NaN input, so it is not a guard here.
/// - **Finite but outside `[0, 1]`** — clamped. `1.2` has one unambiguous
///   legal answer, and dropping it would silently freeze the dimension at
///   whatever the plugin last saw.
///
/// The MIDI paths in this crate reach here through `u32_to_unit_f32`, in range
/// by construction, and through the tuning map, in range unless a sender sets
/// a per-note bend range past 120 semitones (a bend past VST3's ±120 is then
/// clamped to it). The main exposure is
/// [`Vst3InputEvents::note_expressions`], an arbitrary caller-supplied slice.
pub fn note_expression_to_vst3(value: &NoteExpressionValue) -> Option<Vst3Event> {
    let type_id = note_expression_type_to_id(value.expression_type)?;
    if !is_usable(value.value) {
        return None;
    }

    let header = EventHeader {
        bus_index: 0,
        sample_offset: value.sample_offset,
        ppq_position: 0.0,
        flags: 0,
        event_type: K_NOTE_EXPRESSION_VALUE_EVENT,
    };

    Some(Vst3Event::NoteExpression(NoteExpressionValueEvent {
        header,
        note_id: value.note_id,
        type_id,
        value: value.value.clamp(0.0, 1.0),
    }))
}

/// Extracts a [`NoteExpressionValue`] from a [`Vst3Event`], or `None` for any
/// non-expression variant or unrecognised `type_id`.
pub fn vst3_to_note_expression(event: &Vst3Event) -> Option<NoteExpressionValue> {
    match event {
        Vst3Event::NoteExpression(e) => {
            let expression_type = note_expression_type_from_id(e.type_id)?;
            Some(NoteExpressionValue {
                sample_offset: e.header.sample_offset,
                note_id: e.note_id,
                expression_type,
                value: e.value,
            })
        }
        _ => None,
    }
}

/// Host-facing per-note text annotation. `text` is UTF-16 (converted from the
/// IPC `String` at this boundary); staging interns it into the event arena.
#[derive(Debug, Clone)]
pub struct NoteExpressionText {
    /// Frame offset within the block the event is staged into.
    pub sample_offset: i32,
    /// Note id assigned by the originating note-on.
    pub note_id: i32,
    /// VST3 note-expression type id the text annotates.
    pub type_id: u32,
    /// The annotation as UTF-16 code units, owned until staging interns it.
    pub text: Vec<u16>,
}

impl NoteExpressionText {
    /// Stage into a [`Vst3Event`], interning the text into `arena`.
    pub(crate) fn to_vst3_event(&self, arena: &mut TextArena) -> Vst3Event {
        Vst3Event::NoteExpressionText(NoteExpressionTextEvent {
            header: text_header(self.sample_offset, K_NOTE_EXPRESSION_TEXT_EVENT),
            type_id: self.type_id,
            note_id: self.note_id,
            text: intern_utf16(arena, &self.text),
        })
    }
}

/// Host-facing integer-valued per-note expression (see
/// [`NoteExpressionIntValueEvent`]). No text, so no arena needed.
#[derive(Debug, Clone, Copy)]
pub struct NoteExpressionIntValue {
    /// Frame offset within the block the event is staged into.
    pub sample_offset: i32,
    /// Note id assigned by the originating note-on.
    pub note_id: i32,
    /// VST3 note-expression type id; its declared info states the valid range.
    pub type_id: u32,
    /// The stepped value, in the type's own units rather than normalized.
    pub value: i64,
}

impl NoteExpressionIntValue {
    /// Stage into a [`Vst3Event`]. No arena is needed — the payload is an `i64`.
    #[must_use]
    pub fn to_vst3_event(&self) -> Vst3Event {
        Vst3Event::NoteExpressionInt(NoteExpressionIntValueEvent {
            header: EventHeader {
                bus_index: 0,
                sample_offset: self.sample_offset,
                ppq_position: 0.0,
                flags: 0,
                event_type: K_NOTE_EXPRESSION_INT_VALUE_EVENT,
            },
            type_id: self.type_id,
            note_id: self.note_id,
            value: self.value,
        })
    }
}

/// See [`vst3_to_chord`].
pub fn vst3_to_note_expression_text(
    event: &Vst3Event,
    arena: &[u16],
) -> Option<NoteExpressionText> {
    match event {
        Vst3Event::NoteExpressionText(e) => Some(NoteExpressionText {
            sample_offset: e.header.sample_offset,
            note_id: e.note_id,
            type_id: e.type_id,
            text: resolve_text(&e.text, arena),
        }),
        _ => None,
    }
}

/// Reads the integer per-note expression off an event, or `None` otherwise.
pub fn vst3_to_note_expression_int(event: &Vst3Event) -> Option<NoteExpressionIntValue> {
    match event {
        Vst3Event::NoteExpressionInt(e) => Some(NoteExpressionIntValue {
            sample_offset: e.header.sample_offset,
            note_id: e.note_id,
            type_id: e.type_id,
            value: e.value,
        }),
        _ => None,
    }
}
