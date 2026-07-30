//! Parameter↔MIDI mapping: the CC→parameter routing table
//! (`kAudioUnitProperty_AllParameterMIDIMappings` and its three siblings).
//!
//! This is AU's answer to VST3's `IMidiMapping`, and the stakes are the same as
//! that module states: mod wheel, breath, expression and sustain reach many
//! instruments *only* through the mapping surface. Where VST3 has the host
//! *query* a fixed channel × controller grid, AU inverts it — the **host writes
//! the table into the plugin**, and the plugin does the routing internally
//! during render. So the host does not need a per-block routing pass here at
//! all; it needs to be able to read, add, remove and replace the AU's mapping
//! table, and to drive "learn" mode.
//!
//! That inversion is why there is no `route()` in this module and no audio-path
//! code: once a mapping is installed, the CC arrives through the ordinary
//! [`send_midi`](crate::instance::AuInstance::send_midi) path and the AU moves
//! its own parameter. Verified end-to-end on macOS 15.6 — see
//! [`AuInstance::add_parameter_midi_mapping`](crate::instance::AuInstance::add_parameter_midi_mapping).
//!
//! # Honest status on this machine
//!
//! Unlike `MIDIOutputCallbackInfo` (0 of 59 components) and `HostCallbacks`
//! (accepted by ~35, called by none), **this family is genuinely implemented**,
//! by exactly two units. Measured on macOS 15.6 across all 59 registered
//! components (every Apple unit plus TDR Nova, TAL-NoiseMaker, TAL-Reverb-4), at
//! global/input/output scope, before and after `AudioUnitInitialize`:
//!
//! | unit | property 41 |
//! |---|---|
//! | AUSampler (`aumu`/`samp`/`appl`) | read/write, works |
//! | AUMIDISynth (`aumu`/`msyn`/`appl`) | read/write, works |
//! | all 57 others (incl. TAL-NoiseMaker, TDR Nova) | `kAudioUnitErr_InvalidProperty` (-10879) |
//!
//! The pattern holds: a synth implements it, effects do not. Apple's header says
//! the properties "normally apply only to … instrument units ('aumu') and music
//! effects ('aumf')", and that is what the machine shows.
//!
//! The full round trip is real, not merely accepted. On AUSampler: adding a
//! mapping of CC 20 → parameter 900 (`Gain`, range `-96..=12` dB), then sending
//! CC 20 = 127 and rendering one block, moved the parameter from `0` to `12`.
//! That is the whole point of the feature and it works.
//!
//! # Property 17 (`kAudioUnitProperty_MIDIControlMapping`) is deliberately absent
//!
//! The deprecated read-only form is **not** supported, and not as an oversight.
//! Apple deprecated it in macOS 10.2 in favour of this family, and the measured
//! answer is unambiguous: **0 of 59 components answer property 17**, at any of
//! the three scopes, in either lifecycle phase. Shipping a fallback would mean
//! shipping a decoder for `AudioUnitMIDIControlMapping` that no installed unit
//! can exercise — untestable against reality, and exactly the "vacuous passing
//! test" this crate refuses. If a unit that answers only 17 ever turns up, the
//! sweep in `tests/au_midi_map.rs` will report it as a *hit* and the fallback can
//! be written then, against a real implementer.
//!
//! # Four AU deviations from the header, each of which shapes the API
//!
//! Every one of these was measured, and each is the reason a nearby method is
//! shaped the way it is rather than the obvious way:
//!
//! 1. **The table cannot be cleared by writing an empty one.** The header
//!    implies a set replaces the table, so `set(&[])` reads as "clear". It does
//!    not: a NULL/0-size write answers `kAudioUnitErr_InvalidPropertyValue`
//!    (-10851) and a non-NULL/0-length write answers `paramErr` (-50). Both
//!    leave the table **unchanged**. So [`set_all`] with an empty slice routes
//!    through *remove* instead — see its docs.
//! 2. **`HotMap` reads `noErr` when nothing is armed.** The header says an AU
//!    "should return a `kAudioUnitErr_InvalidPropertyValue` error when the host
//!    tries to read this property's value" while unarmed. Both implementers
//!    instead return `noErr` with an all-zero struct. A host cannot use the
//!    status to tell "no mapping pending" from "mapping complete", which is why
//!    [`hot_map_pending`] decides on `status == 0` rather than on the `Result`.
//! 3. **Nothing is validated.** A mapping naming parameter id `99999` on a unit
//!    with ~10 parameters is accepted with `noErr` *and appears in the read-back
//!    table*; so is `scope: 99`. This is the crate's twice-paid trap — a `noErr`
//!    proves nothing — so the tests here verify by reading back, and callers
//!    should not treat a successful add as proof the mapping is meaningful.
//! 4. **The table comes back reordered.** AUSampler does not return mappings in
//!    insertion order. Any comparison must be order-insensitive; the tests use
//!    set containment, not index equality.

#![cfg(target_os = "macos")]

use std::mem::size_of;
use std::os::raw::c_void;

use crate::error::Result;
use crate::ffi::{check, get_property_bytes, property_size};
use crate::types::*;

/// Apple's `AUParameterMIDIMapping` from `AudioUnitProperties.h`.
///
/// Hand-declared for the same reason `midi_out.rs`'s callback struct is:
/// `coreaudio-sys` does not export it (nor any of the four property ids, nor the
/// six flag bits — all of them are spelled out in [`crate::types`]). The layout
/// is pinned by [`tests::the_mapping_struct_matches_the_c_abi`] against values
/// read out of a C program compiled against the real header: **32 bytes, align
/// 4**, with `mStatus` at offset 24 and `reserved3` at 28. A wrong guess here
/// would not fail loudly — the AU would read the host's `mStatus` out of the
/// padding and silently map the wrong MIDI message.
///
/// The three `reserved*` fields are Apple's, and the header says outright they
/// "MUST be set to zero". [`AuMidiMapping::to_raw`] is the only constructor and
/// always zeroes them, which is why they are private.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct AuParameterMidiMappingRaw {
    scope: u32,
    element: u32,
    parameter_id: u32,
    flags: u32,
    sub_range_min: f32,
    sub_range_max: f32,
    status: u8,
    data1: u8,
    reserved1: u8,
    reserved2: u8,
    reserved3: u32,
}

/// What MIDI message a mapping triggers on, decoded from `mStatus` / `mData1`.
///
/// A **closed** enum, for the reason `parameters::DisplayCurve` is closed: the
/// MIDI 1.0 channel-voice status nibbles are a fixed structural set, not an
/// extensible catalog. Apple's header tabulates exactly these seven commands and
/// says what `mData1` means for each — which is the real reason this is a typed
/// enum rather than a `(u8, u8)` pair. `mData1` is a controller id for
/// `ControlChange`, a *note number* for the three note commands, a patch number
/// for `ProgramChange`, and **unused** for the two pressure/bend forms. A host
/// that read `data1` without knowing which of those it held would happily show a
/// pitch-bend mapping as "CC 0".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MidiTrigger {
    /// `0x8n` — note off. `mData1` is the note number.
    NoteOff {
        /// Note number, or ignored when [`AuMidiMapping::any_note`] is set.
        note: u8,
    },
    /// `0x9n` — note on. `mData1` is the note number.
    ///
    /// The header notes a note message can drive a continuous parameter, using
    /// the note number itself as the value ("a note message could be used to set
    /// the cut off frequency of a filter").
    NoteOn {
        /// Note number, or ignored when [`AuMidiMapping::any_note`] is set.
        note: u8,
    },
    /// `0xAn` — polyphonic key pressure. `mData1` is the note number.
    KeyPressure {
        /// Note number, or ignored when [`AuMidiMapping::any_note`] is set.
        note: u8,
    },
    /// `0xBn` — control change. `mData1` is the controller id (CC 1 = mod wheel,
    /// CC 2 = breath, CC 11 = expression, CC 64 = sustain).
    ///
    /// This is the arm that carries the module's whole reason for existing.
    ControlChange {
        /// Controller number, 0..=127.
        controller: u8,
    },
    /// `0xCn` — program change. `mData1` is the patch number.
    ProgramChange {
        /// Patch number.
        patch: u8,
    },
    /// `0xDn` — channel pressure (aftertouch). `mData1` is **unused**; the
    /// header marks it "0 (Unused)", and this variant carries no payload so a
    /// caller cannot read a meaningless byte out of it.
    ChannelPressure,
    /// `0xEn` — pitch bend. `mData1` is **unused**, as for
    /// [`ChannelPressure`](Self::ChannelPressure).
    PitchBend,
    /// A status byte whose high nibble is not one of the seven above — a system
    /// message (`0xF_`), or a byte with the high bit clear, which is not a status
    /// byte at all.
    ///
    /// Present because this is decoded from a value an **AU** supplied, not one
    /// the host built: a unit that stores garbage in `mStatus` (and one of them
    /// stores an out-of-range `mScope` verbatim, so this is not paranoia) must
    /// not make [`AuMidiMapping::from_raw`] panic or silently claim "CC 0".
    Other {
        /// The raw status byte, high nibble and channel both.
        status: u8,
        /// The raw `mData1`, uninterpreted.
        data1: u8,
    },
}

impl MidiTrigger {
    /// The status high nibble this trigger occupies (`0x80`, `0x90`, …).
    fn status_nibble(&self) -> u8 {
        match self {
            Self::NoteOff { .. } => 0x80,
            Self::NoteOn { .. } => 0x90,
            Self::KeyPressure { .. } => 0xA0,
            Self::ControlChange { .. } => 0xB0,
            Self::ProgramChange { .. } => 0xC0,
            Self::ChannelPressure => 0xD0,
            Self::PitchBend => 0xE0,
            Self::Other { status, .. } => *status & 0xF0,
        }
    }

    /// The `mData1` byte this trigger carries, `0` for the two forms whose
    /// `mData1` the header marks unused.
    fn data1(&self) -> u8 {
        match self {
            Self::NoteOff { note } | Self::NoteOn { note } | Self::KeyPressure { note } => *note,
            Self::ControlChange { controller } => *controller,
            Self::ProgramChange { patch } => *patch,
            Self::ChannelPressure | Self::PitchBend => 0,
            Self::Other { data1, .. } => *data1,
        }
    }

    /// Decode a raw `(mStatus, mData1)` pair.
    fn from_raw(status: u8, data1: u8) -> Self {
        match status & 0xF0 {
            0x80 => Self::NoteOff { note: data1 },
            0x90 => Self::NoteOn { note: data1 },
            0xA0 => Self::KeyPressure { note: data1 },
            0xB0 => Self::ControlChange { controller: data1 },
            0xC0 => Self::ProgramChange { patch: data1 },
            0xD0 => Self::ChannelPressure,
            0xE0 => Self::PitchBend,
            _ => Self::Other { status, data1 },
        }
    }

    /// Whether the note-number fields are meaningful for this trigger — i.e.
    /// whether [`kAUParameterMIDIMapping_AnyNoteFlag`](K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE)
    /// can apply. The header restricts that flag to note on / note off /
    /// polyphonic pressure.
    pub fn is_note_command(&self) -> bool {
        matches!(
            self,
            Self::NoteOff { .. } | Self::NoteOn { .. } | Self::KeyPressure { .. }
        )
    }
}

/// One parameter↔MIDI mapping: which MIDI message drives which parameter, and
/// how the controller's value is interpreted on the way.
///
/// The flags word is modelled as **five named fields, not one `u32` and not one
/// `bool`**, because the bits are not variations on "enabled" — three of them
/// change what an incoming controller value *means*:
///
/// * [`sub_range`](Self::sub_range) confines the parameter to a slice of its own
///   range, so CC 127 no longer means "maximum".
/// * [`toggle`](Self::toggle) discards the value entirely and *flips* the
///   parameter.
/// * [`bipolar`](Self::bipolar) collapses the value to two states with a
///   threshold at 64/65 (the header's sustain-pedal example), and
///   [`bipolar_on`](Self::bipolar_on) then decides which end is "on".
///
/// A host that stored the flags as an opaque `u32` could not draw the difference
/// between a continuous knob and a sustain pedal, and one that stored a `bool`
/// could not represent either.
///
/// The two "any" flags widen what matches rather than reinterpreting the value:
/// [`any_channel`](Self::any_channel) ignores the MIDI channel in the status
/// byte, [`any_note`](Self::any_note) ignores the note number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AuMidiMapping {
    /// The parameter's scope — `kAudioUnitScope_Global` in every mapping
    /// measured. Apple's header requires `mScope`, `mElement` and
    /// `mParameterID` to be "correctly specified" in all usages, and the
    /// remove path identifies a mapping by exactly these three.
    pub scope: u32,
    /// The parameter's element within `scope`.
    pub element: u32,
    /// The `AudioUnitParameterID` this mapping drives.
    ///
    /// Not validated by the AU: an id no parameter uses is accepted with
    /// `noErr` and stored (measured — id `99999` on a ~10-parameter unit). A
    /// caller should cross-check against
    /// [`get_parameter_list`](crate::instance::AuInstance::get_parameter_list).
    pub parameter_id: u32,
    /// The MIDI message that triggers this mapping.
    pub trigger: MidiTrigger,
    /// MIDI channel, `0..=15`, taken from the low nibble of `mStatus`.
    ///
    /// Meaningless when [`any_channel`](Self::any_channel) is set — the AU
    /// ignores it then — but still preserved on the round trip rather than
    /// zeroed, because the AU preserves it and a lossy read would make
    /// "what did I write" untestable.
    pub channel: u8,
    /// `kAUParameterMIDIMapping_AnyChannelFlag`: match this message on **any**
    /// channel, ignoring [`channel`](Self::channel).
    pub any_channel: bool,
    /// `kAUParameterMIDIMapping_AnyNoteFlag`: for a note command, match **any**
    /// note number. Only meaningful when
    /// [`MidiTrigger::is_note_command`] holds.
    pub any_note: bool,
    /// `kAUParameterMIDIMapping_SubRange`: confine the controller to
    /// `[min, max]` of the parameter's range instead of its full span.
    ///
    /// `None` means the flag is clear and the whole range is used. `Some` also
    /// carries the two `AudioUnitParameterValue` fields, which are in the
    /// **parameter's own units** — not normalized — so a dB gain's sub-range is
    /// written in dB.
    pub sub_range: Option<(f32, f32)>,
    /// `kAUParameterMIDIMapping_Toggle`: the mapped message *flips* the
    /// parameter rather than setting it from the controller value. For boolean
    /// parameters.
    pub toggle: bool,
    /// `kAUParameterMIDIMapping_Bipolar`: the parameter takes only two states,
    /// with the controller thresholded — the header's example is a sustain
    /// pedal, where 0..64 is "off" and 65..127 is "on".
    pub bipolar: bool,
    /// `kAUParameterMIDIMapping_Bipolar_On`: which end of the controller maps to
    /// the parameter's "on" state. The header says this is "only valid if
    /// `kAUParameterMIDIMapping_Bipolar` is set", so it is meaningless — and
    /// ignored by the AU — with [`bipolar`](Self::bipolar) clear.
    pub bipolar_on: bool,
}

impl AuMidiMapping {
    /// The common case: a control-change message on one channel driving a
    /// parameter's full range.
    ///
    /// This is the shape a "map mod wheel to filter cutoff" gesture produces —
    /// global scope, element 0, no flags. Reach for the struct literal for
    /// anything needing sub-range, toggle or bipolar semantics.
    pub fn control_change(parameter_id: u32, channel: u8, controller: u8) -> Self {
        Self {
            scope: K_AUDIO_UNIT_SCOPE_GLOBAL,
            element: 0,
            parameter_id,
            trigger: MidiTrigger::ControlChange { controller },
            channel,
            any_channel: false,
            any_note: false,
            sub_range: None,
            toggle: false,
            bipolar: false,
            bipolar_on: false,
        }
    }

    /// The same, matching the controller on **every** channel.
    ///
    /// Separate constructor rather than a `channel: Option<u8>` argument because
    /// the AU still stores a channel nibble alongside the any-channel flag, and
    /// a caller passing `None` would have no way to know which of the two it had
    /// set. Sets [`channel`](Self::channel) to 0, which the AU ignores.
    pub fn control_change_any_channel(parameter_id: u32, controller: u8) -> Self {
        Self {
            any_channel: true,
            ..Self::control_change(parameter_id, 0, controller)
        }
    }

    /// Decode a mapping the AU handed back.
    ///
    /// Total — every bit pattern decodes. An unrecognised status byte becomes
    /// [`MidiTrigger::Other`] rather than a panic or a false `ControlChange`,
    /// and an out-of-range `scope` is preserved verbatim (one implementer stores
    /// `scope: 99` when given it, so this is a real shape).
    pub(crate) fn from_raw(raw: &AuParameterMidiMappingRaw) -> Self {
        let sub_range = (raw.flags & K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE != 0)
            .then_some((raw.sub_range_min, raw.sub_range_max));
        Self {
            scope: raw.scope,
            element: raw.element,
            parameter_id: raw.parameter_id,
            trigger: MidiTrigger::from_raw(raw.status, raw.data1),
            channel: raw.status & 0x0F,
            any_channel: raw.flags & K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL != 0,
            any_note: raw.flags & K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE != 0,
            sub_range,
            toggle: raw.flags & K_AU_PARAMETER_MIDI_MAPPING_TOGGLE != 0,
            bipolar: raw.flags & K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR != 0,
            bipolar_on: raw.flags & K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR_ON != 0,
        }
    }

    /// Encode for the AU. The `reserved*` fields are zeroed, as the header
    /// demands; the sub-range values are written only when
    /// [`sub_range`](Self::sub_range) is `Some`, so a `None` mapping cannot
    /// leave stale bounds behind.
    pub(crate) fn to_raw(self) -> AuParameterMidiMappingRaw {
        let mut flags = 0u32;
        if self.any_channel {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL;
        }
        if self.any_note {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE;
        }
        if self.sub_range.is_some() {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE;
        }
        if self.toggle {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_TOGGLE;
        }
        if self.bipolar {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR;
        }
        if self.bipolar_on {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR_ON;
        }
        let (lo, hi) = self.sub_range.unwrap_or((0.0, 0.0));
        AuParameterMidiMappingRaw {
            scope: self.scope,
            element: self.element,
            parameter_id: self.parameter_id,
            flags,
            sub_range_min: lo,
            sub_range_max: hi,
            // The channel lives in the status byte's low nibble, masked so a
            // caller passing 16..=255 cannot corrupt the command nibble above it.
            status: self.trigger.status_nibble() | (self.channel & 0x0F),
            data1: self.trigger.data1(),
            reserved1: 0,
            reserved2: 0,
            reserved3: 0,
        }
    }

    /// True when this mapping names the same `(scope, element, parameter_id)` as
    /// `other`.
    ///
    /// This triple — not the whole struct — is what the AU matches on for both
    /// remove ("the Scope/Element/ParameterID is used to find the mapping to
    /// remove") and replace ("there can be only one mapping per parameter").
    /// Verified: adding CC 10 then CC 11 for the same parameter leaves **one**
    /// mapping, carrying CC 11.
    pub fn targets_same_parameter(&self, other: &Self) -> bool {
        self.scope == other.scope
            && self.element == other.element
            && self.parameter_id == other.parameter_id
    }
}

/// Read the AU's whole mapping table.
///
/// Returns an empty `Vec` when the AU has no mappings installed, which is what a
/// fresh AUSampler reports (`noErr`, size 0) — distinct from *not implementing
/// the property*, which is an error.
///
/// # Errors
/// [`AuError::OsStatus`](crate::error::AuError::OsStatus) with
/// `kAudioUnitErr_InvalidProperty` (-10879) from the 57 of 59 units that do not
/// implement the property. That refusal is the normal answer for an effect and a
/// caller should treat it as "this AU does not do MIDI mapping", not as a fault.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn all(unit: AudioUnit) -> Result<Vec<AuMidiMapping>> {
    let bytes = get_property_bytes(
        unit,
        K_AUDIO_UNIT_PROPERTY_ALL_PARAMETER_MIDI_MAPPINGS,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )?;
    Ok(decode_table(&bytes))
}

/// Decode a byte buffer of `AUParameterMIDIMapping` structs.
///
/// Split out from [`all`] so the decode is unit-testable without an AU, and so
/// the truncation rule lives in one place: a buffer whose length is not a
/// multiple of the struct size is **truncated to whole structs** rather than
/// rejected. That is the safe reading of a size an AU reported — a partial
/// trailing struct would be read out of uninitialized bytes, and refusing the
/// whole table over one bad byte would lose the mappings that did decode.
fn decode_table(bytes: &[u8]) -> Vec<AuMidiMapping> {
    let stride = size_of::<AuParameterMidiMappingRaw>();
    bytes
        .chunks_exact(stride)
        .map(|chunk| {
            // SAFETY: `chunk` is exactly `stride` bytes and the raw struct is
            // `#[repr(C)]` with no padding-sensitive reads and no invalid bit
            // patterns (every field is an integer or an `f32`), so any byte
            // sequence of the right length is a valid value. Read unaligned
            // because `bytes` came from a `Vec<u8>`, whose alignment is 1 —
            // the struct's alignment is 4, so a plain deref would be UB.
            let raw = unsafe {
                std::ptr::read_unaligned(chunk.as_ptr() as *const AuParameterMidiMappingRaw)
            };
            AuMidiMapping::from_raw(&raw)
        })
        .collect()
}

/// Encode mappings into the flat buffer the property expects.
///
/// Separate from the write calls so the encode is testable without an AU, and so
/// both [`add`] and [`set_all`] share one layout.
fn encode_table(mappings: &[AuMidiMapping]) -> Vec<AuParameterMidiMappingRaw> {
    mappings
        .iter()
        .copied()
        .map(AuMidiMapping::to_raw)
        .collect()
}

/// Write `mappings` to a property that takes an array of them.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit, and `property` must be one
/// that takes an `AUParameterMIDIMapping` array.
unsafe fn write_table(unit: AudioUnit, property: u32, mappings: &[AuMidiMapping]) -> Result<()> {
    let raw = encode_table(mappings);
    check(
        "AudioUnitSetProperty",
        AudioUnitSetProperty(
            unit,
            property,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            raw.as_ptr() as *const c_void,
            (raw.len() * size_of::<AuParameterMidiMappingRaw>()) as u32,
        ),
    )
}

/// Add `mappings` to whatever the AU already has, replacing any mapping that
/// already targets the same parameter.
///
/// A no-op for an empty slice: a zero-length write is `paramErr` (-50) on both
/// implementers, so it is refused *before* the FFI call rather than surfaced as
/// a confusing error for an operation that asked for nothing.
///
/// # Errors
/// `kAudioUnitErr_InvalidProperty` from a unit that does not implement the
/// family. Note that a `noErr` return proves **nothing** about the mapping being
/// meaningful — a parameter id no parameter uses is accepted and stored. Verify
/// with [`all`] if it matters.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn add(unit: AudioUnit, mappings: &[AuMidiMapping]) -> Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }
    write_table(
        unit,
        K_AUDIO_UNIT_PROPERTY_ADD_PARAMETER_MIDI_MAPPING,
        mappings,
    )
}

/// Remove the mappings targeting each of `mappings`'
/// `(scope, element, parameter_id)`.
///
/// Only that triple is matched — the trigger and flags of the argument are
/// ignored — so a caller can remove a mapping it read back, or construct one
/// naming just the parameter. A mapping that is not installed is silently
/// ignored, which the header specifies and both implementers honour (`noErr`).
///
/// Empty slice is a no-op, as in [`add`], and for the same `paramErr` reason.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn remove(unit: AudioUnit, mappings: &[AuMidiMapping]) -> Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }
    write_table(
        unit,
        K_AUDIO_UNIT_PROPERTY_REMOVE_PARAMETER_MIDI_MAPPING,
        mappings,
    )
}

/// Replace the AU's entire mapping table with `mappings`.
///
/// # Why an empty slice takes a different path
///
/// The header reads as though writing an empty table clears it. It does not.
/// Measured on both implementers: a NULL/0-size write answers
/// `kAudioUnitErr_InvalidPropertyValue` (-10851), a non-NULL/0-length write
/// answers `paramErr` (-50), and **the table is unchanged either way**. So the
/// clear is done the only way that works — read the table back and [`remove`]
/// every entry, which both units accept.
///
/// That read-then-remove is not atomic. It cannot be: the AU offers no clear
/// operation. A mapping the plugin adds itself between the two calls (a hot map
/// completing) survives the clear. Documented rather than papered over, because
/// a caller that cares must serialize its own mapping edits.
///
/// # Errors
/// `kAudioUnitErr_InvalidProperty` from a unit that does not implement the
/// family. The empty-slice path additionally surfaces the [`all`] read's error.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn set_all(unit: AudioUnit, mappings: &[AuMidiMapping]) -> Result<()> {
    if mappings.is_empty() {
        let existing = all(unit)?;
        return remove(unit, &existing);
    }
    write_table(
        unit,
        K_AUDIO_UNIT_PROPERTY_ALL_PARAMETER_MIDI_MAPPINGS,
        mappings,
    )
}

/// Arm "learn" mode: the AU maps the **next MIDI message it sees** to the
/// parameter `mapping` names.
///
/// The trigger and channel of `mapping` are ignored by the AU — it fills those
/// in from whatever arrives — so a caller should supply only the parameter
/// target. Verified end-to-end: arming parameter 2 on AUSampler and then sending
/// CC 11 made [`hot_map`] report `ControlChange { controller: 11 }` and grew the
/// table by one.
///
/// The header says the AU fires a notification on this property when the mapping
/// completes; a host that would rather not poll can watch for it through
/// [`crate::listener`].
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn arm_hot_map(unit: AudioUnit, mapping: &AuMidiMapping) -> Result<()> {
    let raw = mapping.to_raw();
    crate::ffi::set_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_HOT_MAP_PARAMETER_MIDI_MAPPING,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
        &raw,
    )
}

/// Read the pending or just-completed hot map.
///
/// # Why this returns `Option`, and why the status is not the signal
///
/// The header says an AU with no hot map in progress "should return a
/// `kAudioUnitErr_InvalidPropertyValue` error". **Neither implementer does**:
/// both answer `noErr` with an all-zero struct, on a completely fresh instance.
/// So a host branching on the `Result` would read an all-zero mapping as a real
/// one — "note off, note 0, channel 0" — and offer to bind it.
///
/// `None` therefore means "nothing armed or nothing arrived yet", decided on
/// `mStatus == 0`: zero is not a valid MIDI status byte (the high bit is
/// always set on a real one), so it cannot collide with a genuine mapping. An
/// AU that *does* follow the header and errors is also reported as `None`,
/// since "the property errored" and "nothing is pending" are the same answer to
/// the caller's question.
///
/// A `Some` whose `trigger` is filled in is the completed mapping; the armed
/// state before any MIDI arrives reads as `None`, because the AU has not put a
/// status byte in it yet.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn hot_map(unit: AudioUnit) -> Option<AuMidiMapping> {
    let raw: AuParameterMidiMappingRaw = crate::ffi::get_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_HOT_MAP_PARAMETER_MIDI_MAPPING,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )
    .ok()?;
    // See the docs above: `mStatus == 0` is the only reliable "nothing here"
    // signal, because the documented error is not delivered.
    (raw.status != 0).then(|| AuMidiMapping::from_raw(&raw))
}

/// Whether the AU currently reports a completed hot map waiting to be read.
///
/// Thin, but it is the predicate a "learn" UI polls, and having it named here
/// keeps the `mStatus == 0` rule from being reimplemented (differently) at each
/// call site.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn hot_map_pending(unit: AudioUnit) -> bool {
    hot_map(unit).is_some()
}

/// Whether this AU implements the parameter↔MIDI mapping family at all.
///
/// The capability gate a host should branch on before offering a MIDI-learn UI,
/// and the counterpart of [`crate::midi_out::midi_output_info`]. Asks
/// `AudioUnitGetPropertyInfo` for property 41 rather than reading the table,
/// because a unit that implements it answers `noErr` with size 0 when it has no
/// mappings — indistinguishable from an empty read otherwise.
///
/// Measured on macOS 15.6: true for exactly 2 of 59 installed components
/// (AUSampler, AUMIDISynth), false for the other 57 including every effect and
/// every third-party unit on this machine.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn supports_parameter_midi_mapping(unit: AudioUnit) -> bool {
    property_size(
        unit,
        K_AUDIO_UNIT_PROPERTY_ALL_PARAMETER_MIDI_MAPPINGS,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The C ABI, pinned against values printed by a C program compiled against
    /// Apple's own `AudioUnitProperties.h` on macOS 15.6:
    ///
    /// ```text
    /// AUParameterMIDIMapping sizeof=32 align=4
    ///   mScope=0 mElement=4 mParameterID=8 mFlags=12
    ///   mSubRangeMin=16 mSubRangeMax=20 mStatus=24 mData1=25 reserved3=28
    /// ```
    ///
    /// A wrong layout here is silent: the AU reads the host's `mStatus` out of
    /// the wrong byte and maps a different MIDI message than the host asked for.
    #[test]
    fn the_mapping_struct_matches_the_c_abi() {
        assert_eq!(size_of::<AuParameterMidiMappingRaw>(), 32);
        assert_eq!(std::mem::align_of::<AuParameterMidiMappingRaw>(), 4);

        // Field offsets, computed from a real value rather than asserted by
        // eye — `offset_of!` over every field, including the two `u8`s that
        // share a word with the reserved bytes.
        assert_eq!(std::mem::offset_of!(AuParameterMidiMappingRaw, scope), 0);
        assert_eq!(std::mem::offset_of!(AuParameterMidiMappingRaw, element), 4);
        assert_eq!(
            std::mem::offset_of!(AuParameterMidiMappingRaw, parameter_id),
            8
        );
        assert_eq!(std::mem::offset_of!(AuParameterMidiMappingRaw, flags), 12);
        assert_eq!(
            std::mem::offset_of!(AuParameterMidiMappingRaw, sub_range_min),
            16
        );
        assert_eq!(
            std::mem::offset_of!(AuParameterMidiMappingRaw, sub_range_max),
            20
        );
        assert_eq!(std::mem::offset_of!(AuParameterMidiMappingRaw, status), 24);
        assert_eq!(std::mem::offset_of!(AuParameterMidiMappingRaw, data1), 25);
        assert_eq!(
            std::mem::offset_of!(AuParameterMidiMappingRaw, reserved3),
            28
        );
    }

    /// The flag constants must equal Apple's, which are `1 << 0 .. 1 << 5`.
    /// Spelled out rather than derived, because `coreaudio-sys` does not export
    /// them and a transposed pair (`Toggle` for `SubRange`) would change what an
    /// incoming controller value means with no compile error.
    #[test]
    fn the_flag_bits_match_the_header() {
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL, 1);
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE, 2);
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE, 4);
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_TOGGLE, 8);
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR, 16);
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR_ON, 32);
        // And the property ids, from the same C run: All=41 Add=42 Remove=43
        // HotMap=44.
        assert_eq!(K_AUDIO_UNIT_PROPERTY_ALL_PARAMETER_MIDI_MAPPINGS, 41);
        assert_eq!(K_AUDIO_UNIT_PROPERTY_ADD_PARAMETER_MIDI_MAPPING, 42);
        assert_eq!(K_AUDIO_UNIT_PROPERTY_REMOVE_PARAMETER_MIDI_MAPPING, 43);
        assert_eq!(K_AUDIO_UNIT_PROPERTY_HOT_MAP_PARAMETER_MIDI_MAPPING, 44);
    }

    /// Every trigger must survive `to_raw`/`from_raw`, and the status byte must
    /// carry the command nibble Apple's table specifies. The table itself is the
    /// assertion — a transposed nibble maps note-off where the host meant CC.
    #[test]
    fn trigger_round_trips_through_the_status_byte() {
        let cases = [
            (MidiTrigger::NoteOff { note: 60 }, 0x80u8, 60u8),
            (MidiTrigger::NoteOn { note: 61 }, 0x90, 61),
            (MidiTrigger::KeyPressure { note: 62 }, 0xA0, 62),
            (MidiTrigger::ControlChange { controller: 1 }, 0xB0, 1),
            (MidiTrigger::ProgramChange { patch: 5 }, 0xC0, 5),
            // The two forms whose mData1 the header marks unused must encode 0.
            (MidiTrigger::ChannelPressure, 0xD0, 0),
            (MidiTrigger::PitchBend, 0xE0, 0),
        ];
        for (trigger, nibble, data1) in cases {
            let m = AuMidiMapping {
                trigger,
                channel: 3,
                ..AuMidiMapping::control_change(0, 3, 0)
            };
            let raw = m.to_raw();
            assert_eq!(
                raw.status,
                nibble | 3,
                "{trigger:?} must encode status {nibble:#x} | channel 3"
            );
            assert_eq!(raw.data1, data1, "{trigger:?} mData1");
            assert_eq!(
                AuMidiMapping::from_raw(&raw).trigger,
                trigger,
                "{trigger:?} must survive the round trip"
            );
            assert_eq!(AuMidiMapping::from_raw(&raw).channel, 3);
        }
    }

    /// A status byte outside the seven channel-voice commands must decode to
    /// `Other`, not be mistaken for the nearest match. `0xF8` (MIDI clock) and
    /// `0x00` (not a status byte at all) are both shapes an AU could store —
    /// one implementer stores an out-of-range `mScope` verbatim, so garbage in
    /// this field is not hypothetical.
    #[test]
    fn unknown_status_bytes_decode_to_other() {
        for status in [0xF8u8, 0xFF, 0x00, 0x7F] {
            let raw = AuParameterMidiMappingRaw {
                status,
                data1: 42,
                ..Default::default()
            };
            assert_eq!(
                AuMidiMapping::from_raw(&raw).trigger,
                MidiTrigger::Other { status, data1: 42 },
                "status {status:#x} must not be decoded as a known command"
            );
        }
    }

    /// The channel nibble must be masked on the way out. Without the mask a
    /// caller passing an out-of-range channel sets bits above the low nibble and
    /// silently changes *which MIDI message* the mapping listens for — a
    /// `ControlChange` (`0xB0`) becomes a system message (`0xF0`), so the mapping
    /// never fires.
    ///
    /// The values here are chosen to actually expose that. This test originally
    /// used `channel: 16` — the natural off-by-one from the 1-based channel
    /// numbering every DAW displays — and **passed with the mask deleted**,
    /// because `0xB0 | 16 == 0xB0`: bit 4 is already set in the CC status
    /// nibble, so 16 is absorbed. Each case below is asserted against the
    /// unmasked result it would produce, so no case can be silently absorbed
    /// again.
    #[test]
    fn an_out_of_range_channel_cannot_corrupt_the_command_nibble() {
        // (channel, what `0xB0 | channel` would give unmasked, masked low nibble)
        let cases = [
            (64u8, 0xF0u8, 0u8),
            (100, 0xF0, 4),
            (255, 0xF0, 15),
            (17, 0xB0, 1),
        ];
        for (channel, unmasked_nibble, expected_low) in cases {
            let raw = AuMidiMapping::control_change(7, channel, 74).to_raw();
            assert_eq!(
                raw.status & 0xF0,
                0xB0,
                "channel {channel} must not bleed into the command nibble — \
                 unmasked it would produce {unmasked_nibble:#x}"
            );
            assert_eq!(
                raw.status & 0x0F,
                expected_low,
                "channel {channel} must mask to its low nibble"
            );
            assert_eq!(
                AuMidiMapping::from_raw(&raw).trigger,
                MidiTrigger::ControlChange { controller: 74 },
                "channel {channel} must still decode as a control change"
            );
        }
        // At least one case must be one the unmasked path gets WRONG, or this
        // test cannot fail. `64` and `255` both land on 0xF0; assert that here
        // so a future edit cannot narrow the table down to absorbed values only.
        assert!(
            cases.iter().any(|(_, unmasked, _)| *unmasked != 0xB0),
            "the case table must contain a channel the unmasked path corrupts"
        );
    }

    /// Each flag must occupy its own bit and survive the round trip. Asserted
    /// one flag at a time *and* all together, because a `|=` written against the
    /// wrong constant only shows up when the two flags are set independently.
    #[test]
    fn each_flag_round_trips_independently() {
        let base = AuMidiMapping::control_change(1, 0, 1);

        let cases: [(&str, AuMidiMapping, u32); 5] = [
            (
                "any_channel",
                AuMidiMapping {
                    any_channel: true,
                    ..base
                },
                K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL,
            ),
            (
                "any_note",
                AuMidiMapping {
                    any_note: true,
                    ..base
                },
                K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE,
            ),
            (
                "toggle",
                AuMidiMapping {
                    toggle: true,
                    ..base
                },
                K_AU_PARAMETER_MIDI_MAPPING_TOGGLE,
            ),
            (
                "bipolar",
                AuMidiMapping {
                    bipolar: true,
                    ..base
                },
                K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR,
            ),
            (
                "bipolar_on",
                AuMidiMapping {
                    bipolar_on: true,
                    ..base
                },
                K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR_ON,
            ),
        ];
        for (name, mapping, bit) in cases {
            let raw = mapping.to_raw();
            assert_eq!(raw.flags, bit, "{name} must set exactly bit {bit}");
            assert_eq!(
                AuMidiMapping::from_raw(&raw),
                mapping,
                "{name} must survive the round trip"
            );
        }

        // All six at once, sub-range included.
        let every = AuMidiMapping {
            any_channel: true,
            any_note: true,
            sub_range: Some((0.25, 0.75)),
            toggle: true,
            bipolar: true,
            bipolar_on: true,
            ..base
        };
        let raw = every.to_raw();
        assert_eq!(raw.flags, 0b11_1111, "all six flags");
        assert_eq!(AuMidiMapping::from_raw(&raw), every);
    }

    /// The sub-range flag and its two value fields must move together. This is
    /// the case a `bool` + two floats would get wrong: `None` must not leave
    /// stale bounds in the struct (the AU stores what it is handed), and a
    /// mapping whose flag is clear must decode as `None` **whatever** the two
    /// value fields hold — otherwise a host would show a sub-range that the AU
    /// is ignoring.
    #[test]
    fn sub_range_travels_with_its_flag() {
        let none = AuMidiMapping::control_change(1, 0, 1);
        let raw = none.to_raw();
        assert_eq!(raw.flags & K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE, 0);
        assert_eq!(
            (raw.sub_range_min, raw.sub_range_max),
            (0.0, 0.0),
            "a None sub-range must zero the bounds, not leave them stale"
        );

        // Flag clear but bounds populated — an AU could hand this back.
        let stale = AuParameterMidiMappingRaw {
            flags: 0,
            sub_range_min: 0.1,
            sub_range_max: 0.9,
            status: 0xB0,
            ..Default::default()
        };
        assert_eq!(
            AuMidiMapping::from_raw(&stale).sub_range,
            None,
            "bounds without the flag must read as no sub-range"
        );

        // And a real sub-range survives verbatim, in parameter units (not
        // normalized) — the AUSampler `Gain` case is -96..=12 dB.
        let dbs = AuMidiMapping {
            sub_range: Some((-24.0, 6.0)),
            ..AuMidiMapping::control_change(900, 0, 7)
        };
        let raw = dbs.to_raw();
        assert_ne!(raw.flags & K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE, 0);
        assert_eq!((raw.sub_range_min, raw.sub_range_max), (-24.0, 6.0));
        assert_eq!(AuMidiMapping::from_raw(&raw), dbs);
    }

    /// The reserved fields must be zeroed on every encode. Apple's header says
    /// "MUST be set to zero"; they are private precisely so no call site can
    /// leave them holding whatever was on the stack.
    #[test]
    fn reserved_fields_are_always_zero() {
        let m = AuMidiMapping {
            sub_range: Some((0.1, 0.2)),
            any_channel: true,
            toggle: true,
            ..AuMidiMapping::control_change(9, 5, 64)
        };
        let raw = m.to_raw();
        assert_eq!((raw.reserved1, raw.reserved2, raw.reserved3), (0, 0, 0));
    }

    /// A table decodes as a whole, and a buffer with a partial trailing struct
    /// is truncated to whole structs rather than reading uninitialized bytes.
    /// The unaligned read matters: the bytes come from a `Vec<u8>` (align 1)
    /// while the struct wants align 4, so a plain pointer deref would be UB.
    #[test]
    fn decode_table_handles_whole_and_partial_buffers() {
        let mappings = [
            AuMidiMapping::control_change(900, 0, 1),
            AuMidiMapping {
                sub_range: Some((0.25, 0.75)),
                toggle: true,
                ..AuMidiMapping::control_change(901, 3, 74)
            },
        ];
        let raw = encode_table(&mappings);
        // SAFETY: `raw` is a live slice of `#[repr(C)]` PODs; reading it as
        // bytes is the same reinterpretation the FFI write performs.
        let bytes = unsafe {
            std::slice::from_raw_parts(raw.as_ptr() as *const u8, std::mem::size_of_val(&*raw))
        };
        assert_eq!(bytes.len(), 64);
        assert_eq!(decode_table(bytes), mappings);

        // 63 bytes: one whole struct plus a fragment.
        assert_eq!(decode_table(&bytes[..63]), mappings[..1]);
        // Shorter than one struct: nothing decodes.
        assert!(decode_table(&bytes[..31]).is_empty());
        assert!(decode_table(&[]).is_empty());
    }

    /// Remove and replace both match on `(scope, element, parameter_id)` only,
    /// so the predicate must ignore the trigger and every flag — and must NOT
    /// ignore the element, which is what distinguishes two parameters that share
    /// an id across elements.
    #[test]
    fn targets_same_parameter_keys_on_the_documented_triple() {
        let a = AuMidiMapping::control_change(900, 0, 1);
        let b = AuMidiMapping {
            trigger: MidiTrigger::PitchBend,
            channel: 9,
            toggle: true,
            sub_range: Some((0.0, 0.5)),
            ..a
        };
        assert!(
            a.targets_same_parameter(&b),
            "trigger and flags must not count"
        );

        let other_param = AuMidiMapping::control_change(901, 0, 1);
        assert!(!a.targets_same_parameter(&other_param));

        let other_element = AuMidiMapping { element: 1, ..a };
        assert!(
            !a.targets_same_parameter(&other_element),
            "the element is part of the key"
        );

        let other_scope = AuMidiMapping {
            scope: K_AUDIO_UNIT_SCOPE_INPUT,
            ..a
        };
        assert!(
            !a.targets_same_parameter(&other_scope),
            "the scope is part of the key"
        );
    }

    /// The any-channel constructor must set the flag *and* leave a channel the
    /// AU will ignore — not silently produce a channel-1 mapping that looks
    /// identical to a deliberate one.
    #[test]
    fn any_channel_constructor_sets_the_flag() {
        let m = AuMidiMapping::control_change_any_channel(900, 1);
        assert!(m.any_channel);
        assert_eq!(m.channel, 0);
        assert_eq!(m.trigger, MidiTrigger::ControlChange { controller: 1 });
        assert_ne!(
            m.to_raw().flags & K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL,
            0
        );
        // And it differs from the explicit-channel form, so the two are not
        // interchangeable at a call site.
        assert_ne!(m, AuMidiMapping::control_change(900, 0, 1));
    }

    /// `is_note_command` gates the any-note flag, and the header restricts that
    /// flag to exactly note on / note off / polyphonic pressure. A wrong answer
    /// here would let a UI offer "any note" for a pitch-bend mapping.
    #[test]
    fn only_note_commands_are_note_commands() {
        assert!(MidiTrigger::NoteOn { note: 0 }.is_note_command());
        assert!(MidiTrigger::NoteOff { note: 0 }.is_note_command());
        assert!(MidiTrigger::KeyPressure { note: 0 }.is_note_command());
        assert!(!MidiTrigger::ControlChange { controller: 1 }.is_note_command());
        assert!(!MidiTrigger::ProgramChange { patch: 0 }.is_note_command());
        assert!(!MidiTrigger::ChannelPressure.is_note_command());
        assert!(!MidiTrigger::PitchBend.is_note_command());
        assert!(!MidiTrigger::Other {
            status: 0xF8,
            data1: 0
        }
        .is_note_command());
    }
}
