//! `effGetParameterProperties` and the MIDI-metadata opcode family.
//!
//! # There is no CC→parameter mapping query in VST2
//!
//! VST3 has `IMidiMapping::getMidiControllerAssignment`, which answers "which
//! parameter does CC 1 on channel 0 drive?"; CLAP has `clap_plugin_param_indication`
//! and the note-ports/param mapping extensions. **VST 2.4 has no equivalent, and
//! the vendored bindings confirm it**: the full `OpCode` enum (`vst-tutti`'s
//! `plugin.rs`) contains no opcode that takes a controller number, and none that
//! returns a parameter index for anything. A VST2 plugin receives MIDI CCs as
//! raw `MidiEvent`s in `effProcessEvents` and interprets them internally; the
//! host cannot see, or override, that routing. There is nothing to implement.
//!
//! That is the whole answer to "does VST2 support CC→parameter mapping". What
//! VST2 *does* have — and what this module implements — is the adjacent
//! metadata surface the host was ignoring entirely:
//!
//! * `effGetParameterProperties` (opcode 56, `GetParamInfo` in the bindings)
//!   describes a parameter's integer range, step granularity and grouping.
//!   Without it a host shows a flat list of normalized 0..1 sliders, because
//!   name and label are all it has.
//! * `effGetMidiProgramName` / `effGetCurrentMidiProgram` /
//!   `effGetMidiProgramCategory` / `effHasMidiProgramsChanged` (62-65) tie
//!   program names to the MIDI program-change and bank-select numbers that
//!   select them.
//! * `effGetMidiKeyName` (66) names individual keys, which is how a drum plugin
//!   tells a host to label a pad "Kick" rather than "note 36".
//!
//! # All of it is optional, and real plugins decline
//!
//! Measured (see `tests/vst2_param_properties.rs` for the recorded numbers)
//! against every VST2 plugin installed on the development machine —
//! TAL-NoiseMaker (88 params, synth), TAL-Reverb-4 (20 params), TDR Nova (75
//! params, 73 programs): **all three answer `0` to `effGetParameterProperties`
//! for every parameter, and decline the entire MIDI-metadata family.** So these
//! accessors return `Option`, absence is the expected case, and the caller must
//! keep its name/label fallback. A host that assumed the data was there would
//! render every parameter as the degenerate integer range `0..0`.
//!
//! Absence is always detected from the dispatch **return value**, never by
//! inspecting the buffer: an unimplemented opcode falls through the plugin's
//! dispatcher without writing, leaving the zeros the host put there, which is
//! byte-identical to a plugin genuinely reporting "no range, no category".
//! One measured plugin answers `-1` rather than `0` to
//! `effGetCurrentMidiProgram`, which is why the decode compares against the
//! spec'd success value instead of testing `!= 0`.

use vst::api;
use vst::plugin::Plugin as _;

use crate::instance::Vst2Instance;

/// How many MIDI channels the program/key queries are enumerated over.
///
/// VST2 scopes `effGetMidiProgramName` and friends to a channel index, and MIDI
/// 1.0 has 16 channels per port.
pub const NUM_MIDI_CHANNELS: i32 = 16;

/// Highest MIDI note number, so key-name walks have a bound that is not a
/// magic literal at the call site.
pub const MAX_MIDI_KEY: i32 = 127;

/// Which fields of a [`ParameterProperties`] the plugin declared valid.
///
/// Modelled as typed flags rather than a bool because the flags word is the
/// *validity gate* for the rest of the struct, and each bit gates a different
/// group: reading `min_integer`/`max_integer` without `USES_INT_STEP`, or
/// `category` without `USES_CATEGORY`, reads whatever the plugin left in the
/// buffer. Collapsing them to "has properties" would lose exactly the
/// information that makes the payload safe to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ParameterPropertyFlags {
    /// The integer range (`min`/`max`/`step`/`large_step`) is meaningful.
    pub uses_int_step: bool,
    /// The float step trio is meaningful.
    pub uses_float_step: bool,
    /// `display_index` is meaningful.
    pub uses_index: bool,
    /// The category fields are meaningful.
    pub uses_category: bool,
    /// The plugin says this parameter can be ramped/automated smoothly.
    pub can_ramp: bool,
}

impl ParameterPropertyFlags {
    /// Decode the raw flags word.
    ///
    /// Undefined bits are ignored rather than rejected: the flags word is
    /// plugin-authored and VST2 reserves the high bits, so a plugin setting one
    /// must not invalidate the bits we do understand.
    fn from_bits(raw: i32) -> Self {
        let bits = api::ParameterFlags::from_bits_truncate(raw);
        Self {
            uses_int_step: bits.contains(api::ParameterFlags::USES_INT_STEP),
            uses_float_step: bits.contains(api::ParameterFlags::USES_FLOAT_STEP),
            uses_index: bits.contains(api::ParameterFlags::USES_INDEX),
            uses_category: bits.contains(api::ParameterFlags::USES_CATEGORY),
            can_ramp: bits.contains(api::ParameterFlags::CAN_RAMP),
        }
    }
}

/// The integer range a parameter declares, when it declares one.
///
/// A separate type because it exists only under `USES_INT_STEP`. Making that
/// conditionality structural (`Option<IntegerRange>`) means a caller cannot read
/// the bounds without having handled their absence — the flat-struct
/// alternative invites reading a `min`/`max` pair that is meaningless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegerRange {
    pub min: i32,
    pub max: i32,
    /// Increment for a single step.
    pub step: i32,
    /// Increment for a coarse (page) step.
    pub large_step: i32,
}

impl IntegerRange {
    /// Number of discrete steps the range spans, or `None` if the plugin
    /// reported a range that cannot be stepped through.
    ///
    /// Guards three plugin answers that are each individually plausible and
    /// jointly fatal to a naive `(max - min) / step`: a non-positive `step`
    /// (division by zero, or an infinite loop in a caller that walks the
    /// range), an inverted range, and a `max - min` that overflows `i32`.
    pub fn step_count(&self) -> Option<u32> {
        if self.step <= 0 || self.max < self.min {
            return None;
        }
        let span = (self.max as i64) - (self.min as i64);
        Some((span / self.step as i64) as u32)
    }
}

/// The float step granularity a parameter declares, under `USES_FLOAT_STEP`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatSteps {
    pub step: f32,
    pub small_step: f32,
    pub large_step: f32,
}

/// The grouping a parameter declares, under `USES_CATEGORY`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterCategory {
    /// VST2 numbers categories from 1; `0` means "uncategorised" even with the
    /// flag set, so a zero index is reported as [`None`] by the decoder rather
    /// than as category 0.
    pub index: u16,
    /// How many parameters the plugin says share this category.
    pub parameter_count: u16,
    /// The category's short label; may be empty.
    pub label: String,
}

/// Everything `effGetParameterProperties` reported for one parameter.
///
/// Each optional field is `Some` only when its gating flag was set, so the
/// type cannot represent "range present but not declared valid".
#[derive(Debug, Clone, PartialEq)]
pub struct ParameterProperties {
    /// Index the properties were queried for.
    pub index: i32,
    /// Full display label. May be empty even on a successful query.
    pub label: String,
    /// Short label for narrow UI slots. May be empty.
    pub short_label: String,
    /// The raw validity flags, decoded.
    pub flags: ParameterPropertyFlags,
    /// Integer range, when `uses_int_step`.
    pub integer_range: Option<IntegerRange>,
    /// Float step granularity, when `uses_float_step`.
    pub float_steps: Option<FloatSteps>,
    /// Preferred display position, when `uses_index`.
    pub display_index: Option<u16>,
    /// Grouping, when `uses_category` and the index is non-zero.
    pub category: Option<ParameterCategory>,
}

/// One named MIDI program, with the MIDI numbers that select it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiProgram {
    /// Index queried.
    pub index: i32,
    /// Program name; may be empty.
    pub name: String,
    /// Program-change number (0-127) selecting this program.
    pub midi_program: u8,
    /// Bank-select MSB/LSB pair, or `None` when the plugin marks them unused.
    ///
    /// VST2 signals "unused" with `255`, which is not a legal 7-bit MIDI data
    /// byte. Passing it through as a number would have a caller emit CC 0 with
    /// value 255 — truncated to 127 on the wire, selecting a real and wrong
    /// bank. Modelling it as `None` makes that unrepresentable.
    pub bank: Option<(u8, u8)>,
    /// Enclosing category index, or `None` at the top level (VST2 uses `-1`).
    pub parent_category: Option<i32>,
    /// Set when the plugin declares this program a drum kit, whose keys are
    /// individual instruments — the cue to query key names.
    pub is_drum_kit: bool,
}

/// A category grouping MIDI programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiProgramCategory {
    pub index: i32,
    pub name: String,
    /// Enclosing category, or `None` at the top level.
    pub parent_category: Option<i32>,
}

/// The plugin's name for one MIDI key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiKeyName {
    /// Program the name is scoped to.
    pub program_index: i32,
    /// MIDI note number (0-127).
    pub key_number: i32,
    /// The name; may be empty even on a successful query.
    pub name: String,
}

/// Decode a fixed-size, NUL-padded C string field the plugin wrote.
///
/// Stops at the first NUL and lossily decodes the rest. Both halves matter:
/// VST2 pads with NULs and plugins are not required to terminate a field that
/// exactly fills it, and the bytes are plugin-authored so they are not
/// guaranteed UTF-8. A `from_utf8` that errored would drop an otherwise good
/// name over one stray byte in a vendor's copyright sign.
fn decode_label(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Decode a bank-select byte pair, mapping VST2's `255` sentinel to absence.
///
/// Either byte reading 255 discards the pair: a half-specified bank select is
/// not a bank select, and emitting only the MSB would leave the LSB at whatever
/// the receiver last saw.
fn decode_bank(msb: u8, lsb: u8) -> Option<(u8, u8)> {
    const UNUSED: u8 = 255;
    if msb == UNUSED || lsb == UNUSED {
        return None;
    }
    Some((msb, lsb))
}

/// VST2's "no parent" sentinel for the category index fields.
const NO_PARENT_CATEGORY: i32 = -1;

/// Map a parent-category field onto `Option`, treating any negative value as
/// absent.
///
/// The spec names `-1`, but a plugin returning another negative number means
/// the same thing and must not become a lookup at a negative index.
fn decode_parent_category(raw: i32) -> Option<i32> {
    if raw <= NO_PARENT_CATEGORY {
        return None;
    }
    Some(raw)
}

impl ParameterProperties {
    /// Decode the raw `#[repr(C)]` struct the plugin filled in.
    ///
    /// Kept separate from the query so the decode is testable against
    /// hand-built structs. That is not a convenience: no plugin available here
    /// implements the opcode, so a test that could only go through a real
    /// plugin would assert nothing.
    fn decode(index: i32, raw: &api::ParameterProperties) -> Self {
        let flags = ParameterPropertyFlags::from_bits(raw.flags);

        Self {
            index,
            label: decode_label(&raw.label),
            short_label: decode_label(&raw.short_label),
            flags,
            // Each payload is read only under its own gate. The plugin is not
            // required to zero the fields it does not describe.
            integer_range: flags.uses_int_step.then_some(IntegerRange {
                min: raw.min_integer,
                max: raw.max_integer,
                step: raw.step_integer,
                large_step: raw.large_step_integer,
            }),
            float_steps: flags.uses_float_step.then_some(FloatSteps {
                step: raw.step_float,
                small_step: raw.small_step_float,
                large_step: raw.large_step_float,
            }),
            // `display_index` is a position, so a negative one is not a
            // smaller position — it is a malformed answer.
            display_index: flags
                .uses_index
                .then(|| u16::try_from(raw.display_index).ok())
                .flatten(),
            category: flags
                .uses_category
                .then(|| Self::decode_category(raw))
                .flatten(),
        }
    }

    /// Decode the category triple, or `None` when the plugin set the flag but
    /// left the index at VST2's `0` = uncategorised.
    fn decode_category(raw: &api::ParameterProperties) -> Option<ParameterCategory> {
        // Categories are 1-based. A zero index with the flag set means
        // "this parameter is in no category" — reporting it as category 0
        // would invent a group every uncategorised parameter shares.
        let index = u16::try_from(raw.category).ok()?;
        if index == 0 {
            return None;
        }
        Some(ParameterCategory {
            index,
            // A negative count is malformed; clamp to zero rather than
            // wrapping into a huge one.
            parameter_count: u16::try_from(raw.num_parameters_in_category).unwrap_or(0),
            label: decode_label(&raw.category_label),
        })
    }
}

impl MidiProgram {
    /// Decode the raw struct. Separate from the query for the same reason as
    /// [`ParameterProperties::decode`].
    fn decode(index: i32, raw: &api::MidiProgramName) -> Self {
        Self {
            index,
            name: decode_label(&raw.name),
            midi_program: raw.midi_program,
            bank: decode_bank(raw.midi_bank_msb, raw.midi_bank_lsb),
            parent_category: decode_parent_category(raw.parent_category_index),
            is_drum_kit: api::MidiProgramFlags::from_bits_truncate(raw.flags)
                .contains(api::MidiProgramFlags::IS_OMNI),
        }
    }
}

impl MidiProgramCategory {
    fn decode(index: i32, raw: &api::MidiProgramCategory) -> Self {
        Self {
            index,
            name: decode_label(&raw.name),
            parent_category: decode_parent_category(raw.parent_category_index),
        }
    }
}

impl MidiKeyName {
    fn decode(program_index: i32, key_number: i32, raw: &api::MidiKeyName) -> Self {
        Self {
            program_index,
            key_number,
            name: decode_label(&raw.keyname),
        }
    }
}

impl Vst2Instance {
    /// Query `effGetParameterProperties` for one parameter.
    ///
    /// `None` when the index is out of range, or when the plugin does not
    /// implement the opcode — the common case, measured on every plugin
    /// available here. Callers must keep their name/label fallback.
    pub fn parameter_properties(&self, id: i32) -> Option<ParameterProperties> {
        // Range-check before dispatch. Plugins are not required to bounds-check
        // the index, and the probe's own out-of-range answers show why: an
        // unchecked walk reads properties for parameters that do not exist.
        let count = self.parameter_count();
        if id < 0 || id >= count {
            return None;
        }

        let raw = self.handle.instance.parameter_properties(id)?;
        Some(ParameterProperties::decode(id, &raw))
    }

    /// Query `effGetParameterProperties` for every declared parameter.
    ///
    /// Entries are `None` where the plugin declined, so the result stays index-
    /// aligned with [`Vst2Instance::parameters`]. Compacting to only the
    /// answered ones would silently renumber every parameter after a gap.
    pub fn all_parameter_properties(&self) -> Vec<Option<ParameterProperties>> {
        (0..self.parameter_count())
            .map(|i| self.parameter_properties(i))
            .collect()
    }

    /// Number of parameters the plugin advertises, never negative.
    ///
    /// `AEffect::numParams` is a signed `i32` and a malformed plugin can report
    /// a negative one; that must become "no parameters", not an empty range that
    /// happens to iterate zero times by accident. Clamping states it.
    fn parameter_count(&self) -> i32 {
        self.handle.instance.get_info().parameters.max(0)
    }

    /// Query `effGetMidiProgramName` for one program on one MIDI channel.
    ///
    /// Returns the program plus the number of programs the plugin says it
    /// services on that channel. `None` when the channel is out of range or the
    /// plugin declines.
    pub fn midi_program(&self, channel: i32, program_index: i32) -> Option<(MidiProgram, i32)> {
        if !(0..NUM_MIDI_CHANNELS).contains(&channel) || program_index < 0 {
            return None;
        }
        let (raw, serviced) = self
            .handle
            .instance
            .midi_program_name(channel, program_index)?;
        Some((MidiProgram::decode(program_index, &raw), serviced))
    }

    /// Enumerate every MIDI program the plugin services on `channel`.
    ///
    /// The plugin's own serviced count bounds the walk, and each entry is
    /// re-queried — the count from the first call is not assumed to hold, and a
    /// program the plugin stops answering for ends the list rather than
    /// contributing an empty name.
    ///
    /// Empty when unsupported, which is not distinguishable from "supports zero
    /// programs" and does not need to be: both mean there is nothing to show.
    pub fn midi_programs(&self, channel: i32) -> Vec<MidiProgram> {
        let Some((first, serviced)) = self.midi_program(channel, 0) else {
            return Vec::new();
        };

        let mut programs = Vec::with_capacity(serviced.max(1) as usize);
        programs.push(first);
        for index in 1..serviced {
            match self.midi_program(channel, index) {
                Some((program, _)) => programs.push(program),
                // The plugin advertised a count it will not service — the
                // enumeration hole the reference probe models. Stop at the
                // truth rather than padding with blanks.
                None => break,
            }
        }
        programs
    }

    /// Query `effGetCurrentMidiProgram` — which program `channel` is on.
    ///
    /// `None` when unsupported.
    ///
    /// # Why this needs a second query
    ///
    /// This opcode's return value alone cannot distinguish success from refusal,
    /// and it is the only one in the family with that defect. It returns the
    /// current program *index*, so `0` is a perfectly valid answer — and `0` is
    /// also what an unimplemented opcode returns after falling through the
    /// plugin's dispatcher. The two are byte-identical: same return value, and a
    /// buffer still holding the zeros the host put there.
    ///
    /// Both readings are wrong on real plugins. Treating `0` as unsupported
    /// discards a genuine "program 0", which is where most instruments sit at
    /// load. Treating it as supported invents a nameless program 0 for every
    /// plugin that ignores the opcode — the majority, and the bug this guard was
    /// added to fix after the reference probe reproduced it.
    ///
    /// So the question is answered by an opcode that *can* say no:
    /// `effGetMidiProgramName` reports a serviced count, where `0` is
    /// unambiguously "none". A plugin servicing no MIDI programs has no current
    /// one, so a `0` here is a refusal; a plugin that does service them means
    /// index 0 literally. (The three plugins measured here answer `-1`, which
    /// needs no disambiguation — but a `-1`-only guard is not enough, because
    /// vst-rs's own fall-through answers `0`.)
    pub fn current_midi_program(&self, channel: i32) -> Option<MidiProgram> {
        if !(0..NUM_MIDI_CHANNELS).contains(&channel) {
            return None;
        }

        // Gate on the query whose zero is unambiguous. Cheap: one extra
        // dispatch on a cold, UI-thread path.
        self.midi_program(channel, 0)?;

        let (raw, current) = self.handle.instance.current_midi_program(channel)?;
        Some(MidiProgram::decode(current, &raw))
    }

    /// Query `effGetMidiProgramCategory` for one category on `channel`.
    ///
    /// Returns the category plus the plugin's count of used categories.
    pub fn midi_program_category(
        &self,
        channel: i32,
        category_index: i32,
    ) -> Option<(MidiProgramCategory, i32)> {
        if !(0..NUM_MIDI_CHANNELS).contains(&channel) || category_index < 0 {
            return None;
        }
        let (raw, serviced) = self
            .handle
            .instance
            .midi_program_category(channel, category_index)?;
        Some((MidiProgramCategory::decode(category_index, &raw), serviced))
    }

    /// Query `effHasMidiProgramsChanged` — whether `channel`'s program or key
    /// names changed since the host last read them.
    ///
    /// A cache-invalidation signal, so it is `true` only on the spec'd `1`.
    pub fn midi_programs_changed(&self, channel: i32) -> bool {
        if !(0..NUM_MIDI_CHANNELS).contains(&channel) {
            return false;
        }
        self.handle.instance.midi_programs_changed(channel)
    }

    /// Query `effGetMidiKeyName` for one key.
    ///
    /// `None` when the arguments are out of range or the plugin declines. Drum
    /// plugins use this so a host can label a pad instead of showing a note
    /// number.
    pub fn midi_key_name(
        &self,
        channel: i32,
        program_index: i32,
        key_number: i32,
    ) -> Option<MidiKeyName> {
        if !(0..NUM_MIDI_CHANNELS).contains(&channel)
            || program_index < 0
            || !(0..=MAX_MIDI_KEY).contains(&key_number)
        {
            return None;
        }
        let raw = self
            .handle
            .instance
            .midi_key_name(channel, program_index, key_number)?;
        Some(MidiKeyName::decode(program_index, key_number, &raw))
    }

    /// Every key the plugin names for `program_index` on `channel`.
    ///
    /// Keys the plugin declines are omitted: an unnamed key is not an
    /// empty-named key, and a drum editor should fall back to the note number
    /// for it rather than render a blank pad.
    pub fn midi_key_names(&self, channel: i32, program_index: i32) -> Vec<MidiKeyName> {
        (0..=MAX_MIDI_KEY)
            .filter_map(|key| self.midi_key_name(channel, program_index, key))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw properties struct with the flags word and nothing else set,
    /// so a test can prove the gated fields are not read.
    fn raw_properties(flags: i32) -> api::ParameterProperties {
        api::ParameterProperties {
            step_float: 0.0,
            small_step_float: 0.0,
            large_step_float: 0.0,
            label: [0; 64],
            flags,
            min_integer: 0,
            max_integer: 0,
            step_integer: 0,
            large_step_integer: 0,
            short_label: [0; 8],
            display_index: 0,
            category: 0,
            num_parameters_in_category: 0,
            reserved: 0,
            category_label: [0; 8],
            future: [0; 16],
        }
    }

    /// Write a `&str` into a fixed NUL-padded field.
    fn label<const N: usize>(text: &str) -> [u8; N] {
        let mut out = [0u8; N];
        let bytes = text.as_bytes();
        out[..bytes.len()].copy_from_slice(bytes);
        out
    }

    /// A category index of 0 is "uncategorised", even with the flag set.
    ///
    /// VST2 numbers categories from 1, so 0 is the sentinel and not a real
    /// group. A decoder that trusted the flag alone would publish category 0
    /// with whatever label happened to sit in `category_label` — and since the
    /// shared `ParameterInfo.group` renders that label as a section heading,
    /// the visible result is every ungrouped parameter filed under one
    /// arbitrary name.
    #[test]
    fn a_zero_category_index_is_uncategorised() {
        let mut raw = raw_properties(api::ParameterFlags::USES_CATEGORY.bits());
        raw.category = 0;
        raw.category_label = label("Osc");
        assert_eq!(
            ParameterProperties::decode(0, &raw).category,
            None,
            "category 0 is VST2's uncategorised sentinel, not a group"
        );

        // …and a real index does decode, so the guard above is not simply
        // refusing everything.
        raw.category = 1;
        let decoded = ParameterProperties::decode(0, &raw)
            .category
            .expect("a non-zero index is a real category");
        assert_eq!(decoded.index, 1);
        assert_eq!(decoded.label, "Osc");
    }

    /// The integer range must be readable only under `USES_INT_STEP`.
    ///
    /// Catches a decoder that populates `integer_range` unconditionally: the
    /// plugin here reports a real-looking 0..127 range while declaring the
    /// fields invalid, so an ungated decode publishes a range the plugin never
    /// promised.
    #[test]
    fn integer_range_is_gated_on_its_flag() {
        let mut raw = raw_properties(0);
        raw.min_integer = 0;
        raw.max_integer = 127;
        raw.step_integer = 1;

        let ungated = ParameterProperties::decode(0, &raw);
        assert_eq!(ungated.integer_range, None);
        assert!(!ungated.flags.uses_int_step);

        raw.flags = api::ParameterFlags::USES_INT_STEP.bits();
        let gated = ParameterProperties::decode(0, &raw);
        assert_eq!(
            gated.integer_range,
            Some(IntegerRange {
                min: 0,
                max: 127,
                step: 1,
                large_step: 0,
            })
        );
    }

    /// Same gate, on the float-step and display-index groups.
    #[test]
    fn float_steps_and_display_index_are_gated_on_their_flags() {
        let mut raw = raw_properties(0);
        raw.step_float = 0.1;
        raw.small_step_float = 0.01;
        raw.large_step_float = 0.5;
        raw.display_index = 7;

        let ungated = ParameterProperties::decode(0, &raw);
        assert_eq!(ungated.float_steps, None);
        assert_eq!(ungated.display_index, None);

        raw.flags = (api::ParameterFlags::USES_FLOAT_STEP | api::ParameterFlags::USES_INDEX).bits();
        let gated = ParameterProperties::decode(0, &raw);
        assert_eq!(
            gated.float_steps,
            Some(FloatSteps {
                step: 0.1,
                small_step: 0.01,
                large_step: 0.5,
            })
        );
        assert_eq!(gated.display_index, Some(7));
    }

    /// A plugin that sets `USES_CATEGORY` but leaves the index at VST2's
    /// `0` = uncategorised must not be reported as belonging to "category 0".
    ///
    /// Catches a decoder that trusts the flag alone: every uncategorised
    /// parameter would then share one invented group, and a UI grouping by
    /// category renders them as a single bogus folder.
    #[test]
    fn category_zero_means_uncategorised_even_with_the_flag_set() {
        let mut raw = raw_properties(api::ParameterFlags::USES_CATEGORY.bits());
        raw.category = 0;
        raw.num_parameters_in_category = 4;

        assert_eq!(ParameterProperties::decode(0, &raw).category, None);

        // 1-based: the first real category is 1, and it must survive.
        raw.category = 1;
        raw.category_label = label("Osc");
        assert_eq!(
            ParameterProperties::decode(0, &raw).category,
            Some(ParameterCategory {
                index: 1,
                parameter_count: 4,
                label: "Osc".to_string(),
            })
        );
    }

    /// Labels stop at the first NUL and survive non-UTF-8 bytes.
    ///
    /// Catches a decoder that decodes the whole fixed field: the trailing NUL
    /// padding would arrive as embedded `\0` characters in the `String`, which
    /// render as boxes or truncate the label in a C-string-based UI toolkit.
    #[test]
    fn labels_stop_at_the_nul_and_tolerate_invalid_utf8() {
        let mut raw = raw_properties(0);
        raw.label = label("Cutoff");
        // A lone 0xFF is not valid UTF-8. Vendors do put Latin-1 bytes
        // (degree and copyright signs) in these fields; dropping the whole
        // name over one of them loses good data.
        raw.short_label[0] = b'd';
        raw.short_label[1] = 0xFF;
        raw.short_label[2] = 0;

        let decoded = ParameterProperties::decode(0, &raw);
        assert_eq!(decoded.label, "Cutoff");
        assert!(!decoded.label.contains('\0'));
        assert!(decoded.short_label.starts_with('d'));
        assert_eq!(decoded.short_label.chars().count(), 2);
    }

    /// A field that exactly fills its buffer has no room for a terminator.
    ///
    /// Catches a decode that requires a NUL: a 64-byte label would come back
    /// empty or truncated.
    #[test]
    fn a_label_filling_the_whole_field_is_not_truncated() {
        let mut raw = raw_properties(0);
        let full = "x".repeat(64);
        raw.label = label(&full);
        assert_eq!(ParameterProperties::decode(0, &raw).label, full);
    }

    /// Undefined flag bits must not invalidate the defined ones.
    ///
    /// Catches a `from_bits` that returns `None`/errors on unknown bits and a
    /// decoder that then discards the whole flags word — the plugin's real
    /// `USES_INT_STEP` declaration would be lost.
    #[test]
    fn undefined_flag_bits_are_ignored_not_fatal() {
        let raw = raw_properties(api::ParameterFlags::USES_INT_STEP.bits() | (1 << 20));
        let decoded = ParameterProperties::decode(0, &raw);
        assert!(decoded.flags.uses_int_step);
        assert!(decoded.integer_range.is_some());
        assert!(!decoded.flags.uses_category);
    }

    /// `step_count` must refuse the three plugin answers that break naive
    /// arithmetic, and be right on a normal one.
    ///
    /// Catches a `(max - min) / step` written without guards: a zero step
    /// divides by zero (panic in debug), and the full-i32 range overflows the
    /// subtraction.
    #[test]
    fn step_count_refuses_unusable_ranges() {
        let usable = IntegerRange {
            min: 0,
            max: 127,
            step: 1,
            large_step: 12,
        };
        assert_eq!(usable.step_count(), Some(127));

        let coarse = IntegerRange {
            min: 0,
            max: 100,
            step: 25,
            large_step: 50,
        };
        assert_eq!(coarse.step_count(), Some(4));

        let zero_step = IntegerRange {
            min: 0,
            max: 127,
            step: 0,
            large_step: 0,
        };
        assert_eq!(zero_step.step_count(), None);

        let inverted = IntegerRange {
            min: 127,
            max: 0,
            step: 1,
            large_step: 0,
        };
        assert_eq!(inverted.step_count(), None);

        // `max - min` here is 4294967295, which overflows i32.
        let overflowing = IntegerRange {
            min: i32::MIN,
            max: i32::MAX,
            step: 1,
            large_step: 0,
        };
        assert_eq!(overflowing.step_count(), Some(u32::MAX));
    }

    /// VST2's `255` bank sentinel must decode to absence, not to the number.
    ///
    /// Catches a passthrough decode: emitting CC 0 with value 255 truncates to
    /// 127 on the wire and selects a real, wrong bank.
    #[test]
    fn the_unused_bank_sentinel_decodes_to_absence() {
        assert_eq!(decode_bank(0, 0), Some((0, 0)));
        assert_eq!(decode_bank(1, 2), Some((1, 2)));
        assert_eq!(decode_bank(255, 0), None);
        assert_eq!(decode_bank(0, 255), None);
        assert_eq!(decode_bank(255, 255), None);
    }

    /// A negative parent-category index is absence, never a real index.
    ///
    /// Catches a decoder that passes `-1` through: a caller indexing a category
    /// list with it panics, or with `as usize` reads at `usize::MAX`.
    #[test]
    fn negative_parent_category_is_absence() {
        assert_eq!(decode_parent_category(-1), None);
        assert_eq!(decode_parent_category(-99), None);
        assert_eq!(decode_parent_category(0), Some(0));
        assert_eq!(decode_parent_category(3), Some(3));
    }

    /// The drum-kit flag drives whether a host queries key names at all, so it
    /// must be decoded from the right bit.
    #[test]
    fn the_drum_kit_flag_decodes_from_the_program_flags() {
        let mut raw = api::MidiProgramName {
            this_program_index: 2,
            name: label("Acoustic Kit"),
            midi_program: 32,
            midi_bank_msb: 1,
            midi_bank_lsb: 0,
            reserved: 0,
            parent_category_index: -1,
            flags: 0,
        };

        let melodic = MidiProgram::decode(2, &raw);
        assert!(!melodic.is_drum_kit);
        assert_eq!(melodic.name, "Acoustic Kit");
        assert_eq!(melodic.midi_program, 32);
        assert_eq!(melodic.bank, Some((1, 0)));
        assert_eq!(melodic.parent_category, None);

        raw.flags = api::MidiProgramFlags::IS_OMNI.bits();
        assert!(MidiProgram::decode(2, &raw).is_drum_kit);
    }

    /// Key names decode their name and carry back the coordinates queried, so
    /// a caller can match an answer to its question.
    #[test]
    fn key_names_carry_their_coordinates() {
        let raw = api::MidiKeyName {
            this_program_index: 0,
            this_key_number: 36,
            keyname: label("Kick"),
            reserved: 0,
            flags: 0,
        };
        let decoded = MidiKeyName::decode(0, 36, &raw);
        assert_eq!(decoded.name, "Kick");
        assert_eq!(decoded.key_number, 36);
        assert_eq!(decoded.program_index, 0);
    }
}
