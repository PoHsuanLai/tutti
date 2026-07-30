//! `effGetParameterProperties` + MIDI-metadata conformance, driven through the
//! real `AEffect` FFI by the in-repo reference plugin.
//!
//! # Why these go through the probe and not a real plugin
//!
//! Every VST2 plugin installed on the development machine declines all six
//! opcodes. Measured directly, by dispatching to each plugin's `AEffect` and
//! reading back the buffer:
//!
//! | plugin          | numParams | numPrograms | opcode 56 | 62 | 63 | 64 | 65 | 66 |
//! |-----------------|-----------|-------------|-----------|----|----|----|----|----|
//! | TAL-NoiseMaker  | 88 (synth)| 1           | 0 (all)   | 0  | -1 | 0  | 0  | 0  |
//! | TAL-Reverb-4    | 20        | 1           | 0 (all)   | 0  | -1 | 0  | 0  | 0  |
//! | TDR Nova        | 75        | 73          | 0 (all)   | 0  | -1 | 0  | 0  | 0  |
//!
//! All three report `effGetApiVersion` = 2400, so they are full VST 2.4
//! plugins that simply do not implement the optional opcodes. A test written
//! against them could only ever assert "returns None", which is worth exactly
//! one test — [`real_world_plugins_decline_these_opcodes`] below is that test,
//! written against the probe's *default* (declining) behaviour so it runs
//! everywhere rather than only on this machine.
//!
//! Everything else needs a plugin that answers, so the probe answers, behind
//! switches that are off by default. That keeps the probe's default behaviour
//! matching the measured real-world one while making the decode path reachable.
//!
//! The `-1` in the `63` column is the reason the host compares against the
//! spec'd success value rather than testing `!= 0`: a `!= 0` test reads all
//! three of these plugins as *successfully* reporting a program.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use tutti_vst2_host::Vst2Instance;

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 44_100.0;
const BLOCK: usize = 512;

/// Serializes the probe's process-global switches across load→assert. Shared
/// image, shared statics — same reason `integration_tests.rs` has one.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Call one of the probe's `#[no_mangle]` switch functions.
///
/// Goes through `dlopen` rather than the linked rlib: the rlib is a separate
/// image with separate statics, and only the cdylib's are the ones the host
/// dispatches against. Same mechanism as `integration_tests.rs`.
fn probe_call<F, R>(path: &Path, symbol: &[u8], f: F) -> R
where
    F: FnOnce(libloading::Symbol<'_, *mut std::ffi::c_void>) -> R,
{
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }
        .unwrap_or_else(|e| panic!("re-open reference plugin at {path:?}: {e}"));
    // SAFETY: the symbol names are the probe's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(symbol) }.unwrap_or_else(|e| {
            panic!(
                "probe missing symbol {}: {e}",
                String::from_utf8_lossy(symbol)
            )
        });
    f(sym)
}

fn reset_switches(path: &Path) {
    probe_call(path, b"tutti_vst2_probe_reset_switches\0", |sym| {
        // SAFETY: the probe exports this as `extern "C" fn()`.
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
}

/// Make the probe answer `effGetParameterProperties`.
fn set_answer_param_properties(path: &Path, enable: bool) {
    probe_call(
        path,
        b"tutti_vst2_probe_set_answer_param_properties\0",
        |sym| {
            // SAFETY: the probe exports this as `extern "C" fn(bool)`.
            let f: extern "C" fn(bool) = unsafe { std::mem::transmute(*sym) };
            f(enable);
        },
    );
}

/// Make the probe answer the MIDI-metadata family, advertising
/// `serviced_programs` programs.
fn set_answer_midi_metadata(path: &Path, enable: bool, serviced_programs: i32) {
    probe_call(
        path,
        b"tutti_vst2_probe_set_answer_midi_metadata\0",
        |sym| {
            // SAFETY: the probe exports this as `extern "C" fn(bool, i32)`.
            let f: extern "C" fn(bool, i32) = unsafe { std::mem::transmute(*sym) };
            f(enable, serviced_programs);
        },
    );
}

/// Load the probe with every switch at its default (declining) state.
fn load_probe() -> (Vst2Instance, std::path::PathBuf) {
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK)
        .unwrap_or_else(|e| panic!("host failed to load reference plugin at {path:?}: {e:?}"));
    (instance, path)
}

/// The probe's own constants, so an expectation cannot drift from what the
/// plugin writes.
use tutti_vst2_test_plugin::{
    PROBE_INT_RANGE, PROBE_INT_STEP_PARAM, PROBE_KEY_NAME, PROBE_MIDI_PROGRAM, PROBE_NAMED_KEY,
    PROBE_PARAM_CATEGORY,
};

/// The default probe — like every real plugin measured — declines all six
/// opcodes, and the host reports that as absence rather than as zeroed data.
///
/// This is the only test that matches what the installed plugins actually do,
/// which is why it exists: the decode tests below all require a plugin that
/// answers, and none is available here.
///
/// Mutation that catches it: making `parameter_properties` return
/// `Some(decoded)` unconditionally (dropping the `supported != 1` check in
/// `vst-tutti`'s `parameter_properties`) turns every `None` here into a `Some`
/// full of zeros — exactly the bug of reading an unwritten buffer.
#[test]
fn real_world_plugins_decline_these_opcodes() {
    let _guard = lock_probe();
    let (instance, _path) = load_probe();

    let count = instance.parameters().len();
    assert!(
        count > 0,
        "probe must declare parameters for this test to mean anything"
    );

    for id in 0..count as u32 {
        assert_eq!(
            instance.parameter_properties(id),
            None,
            "a declining plugin must report absence, not zeroed properties, for param {id}"
        );
    }

    // Index-aligned listing: every entry present, every one None.
    let all = instance.all_parameter_properties();
    assert_eq!(all.len(), count);
    assert!(all.iter().all(Option::is_none));

    // The whole MIDI family declines too.
    assert_eq!(instance.midi_program(0, 0), None);
    assert_eq!(instance.current_midi_program(0), None);
    assert_eq!(instance.midi_program_category(0, 0), None);
    assert!(!instance.midi_programs_changed(0));
    assert_eq!(instance.midi_key_name(0, 0, PROBE_NAMED_KEY), None);
    assert!(instance.midi_programs(0).is_empty());
    assert!(instance.midi_key_names(0, 0).is_empty());
}

/// When a plugin *does* answer, every field arrives at the right offset with
/// the right value, across the real FFI boundary.
///
/// The numbers are the probe's, not invented here: a wrong `#[repr(C)]` field
/// order makes the plugin write `flags` where the host reads `min_integer`, and
/// this is what detects it end to end rather than only in the offset unit test.
///
/// Mutation that catches it: swapping `min_integer` and `max_integer` in
/// `api::ParameterProperties` — `size_of` is unchanged and the offset test and
/// this one both fail, reporting min=127/max=0.
#[test]
fn an_answering_plugin_round_trips_every_property_field() {
    let _guard = lock_probe();
    let (instance, path) = load_probe();
    set_answer_param_properties(&path, true);

    let props = instance
        .parameter_properties(PROBE_INT_STEP_PARAM as u32)
        .expect("probe answers once the switch is on");

    let (min, max, step, large_step) = PROBE_INT_RANGE;
    let range = props
        .integer_range
        .expect("probe declares USES_INT_STEP for this parameter");
    assert_eq!(range.min, min);
    assert_eq!(range.max, max);
    assert_eq!(range.step, step);
    assert_eq!(range.large_step, large_step);
    // Derived, not asserted independently of the range above.
    assert_eq!(range.step_count(), Some(((max - min) / step) as u32));

    let category = props.category.expect("probe declares USES_CATEGORY");
    assert_eq!(category.index, PROBE_PARAM_CATEGORY as u16);
    assert_eq!(category.label, "Filter");
    assert_eq!(category.parameter_count, 2);

    assert_eq!(props.display_index, Some(3));

    // Labels come back NUL-trimmed, not padded to the fixed field width.
    assert_eq!(props.label, "Probe Param 1");
    assert_eq!(props.short_label, "P1");
    assert!(!props.label.contains('\0'));

    reset_switches(&path);
}

/// A parameter whose plugin sets only `USES_FLOAT_STEP` must not surface an
/// integer range — even though the plugin left non-zero integer fields there.
///
/// The probe deliberately writes `min_integer = 999, max_integer = -999` on
/// these parameters: values a host cannot mistake for plausible, and that a
/// decode ignoring the flag gate would publish as an inverted range.
///
/// Mutation that catches it: replacing
/// `flags.uses_int_step.then_some(..)` with an unconditional `Some(..)` in
/// `ParameterProperties::decode` surfaces `min=999, max=-999`.
#[test]
fn ungated_integer_fields_are_not_reported_as_a_range() {
    let _guard = lock_probe();
    let (instance, path) = load_probe();
    set_answer_param_properties(&path, true);

    // Parameter 0 is not the int-step one, so the probe reports float steps
    // only while leaving garbage in the integer fields.
    let props = instance
        .parameter_properties(0)
        .expect("probe answers for every parameter");

    assert!(!props.flags.uses_int_step);
    assert_eq!(
        props.integer_range, None,
        "the plugin left 999/-999 in the integer fields and did NOT declare \
         them valid; publishing them is the bug"
    );
    assert!(props.flags.uses_float_step);
    let steps = props.float_steps.expect("USES_FLOAT_STEP is declared");
    assert_eq!(steps.step, 0.25);
    assert_eq!(steps.small_step, 0.05);
    assert_eq!(steps.large_step, 0.5);
    // Not declared, so not reported, despite the plugin's category field.
    assert_eq!(props.category, None);
    assert_eq!(props.display_index, None);

    reset_switches(&path);
}

/// An out-of-range parameter index must be refused by the host, not dispatched.
///
/// VST2 plugins are not required to bounds-check the index, so a host that
/// forwards `numParams + 1` invites the plugin to read past its own table.
///
/// Mutation that catches it: deleting the `id >= count` guard in
/// `Vst2Instance::parameter_properties` — the probe answers `1` for any index,
/// so the host would report properties for a parameter that does not exist.
#[test]
fn an_out_of_range_parameter_index_is_refused_before_dispatch() {
    let _guard = lock_probe();
    let (instance, path) = load_probe();
    set_answer_param_properties(&path, true);

    let count = instance.parameters().len() as u32;
    assert!(instance.parameter_properties(count - 1).is_some());
    assert_eq!(instance.parameter_properties(count), None);
    assert_eq!(instance.parameter_properties(count + 100), None);
    assert_eq!(instance.parameter_properties(u32::MAX), None);

    reset_switches(&path);
}

/// The MIDI-program family decodes name, program number, bank pair and the
/// drum-kit flag across the FFI.
///
/// Mutation that catches it: reordering `midi_bank_msb`/`midi_bank_lsb` in
/// `api::MidiProgramName` swaps the reported pair; dropping the
/// `IS_OMNI` decode makes `is_drum_kit` false for program 1.
#[test]
fn midi_program_metadata_round_trips() {
    let _guard = lock_probe();
    let (instance, path) = load_probe();
    set_answer_midi_metadata(&path, true, 2);

    let (program, serviced) = instance
        .midi_program(0, 0)
        .expect("probe answers once the switch is on");
    assert_eq!(serviced, 2);

    let (base_program, msb, lsb) = PROBE_MIDI_PROGRAM;
    assert_eq!(program.name, "Probe Program 0");
    assert_eq!(program.midi_program, base_program);
    assert_eq!(program.bank, Some((msb, lsb)));
    // The probe reports -1, which must decode to absence.
    assert_eq!(program.parent_category, None);
    assert!(
        !program.is_drum_kit,
        "program 0 is melodic; the flag must vary per program"
    );

    // Program 1 is the drum kit, and its program-change number advances —
    // proving the host wrote `this_program_index` before dispatching rather
    // than describing program 0 every time.
    let (drum_kit, _) = instance.midi_program(0, 1).expect("program 1 is serviced");
    assert_eq!(drum_kit.name, "Probe Program 1");
    assert_eq!(drum_kit.midi_program, base_program + 1);
    assert!(drum_kit.is_drum_kit);

    reset_switches(&path);
}

/// `effGetCurrentMidiProgram` returns the program *index*, not a boolean, and
/// index 0 is a valid answer.
///
/// This is the trap the real-plugin measurement exposed from the other side:
/// all three installed plugins answer `-1` here, so a host testing `!= 0` reads
/// a refusal as success — and, symmetrically, reads a genuine "program 0" as
/// failure.
///
/// Mutation that catches it: changing the `current < 0` check in `vst-tutti`'s
/// `current_midi_program` to `current == 0` (the "0 means unsupported" reflex)
/// turns the probe's valid program 0 into `None`.
#[test]
fn current_midi_program_zero_is_a_valid_answer_not_a_failure() {
    let _guard = lock_probe();
    let (instance, path) = load_probe();
    set_answer_midi_metadata(&path, true, 2);

    let current = instance
        .current_midi_program(0)
        .expect("program index 0 is a successful answer, not a refusal");
    assert_eq!(current.index, 0);
    assert_eq!(current.name, "Probe Program 0");

    reset_switches(&path);
}

/// A plugin that does NOT implement `effGetCurrentMidiProgram` must not be
/// reported as sitting on a nameless program 0.
///
/// This is a **host bug this test caught**. The opcode returns a program
/// *index*, so the obvious `current < 0` guard accepts `0` — but `0` is also
/// what an unimplemented opcode returns after falling through the plugin's
/// dispatcher, with the buffer still holding the host's own zeros. The first
/// version of the host reported
/// `Some(MidiProgram { index: 0, name: "", bank: Some((0, 0)), .. })` for the
/// declining probe: an invented program, with an invented bank-select pair of
/// (0, 0) that a caller could have emitted as a real bank change.
///
/// The fix gates on `effGetMidiProgramName`, whose zero is unambiguous. The
/// measured plugins answer `-1` and so would have passed a `-1`-only guard,
/// which is why the probe — falling through to vst-rs's `0` — was needed to
/// expose it.
///
/// Mutation that catches it: removing the `self.midi_program(channel, 0)?`
/// gate from `current_midi_program` restores the invented program 0.
#[test]
fn a_declining_plugin_is_not_reported_as_sitting_on_program_zero() {
    let _guard = lock_probe();
    let (instance, path) = load_probe();

    // The declining probe answers this opcode with 0 — indistinguishable, by
    // return value alone, from "currently on program 0".
    assert_eq!(
        instance.current_midi_program(0),
        None,
        "a plugin that services no MIDI programs has no current program; \
         reporting one invents a nameless entry and a bogus (0, 0) bank"
    );

    // With the family switched on, index 0 must come back as real — proving the
    // gate rejects refusals without also rejecting genuine zeros.
    set_answer_midi_metadata(&path, true, 2);
    let current = instance
        .current_midi_program(0)
        .expect("an answering plugin's program 0 is real");
    assert_eq!(current.index, 0);
    assert!(!current.name.is_empty());

    reset_switches(&path);
}

/// A plugin advertising more programs than it will name must not yield blank
/// entries — the enumeration hole, on the MIDI-program axis.
///
/// The probe advertises 5 while naming 2. A host that trusts the advertised
/// count produces three programs with empty names, which render as blank rows
/// in a preset menu.
///
/// Mutation that catches it: replacing the `None => break` arm in
/// `midi_programs` with `None => programs.push(<default>)`, or walking
/// `0..serviced` and unwrapping — the length assertion below fails at 5.
#[test]
fn an_advertised_program_count_above_the_serviced_one_does_not_produce_blanks() {
    let _guard = lock_probe();
    let (instance, path) = load_probe();
    // Advertise 5, name 2.
    set_answer_midi_metadata(&path, true, 5);

    let (_, serviced) = instance.midi_program(0, 0).expect("program 0 is named");
    assert_eq!(serviced, 5, "the plugin advertises five");

    let programs = instance.midi_programs(0);
    assert_eq!(
        programs.len(),
        2,
        "only two are actually named; the other three must be dropped, not blanked"
    );
    assert!(
        programs.iter().all(|p| !p.name.is_empty()),
        "no entry may be a blank placeholder: {programs:?}"
    );

    reset_switches(&path);
}

/// Key names cover only the keys the plugin names; unnamed keys are omitted so
/// a caller can fall back to the note number.
///
/// Mutation that catches it: making `midi_key_name` return
/// `Some(<empty name>)` on a `0` return (i.e. dropping the `supported != 1`
/// check) yields 128 entries instead of 1.
#[test]
fn only_named_keys_are_reported() {
    let _guard = lock_probe();
    let (instance, path) = load_probe();
    set_answer_midi_metadata(&path, true, 2);

    let named = instance
        .midi_key_name(0, 0, PROBE_NAMED_KEY)
        .expect("the probe names this key");
    assert_eq!(named.name, PROBE_KEY_NAME);
    assert_eq!(named.key_number, PROBE_NAMED_KEY);

    // A key the plugin does not name is absent, not empty-named.
    assert_eq!(instance.midi_key_name(0, 0, PROBE_NAMED_KEY + 1), None);

    let all = instance.midi_key_names(0, 0);
    assert_eq!(
        all.len(),
        1,
        "exactly one key is named; the other 127 must be omitted: {all:?}"
    );
    assert_eq!(all[0].key_number, PROBE_NAMED_KEY);

    reset_switches(&path);
}

/// Out-of-range channels and keys are refused before dispatch.
///
/// MIDI 1.0 has 16 channels and 128 keys; a negative or oversized index is a
/// caller bug that must not reach the plugin, which is not required to
/// bounds-check it.
///
/// Mutation that catches it: deleting the range guards in
/// `Vst2Instance::midi_key_name` — the probe keys only on `this_key_number`,
/// so channel 99 would answer as happily as channel 0.
#[test]
fn out_of_range_midi_coordinates_are_refused() {
    let _guard = lock_probe();
    let (instance, path) = load_probe();
    set_answer_midi_metadata(&path, true, 2);

    // Channel 0 works, so the refusals below are about the range, not the
    // switch being off.
    assert!(instance.midi_key_name(0, 0, PROBE_NAMED_KEY).is_some());

    assert_eq!(instance.midi_key_name(16, 0, PROBE_NAMED_KEY), None);
    assert_eq!(instance.midi_key_name(-1, 0, PROBE_NAMED_KEY), None);
    assert_eq!(instance.midi_key_name(0, -1, PROBE_NAMED_KEY), None);
    assert_eq!(instance.midi_key_name(0, 0, 128), None);
    assert_eq!(instance.midi_key_name(0, 0, -1), None);

    assert_eq!(instance.midi_program(16, 0), None);
    assert_eq!(instance.midi_program(0, -1), None);
    assert_eq!(instance.current_midi_program(16), None);
    assert_eq!(instance.midi_program_category(16, 0), None);
    assert!(!instance.midi_programs_changed(16));

    reset_switches(&path);
}

/// `effHasMidiProgramsChanged` and `effGetMidiProgramCategory` reach the plugin
/// and their answers are decoded.
///
/// Mutation that catches it: changing `midi_programs_changed`'s `== 1` to
/// `== 0` inverts it, and dropping the category `serviced <= 0` guard would
/// report a category from a declining plugin.
#[test]
fn the_change_flag_and_category_query_reach_the_plugin() {
    let _guard = lock_probe();
    let (instance, path) = load_probe();

    // Default (declining) probe: no change signalled, no category.
    assert!(!instance.midi_programs_changed(0));
    assert_eq!(instance.midi_program_category(0, 0), None);

    set_answer_midi_metadata(&path, true, 2);
    assert!(
        instance.midi_programs_changed(0),
        "the answering probe returns 1"
    );

    let (category, serviced) = instance
        .midi_program_category(0, 0)
        .expect("the answering probe describes category 0");
    assert_eq!(serviced, 1);
    assert_eq!(category.name, "Probe Category");
    assert_eq!(category.parent_category, None);

    // A category the probe does not describe is absent.
    assert_eq!(instance.midi_program_category(0, 1), None);

    reset_switches(&path);
}

/// The switches must be genuinely load-bearing in both directions: the same
/// instance answers when they are on and declines when they are off.
///
/// Without this, a decode test could pass against a probe that always answers,
/// and the "real plugins decline" test could pass against one that never does —
/// each looking correct while proving nothing about the gate itself.
#[test]
fn the_probe_switches_gate_the_answers_both_ways() {
    let _guard = lock_probe();
    let (instance, path) = load_probe();

    assert_eq!(instance.parameter_properties(0), None, "off by default");

    set_answer_param_properties(&path, true);
    assert!(
        instance.parameter_properties(0).is_some(),
        "switching on must change the answer on the same live instance"
    );

    set_answer_param_properties(&path, false);
    assert_eq!(
        instance.parameter_properties(0),
        None,
        "switching off must restore the declining behaviour"
    );

    reset_switches(&path);
}
