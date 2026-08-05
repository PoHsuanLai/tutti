//! Conformance tests for the VST2 host, driven by the in-repo reference
//! plugin (`tutti-vst2-test-plugin`).
//!
//! The probe can be *told to misbehave* — answer `effCanDo` with an explicit
//! `-1`, publish no editor, expose no chunk — which is what turns "the host did
//! not crash" into "the host read the plugin's answer correctly".
//!
//! `ProbeConfig::from_env()` runs inside `Plugin::new`, i.e. during
//! `Vst2Instance::load`, so a test reshapes the plugin by setting
//! `TUTTI_VST2_PROBE_*` immediately before loading. `set_var` is process-global
//! and the probe's switches and capture are too, so every test holds
//! [`PROBE_LOCK`] across the whole set→load→assert sequence.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
use tutti_vst2_host::{
    ChannelLayout, MidiEvent, ProcessContext, RenderScratch, TimeSignature, TransportInfo,
    Vst2Instance,
};

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 44_100.0;
const BLOCK: usize = 512;

/// Serializes both the process-global `TUTTI_VST2_PROBE_*` environment and the
/// probe's process-global switches/capture. Held across set→load→assert.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Every `TUTTI_VST2_PROBE_*` key a test in this file sets. Listed explicitly
/// so `clear_probe_env` cannot miss one: a leaked variable reshapes every
/// plugin loaded afterwards, and the failure surfaces in an unrelated test.
const PROBE_ENV_KEYS: &[&str] = &[
    "TUTTI_VST2_PROBE_EDITOR",
    "TUTTI_VST2_PROBE_IS_SYNTH",
    "TUTTI_VST2_PROBE_NO_CHUNKS",
    "TUTTI_VST2_PROBE_PARAMS",
    "TUTTI_VST2_PROBE_SERVICED_PARAMS",
    "TUTTI_VST2_PROBE_MIDI_INPUTS",
    "TUTTI_VST2_PROBE_MIDI_OUTPUTS",
    "TUTTI_VST2_PROBE_EFFECT_NAME",
];

fn clear_probe_env() {
    for key in PROBE_ENV_KEYS {
        // SAFETY: callers hold `PROBE_LOCK`, so no other test thread is
        // reading or writing the environment concurrently.
        unsafe { std::env::remove_var(key) };
    }
}

fn set_probe_env(pairs: &[(&str, &str)]) {
    clear_probe_env();
    for (k, v) in pairs {
        // SAFETY: as above.
        unsafe { std::env::set_var(k, v) };
    }
}

/// Load the probe with the given `TUTTI_VST2_PROBE_*` overrides applied, then
/// clear them so the next load starts from the well-behaved defaults.
fn load_probe_with(env: &[(&str, &str)]) -> Vst2Instance {
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_probe_env(env);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK)
        .unwrap_or_else(|e| panic!("host failed to load reference plugin at {path:?}: {e:?}"));
    // The AEffect is already built; clearing now keeps the variable from
    // outliving this load.
    clear_probe_env();
    instance
}

/// Load the well-behaved probe.
fn load_probe() -> Vst2Instance {
    load_probe_with(&[])
}

/// Call one of the probe's exported `#[no_mangle]` control functions.
///
/// Must go through `dlopen`, not the linked rlib: the rlib is a *different*
/// image with its own statics, and only the cdylib's globals see the host's
/// calls. `dlopen` on the same path returns the already-loaded image.
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
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
}

/// Set the probe's `effCanDo` answer. `answer` is a `CanDoAnswer` discriminant.
fn set_can_do(path: &Path, answer: i32, custom: isize) {
    probe_call(path, b"tutti_vst2_probe_set_can_do\0", |sym| {
        let f: extern "C" fn(i32, isize) = unsafe { std::mem::transmute(*sym) };
        f(answer, custom);
    });
}

/// Make the probe accept (or keep refusing) `effSetBypass`.
fn set_accept_soft_bypass(path: &Path, enable: bool) {
    probe_call(path, b"tutti_vst2_probe_set_accept_soft_bypass\0", |sym| {
        let f: extern "C" fn(bool) = unsafe { std::mem::transmute(*sym) };
        f(enable);
    });
}

/// The last `effSetBypass` value the probe was sent: `-1` never, `0` resume,
/// `1` bypass.
fn last_bypass(path: &Path) -> i32 {
    probe_call(path, b"tutti_vst2_probe_last_bypass\0", |sym| {
        let f: extern "C" fn() -> i32 = unsafe { std::mem::transmute(*sym) };
        f()
    })
}

/// `CanDoAnswer` discriminants, mirrored from the probe's `switches.rs`.
mod can_do {
    pub const YES: i32 = 0;
    pub const MAYBE: i32 = 1;
    pub const NO: i32 = 2;
    pub const CUSTOM: i32 = 3;
}

fn render_block(
    instance: &mut Vst2Instance,
    scratch: &mut RenderScratch,
    midi: &[MidiEvent],
) -> Vec<Vec<f32>> {
    let meta = instance.metadata().clone();
    let inputs = (meta.num_inputs.count() as usize).max(1);
    let outputs = (meta.num_outputs.count() as usize).max(1);

    let input_data = vec![vec![0.0f32; BLOCK]; inputs];
    let mut output_data = vec![vec![0.0f32; BLOCK]; outputs];

    let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
    let mut output_slices: Vec<&mut [f32]> =
        output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

    let ctx = ProcessContext::new(SAMPLE_RATE).midi(midi);
    instance.process_f32(&input_slices, &mut output_slices, BLOCK, &ctx, scratch);

    output_data
}

// ---------------------------------------------------------------------------
// `has_editor` must ask the plugin
// ---------------------------------------------------------------------------

/// A plugin that publishes no GUI must report `has_editor == false`.
///
/// `has_editor` was a constant `true`: the fork's `get_editor` consulted only
/// its own `is_editor_active` flag and never read `effFlagsHasEditor`.
#[test]
fn editorless_plugin_reports_no_editor() {
    let _guard = lock_probe();
    let instance = load_probe_with(&[]);
    assert!(
        !instance.metadata().has_editor,
        "the probe publishes no editor (effFlagsHasEditor clear), but the host \
         reported has_editor = true — this is the constant-true bug: the host \
         is not asking the plugin"
    );
}

/// The converse, so a constant `false` cannot satisfy the test above.
#[test]
fn plugin_with_an_editor_reports_one() {
    let _guard = lock_probe();
    let instance = load_probe_with(&[("TUTTI_VST2_PROBE_EDITOR", "1")]);
    assert!(
        instance.metadata().has_editor,
        "the probe published an editor (effFlagsHasEditor set) but the host \
         reported has_editor = false"
    );
}

/// The downstream consequence: with `has_editor` constant-`true`,
/// `handle.editor` was always `Some`, so `open_editor`'s "no editor" branch
/// was unreachable and the host reported a dispatched-`effEditOpen` failure
/// instead — blaming the plugin for something that was never there to open.
#[test]
fn opening_an_editor_on_an_editorless_plugin_says_so() {
    let _guard = lock_probe();
    let mut instance = load_probe_with(&[]);

    // SAFETY: the null parent is never dereferenced. A correct host rejects
    // before dispatching `effEditOpen` (the property under test), and on a
    // regression the probe's editor refuses to open rather than touching it.
    let parent = unsafe { tutti_vst2_host::WindowHandle::from_raw(std::ptr::null_mut()) };

    let err = instance
        .open_editor(parent)
        .expect_err("a plugin with no editor must not report a successful open");

    let msg = err.to_string();
    assert!(
        msg.contains("no editor"),
        "expected the 'plugin has no editor' error, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// An explicit `canDo == -1` must beat the category inference
// ---------------------------------------------------------------------------

/// A synth that explicitly answers `-1` to `receiveVstMidiEvent` must not be
/// classified as receiving MIDI.
///
/// `effCanDo` has three answers — `1` yes, `0` don't-know, `-1` explicitly no.
/// The host collapsed `No` and `Maybe` into one falsy bucket and OR'd with
/// `category == Synth`, so a refusal could never win. The fixture is a synth
/// because `Category::Synth` is the term that made the OR unbeatable.
#[test]
fn an_explicit_can_do_refusal_beats_the_synth_category() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_can_do(&path, can_do::NO, 0);

    set_probe_env(&[("TUTTI_VST2_PROBE_IS_SYNTH", "1")]);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    clear_probe_env();

    let meta = instance.metadata();
    assert!(
        !meta.receives_midi,
        "the plugin answered effCanDo(receiveVstMidiEvent) = -1, an explicit \
         refusal, yet the host classified it as receiving MIDI because it \
         declared Category::Synth — the refusal must override the inference"
    );
    assert!(
        !meta.emits_midi,
        "same refusal on sendVstMidiEvent was ignored"
    );

    drop(instance);
    reset_switches(&path);
}

/// `Maybe` (`0`, "don't know") must keep deferring to the category — the
/// common case the OR was written for. Pins the tolerance the fix preserves.
#[test]
fn a_silent_can_do_still_defers_to_the_synth_category() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_can_do(&path, can_do::MAYBE, 0);

    set_probe_env(&[("TUTTI_VST2_PROBE_IS_SYNTH", "1")]);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    clear_probe_env();

    assert!(
        instance.metadata().receives_midi,
        "a plugin answering effCanDo = 0 (don't know) that declares \
         Category::Synth must still be treated as receiving MIDI — that \
         inference is what keeps MIDI-effect plugins from being misclassified"
    );

    drop(instance);
    reset_switches(&path);
}

/// `Yes` (`1`) asserts the capability even for a plain effect.
#[test]
fn an_affirmative_can_do_beats_the_effect_category() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_can_do(&path, can_do::YES, 0);

    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    assert!(
        instance.metadata().receives_midi,
        "the plugin answered effCanDo = 1 but the host ignored it"
    );

    drop(instance);
    reset_switches(&path);
}

/// An undocumented `Custom(n)` return must not be read as an affirmative.
/// Real plugins return integers outside `{-1, 0, 1}`, and a host testing
/// "non-zero, therefore yes" also turns `-1` into a capability.
///
/// The pins are forced to 0. `Custom` defers to the inference, so leaving the
/// probe's default of one declared MIDI input pin would make this pass on the
/// pin term no matter what the host did with `Custom` — the assertion would
/// hold for the wrong reason. "A plain Effect" has to mean a plugin with no
/// MIDI evidence at all, which is what these two overrides buy.
#[test]
fn an_undocumented_can_do_answer_is_not_an_affirmative() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_can_do(&path, can_do::CUSTOM, 42);

    set_probe_env(&[
        ("TUTTI_VST2_PROBE_MIDI_INPUTS", "0"),
        ("TUTTI_VST2_PROBE_MIDI_OUTPUTS", "0"),
    ]);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    clear_probe_env();

    assert!(
        !instance.metadata().receives_midi,
        "effCanDo returned an undocumented 42; a plain Effect must not be \
         promoted to a MIDI receiver by a value the spec does not define"
    );

    drop(instance);
    reset_switches(&path);
}

// ---------------------------------------------------------------------------
// MIDI pin counts come from the live opcodes
// ---------------------------------------------------------------------------

/// A plugin declaring MIDI output pins and answering `Maybe` to
/// `sendVstMidiEvent` emits MIDI.
///
/// This is the bug D-10 hid. `emits_midi`'s only inferred term was
/// `info.midi_outputs > 0`, and `get_info()` hardcodes that to 0 — so the term
/// was a dead `false`, `Maybe` resolved to `false`, and the plugin's MIDI
/// output was dropped. `Maybe` is the common answer: plugins that route MIDI
/// routinely declare pins and never implement `effCanDo`.
#[test]
fn a_maybe_answer_with_declared_output_pins_emits_midi() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_can_do(&path, can_do::MAYBE, 0);

    set_probe_env(&[("TUTTI_VST2_PROBE_MIDI_OUTPUTS", "2")]);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    clear_probe_env();

    assert!(
        instance.metadata().emits_midi,
        "the plugin declared 2 MIDI output channels and answered effCanDo = 0 \
         (don't know); the host must read the declaration and treat it as \
         emitting MIDI, or its MIDI output is silently dropped"
    );

    drop(instance);
    reset_switches(&path);
}

/// The same plugin with no output pins declared is not a MIDI source.
///
/// The negative half: without it, a host that hardcoded `emits_midi = true`
/// would pass the test above. `Category::Synth` is set to pin that an
/// instrument is not promoted to a MIDI *emitter* — it emits audio, and the
/// two are unrelated.
#[test]
fn a_maybe_answer_without_output_pins_does_not_emit_midi() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_can_do(&path, can_do::MAYBE, 0);

    set_probe_env(&[
        ("TUTTI_VST2_PROBE_MIDI_OUTPUTS", "0"),
        ("TUTTI_VST2_PROBE_IS_SYNTH", "1"),
    ]);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    clear_probe_env();

    assert!(
        !instance.metadata().emits_midi,
        "the plugin declared no MIDI output channels; being a Synth says it \
         emits audio, not MIDI, and must not promote it to a MIDI source"
    );

    drop(instance);
    reset_switches(&path);
}

/// Declared MIDI *input* pins make a plugin a MIDI receiver even when it is
/// not a synth and answers `Maybe`.
///
/// Pins the input half of the same read: before the opcodes were sent, the
/// only surviving inference for `receives_midi` was `Category::Synth`, so a
/// MIDI-driven effect that declared pins was classified as receiving nothing.
#[test]
fn declared_input_pins_make_a_non_synth_a_midi_receiver() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_can_do(&path, can_do::MAYBE, 0);

    set_probe_env(&[("TUTTI_VST2_PROBE_MIDI_INPUTS", "1")]);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    clear_probe_env();

    assert!(
        instance.metadata().receives_midi,
        "the plugin declared 1 MIDI input channel and is not a Synth; the \
         declaration is the evidence, and dropping it leaves a MIDI-driven \
         effect classified as receiving nothing"
    );

    drop(instance);
    reset_switches(&path);
}

/// A count outside the spec's `1..=15` is not an answer, and must not be read
/// as one.
///
/// Both opcodes return their count as the dispatcher's `isize` return value,
/// and an unimplemented opcode falls through returning `0` — so `0` cannot mean
/// "I use no MIDI channels", it means "I did not answer". The range is the only
/// thing separating the two. Without the filter a `0` becomes a real answer of
/// zero; the classification lands in the same place here, but the host has
/// stopped being able to tell a declined opcode from a declared absence, and
/// the next reader of `read_midi_channels` inherits the confusion.
#[test]
fn an_out_of_range_midi_count_is_not_an_answer() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);

    set_probe_env(&[
        ("TUTTI_VST2_PROBE_MIDI_INPUTS", "0"),
        ("TUTTI_VST2_PROBE_MIDI_OUTPUTS", "99"),
    ]);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    clear_probe_env();

    let counts = instance.midi_channel_counts();
    assert_eq!(
        counts.inputs, None,
        "0 is what an unimplemented opcode returns, so it must read as \
         'declined', not as a declared count of zero"
    );
    assert_eq!(
        counts.outputs, None,
        "99 is outside the spec's 1..=15; a value that was never a valid count \
         must not be carried through as one"
    );

    drop(instance);
    reset_switches(&path);
}

/// A count inside `1..=15` is carried through verbatim.
///
/// The positive half of the range check: without it, a filter that rejected
/// everything would satisfy the test above.
#[test]
fn an_in_range_midi_count_is_reported_verbatim() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);

    set_probe_env(&[
        ("TUTTI_VST2_PROBE_MIDI_INPUTS", "1"),
        ("TUTTI_VST2_PROBE_MIDI_OUTPUTS", "15"),
    ]);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    clear_probe_env();

    let counts = instance.midi_channel_counts();
    assert_eq!(counts.inputs, Some(1), "the boundary value 1 must be kept");
    assert_eq!(
        counts.outputs,
        Some(15),
        "the boundary value 15 must be kept"
    );

    drop(instance);
    reset_switches(&path);
}

/// An explicit `-1` still beats a declared pin.
///
/// The pin count is evidence for the `Maybe` case only. Wiring the opcodes up
/// added a second term to the same OR that `an_explicit_can_do_refusal_beats_
/// the_synth_category` guards, and a refusal has to outrank that one too.
#[test]
fn an_explicit_refusal_beats_declared_midi_pins() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_can_do(&path, can_do::NO, 0);

    set_probe_env(&[
        ("TUTTI_VST2_PROBE_MIDI_INPUTS", "4"),
        ("TUTTI_VST2_PROBE_MIDI_OUTPUTS", "4"),
    ]);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    clear_probe_env();

    let meta = instance.metadata();
    assert!(
        !meta.receives_midi && !meta.emits_midi,
        "the plugin declared 4 MIDI channels each way but answered effCanDo = \
         -1, an explicit refusal; the refusal must override the declaration"
    );

    drop(instance);
    reset_switches(&path);
}

// ---------------------------------------------------------------------------
// State save / restore must not paper over refusals
// ---------------------------------------------------------------------------

/// The happy path: a chunk-capable plugin round-trips its own blob.
///
/// The probe echoes whatever `effSetChunk` hands it, so this asserts the
/// host's framing, not that any particular plugin state survived.
#[test]
fn chunk_state_round_trips() {
    let _guard = lock_probe();
    let instance = load_probe();

    // Give the plugin a chunk to save by restoring one first — the probe's
    // chunk starts empty, and an empty chunk legitimately falls back to a
    // parameter snapshot.
    let mut seeded = b"CHK\0".to_vec();
    seeded.extend_from_slice(b"probe-state-v1");
    instance
        .load_state(&seeded)
        .expect("probe accepts any chunk");

    let saved = instance.save_state().expect("save_state should succeed");
    assert_eq!(
        &saved[..4],
        b"CHK\0",
        "a plugin advertising effFlagsProgramChunks with a non-empty chunk \
         must be saved as a chunk, not downgraded to a parameter snapshot"
    );
    assert_eq!(&saved[4..], b"probe-state-v1");

    instance.load_state(&saved).expect("restore should succeed");
}

/// The benign half of the `copy_chunk` split: "nothing saved" must still fall
/// back to a parameter snapshot rather than erroring. (`len == 0` and
/// `len == -1` used to fold into the same empty `Vec`, so a *failed* chunk save
/// silently became a parameter snapshot; the failure half is asserted in
/// `vst-tutti`'s `failed_chunk_save_is_distinguishable_from_an_empty_one`.)
///
/// Reached via `NO_CHUNKS` rather than an emptied chunk: the probe seeds its
/// chunk non-empty and `load_state` rejects an empty `CHK\0` payload as
/// malformed, so there is no route to an empty chunk on a chunk-capable
/// plugin. Both land on the same host branch.
#[test]
fn a_chunkless_plugin_falls_back_to_a_parameter_snapshot() {
    let _guard = lock_probe();
    let instance = load_probe_with(&[("TUTTI_VST2_PROBE_NO_CHUNKS", "1")]);

    let saved = instance
        .save_state()
        .expect("no chunk support is not a failure — fall back, don't error");
    assert_eq!(
        &saved[..4],
        b"PRM\0",
        "a plugin without effFlagsProgramChunks must be saved as a parameter \
         snapshot"
    );
}

// The refusal direction — that a chunk *refusal* is representable, so
// `load_state` can report it — is pinned in `vst-tutti`'s
// `a_chunk_refusal_is_representable`; the shared probe deliberately accepts
// every chunk, so it cannot be built here. These cover the host-side half:
// framing, fallback, and the acceptance path.

/// Restoring a chunk into a plugin that does not advertise chunk support still
/// goes through the plugin and reports what it answered.
#[test]
fn restoring_a_chunk_into_a_chunkless_plugin_reports_the_plugin_answer() {
    let _guard = lock_probe();
    let instance = load_probe_with(&[("TUTTI_VST2_PROBE_NO_CHUNKS", "1")]);

    let mut chunk = b"CHK\0".to_vec();
    chunk.extend_from_slice(b"state-for-a-plugin-that-cannot-take-it");

    // The probe's `load_preset_data` is unconditional, so it accepts; what is
    // asserted is that the host relayed that answer rather than inventing one.
    instance
        .load_state(&chunk)
        .expect("the probe accepts chunks, so the host must report success");
}

/// Parameter-snapshot save/restore, for a plugin with no chunk support.
#[test]
fn parameter_state_round_trips() {
    let _guard = lock_probe();
    let instance = load_probe_with(&[("TUTTI_VST2_PROBE_NO_CHUNKS", "1")]);

    assert!(instance.set_parameter(0, 0.25));
    assert!(instance.set_parameter(1, 0.75));

    let state = instance.save_state().expect("save_state should succeed");
    assert_eq!(&state[..4], b"PRM\0");

    assert!(instance.set_parameter(0, 0.9));
    assert!(instance.set_parameter(1, 0.1));

    instance
        .load_state(&state)
        .expect("load_state should succeed");

    assert!((instance.parameter(0).unwrap() - 0.25).abs() < 0.02);
    assert!((instance.parameter(1).unwrap() - 0.75).abs() < 0.02);
}

/// Malformed state blobs are rejected rather than half-applied.
#[test]
fn state_restore_rejects_malformed_blobs() {
    let _guard = lock_probe();
    let instance = load_probe();

    assert!(instance.load_state(&[]).is_err(), "empty blob");
    assert!(instance.load_state(&[0, 1, 2]).is_err(), "short header");
    assert!(instance.load_state(&[0xFF; 4]).is_err(), "unknown header");
    assert!(
        instance.load_state(b"CHK\0").is_err(),
        "empty chunk payload"
    );

    let mut bad = b"PRM\0".to_vec();
    bad.extend_from_slice(&2i32.to_le_bytes()); // claims 2 params
    bad.extend_from_slice(&0.5f32.to_le_bytes()); // only 1 value follows
    assert!(instance.load_state(&bad).is_err(), "count/length mismatch");
}

/// A plugin declaring a negative `numParams` must not take the host down.
///
/// `numParams` is read raw off the `AEffect`, and `save_state` fed
/// `(param_count as usize) * 4` to `Vec::with_capacity` — a negative `i32`
/// became a ~16 EiB request and aborted the process.
///
/// `NO_CHUNKS` is required to reach that line: the probe seeds a non-empty
/// chunk, and a chunk-capable plugin returns from `save_state` before the
/// parameter path. Without it this test passes while executing nothing.
#[test]
fn a_negative_parameter_count_does_not_abort_the_save() {
    let _guard = lock_probe();
    let instance = load_probe_with(&[
        ("TUTTI_VST2_PROBE_PARAMS", "-1"),
        ("TUTTI_VST2_PROBE_NO_CHUNKS", "1"),
    ]);

    let state = instance
        .save_state()
        .expect("a negative numParams should produce an empty snapshot, not an abort");

    assert_eq!(&state[..4], b"PRM\0");
    let count = i32::from_le_bytes(state[4..8].try_into().unwrap());
    assert_eq!(
        count, 0,
        "a negative count must be normalized to 0 in the blob, not written \
         through: `parse_state_header` rejects a negative count outright, so \
         writing -1 here would produce a blob this host cannot read back"
    );
    assert_eq!(
        state.len(),
        8,
        "no parameter values should follow a zero count"
    );

    // The blob must survive the round trip it just claimed to be.
    instance
        .load_state(&state)
        .expect("a state blob this host wrote must be one it can read");
}

// ---------------------------------------------------------------------------
// General host behaviour
// ---------------------------------------------------------------------------

#[test]
fn load_and_metadata() {
    let _guard = lock_probe();
    let instance = load_probe();
    let meta = instance.metadata();
    assert!(!meta.name.is_empty());
    assert!(!meta.id.is_empty());
    assert!(meta.num_outputs.count() > 0);
}

// ---------------------------------------------------------------------------
// Soft bypass
// ---------------------------------------------------------------------------

/// `effSetBypass` (44) reaches the plugin and its acceptance is reported.
///
/// Soft bypass is the plugin's own passthrough: it crossfades and flushes its
/// tail rather than having a reverb cut mid-decay. Without the opcode a host
/// can only hard-mute, which is audibly worse and is what this host was
/// limited to.
#[test]
fn an_accepted_soft_bypass_reaches_the_plugin() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_accept_soft_bypass(&path, true);

    let mut instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");

    assert!(
        instance.set_bypass(true),
        "the probe accepts effSetBypass but the host reported a refusal"
    );
    assert_eq!(
        last_bypass(&path),
        1,
        "the host must dispatch effSetBypass with value=1; -1 means it never \
         dispatched at all"
    );

    assert!(instance.set_bypass(false), "leaving bypass was refused");
    assert_eq!(
        last_bypass(&path),
        0,
        "leaving bypass must dispatch value=0, not repeat the 1"
    );

    drop(instance);
    reset_switches(&path);
}

/// A refusal is reported, not swallowed.
///
/// An unimplemented `effSetBypass` falls through the dispatcher returning 0 —
/// the same answer an explicit refusal gives — so the host cannot tell them
/// apart and must treat both as "did not take". A caller that ignores this
/// leaves the plugin processing while the UI shows it bypassed.
#[test]
fn a_refused_soft_bypass_is_reported() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_accept_soft_bypass(&path, false);

    let mut instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");

    assert!(
        !instance.set_bypass(true),
        "the probe refuses effSetBypass; reporting success would leave the \
         host believing a bypass took effect when the plugin is still \
         processing"
    );
    // The refusal is the plugin's, not a missing dispatch: the host did ask.
    assert_eq!(
        last_bypass(&path),
        1,
        "the host must still dispatch — a refusal is the plugin's answer, not \
         a reason to skip the opcode"
    );

    drop(instance);
    reset_switches(&path);
}

/// `effCanDo("bypass")` is what a host checks before relying on soft bypass.
///
/// A plugin that does not advertise one has to be bypassed by the host itself.
/// `Maybe` is the common answer and is not a yes — reading it as one is the
/// same three-valued mistake `effCanDo` invites everywhere else.
#[test]
fn soft_bypass_support_is_advertised_not_assumed() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();

    reset_switches(&path);
    set_can_do(&path, can_do::YES, 0);
    let advertising = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    assert!(
        advertising.supports_soft_bypass(),
        "the probe answered effCanDo(bypass) = 1 and the host ignored it"
    );
    drop(advertising);

    reset_switches(&path);
    set_can_do(&path, can_do::MAYBE, 0);
    let silent = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    assert!(
        !silent.supports_soft_bypass(),
        "effCanDo = 0 is 'don't know', not a yes; a host that treats it as one \
         relies on a bypass the plugin never promised"
    );
    drop(silent);

    reset_switches(&path);
}

/// `effGetEffectName` (45) is preferred over `effGetProductString` (48).
///
/// The two name different things: the effect name is the plugin's own, the
/// product string names the product it ships in — one string shared by every
/// plugin in a bundled suite. Reading only the product string collapses a
/// suite to a single label, so a host that asks for one must ask for the
/// other first.
#[test]
fn the_effect_name_is_preferred_over_the_product_string() {
    let _guard = lock_probe();
    let instance = load_probe_with(&[("TUTTI_VST2_PROBE_EFFECT_NAME", "Probe Effect 45")]);

    assert_eq!(
        instance.metadata().name,
        "Probe Effect 45",
        "the plugin answered effGetEffectName with its own name; the host \
         reported the product string instead, which every plugin in a suite \
         shares"
    );
}

/// A plugin that declines `effGetEffectName` still gets named.
///
/// The opcode is optional and reports no failure — an unimplemented one falls
/// through the dispatcher leaving the buffer zero-filled, so an empty string
/// is the only "did not answer" available. Without the fallback, the majority
/// of real plugins (which implement `effGetProductString` and not this) would
/// load with an empty name.
#[test]
fn declining_the_effect_name_falls_back_to_the_product_string() {
    let _guard = lock_probe();
    let instance = load_probe();

    assert_eq!(
        instance.metadata().name,
        tutti_vst2_test_plugin::PROBE_NAME,
        "the probe declines effGetEffectName; the host must fall back to \
         effGetProductString rather than reporting the empty buffer"
    );
}

/// `default_value` reports the plugin's load-time state, not its live value.
///
/// VST2 has no default-value opcode — none of the 61 in `OpCode` returns one,
/// and `effGetParameterProperties` carries a range but no default. So the only
/// observable default is the plugin's initial state, snapshotted at load.
///
/// This is what the field used to get wrong: it was filled from the *current*
/// value, so after a user turned a knob the "default" followed it, and a
/// reset-to-default control would have been a no-op that looked broken.
///
/// The probe starts parameter `i` at `i/16`, so the defaults here are distinct
/// and non-zero — a mapping that returned `0.0` or echoed the live value fails.
#[test]
fn default_value_is_the_load_time_state_not_the_live_value() {
    let _guard = lock_probe();
    let instance = load_probe();

    let before = instance.parameter_list();
    let defaults: Vec<f64> = before.iter().map(|p| p.range.default_value()).collect();
    assert!(
        defaults.iter().any(|&d| d != 0.0),
        "every default came back 0.0, so this test could not detect the bug \
         it exists for; the probe should start parameter i at i/16"
    );

    // Move every parameter well away from where it started.
    for p in &before {
        let index = p.id.index().expect("VST2 addresses parameters by index");
        assert!(instance.set_parameter(index, 0.9));
    }

    let after = instance.parameter_list();
    for (i, p) in after.iter().enumerate() {
        assert_eq!(
            p.range.default_value(),
            defaults[i],
            "param {} reported default {} after the value moved to 0.9; the \
             default must not follow the live value",
            p.id,
            p.range.default_value()
        );
    }
    // And the live value really did move, so the assertion above is not
    // passing because the writes were dropped.
    assert!(
        (instance.parameter(0).unwrap() - 0.9).abs() < 0.02,
        "the set_parameter writes did not land, so nothing was proven"
    );
}

#[test]
fn parameter_count_and_names() {
    let _guard = lock_probe();
    let instance = load_probe();
    let params = instance.parameters();
    assert_eq!(params.len(), 4, "the probe declares four parameters");
    for param in &params {
        assert!(!param.name.is_empty(), "param {} has empty name", param.id);
    }
}

#[test]
fn parameter_set_get_roundtrip() {
    let _guard = lock_probe();
    let instance = load_probe();

    let read = |i| {
        instance
            .parameter(i)
            .expect("the probe must expose getParameter")
    };
    let write = |i, v| {
        assert!(
            instance.set_parameter(i, v),
            "the probe must expose setParameter"
        );
    };

    write(0, 0.5);
    assert!((read(0) - 0.5).abs() < 0.01);
    write(0, 0.0);
    assert!(read(0).abs() < 0.01);
    write(0, 1.0);
    assert!((read(0) - 1.0).abs() < 0.01);
}

#[test]
fn parameter_info_lookup() {
    let _guard = lock_probe();
    let instance = load_probe();
    let info = instance.parameter_info(0).expect("param 0 should exist");
    assert_eq!(info.id, 0);
    assert!(!info.name.is_empty());
    assert!(
        instance.parameter_info(99_999).is_none(),
        "an out-of-range index must not be serviced"
    );
}

/// The probe's tag oracle: `out[ch][i] == in[ch][i] + channel_tag(ch)`.
/// Exact samples, not "something non-zero happened" — that is what catches a
/// host that swaps channels or renders the wrong block.
#[test]
fn process_delivers_the_expected_audio() {
    let _guard = lock_probe();
    let mut instance = load_probe();
    let meta = instance.metadata().clone();
    let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, BLOCK);

    let out = render_block(&mut instance, &mut scratch, &[]);

    // Silent input, so every output sample is exactly the channel tag.
    for (ch, samples) in out.iter().enumerate() {
        let expected = tutti_vst2_test_plugin::channel_tag(ch);
        for (i, &s) in samples.iter().enumerate() {
            assert_eq!(
                s, expected,
                "channel {ch} sample {i}: expected the channel tag {expected}"
            );
        }
    }
}

#[test]
fn process_forwards_midi_without_panicking() {
    let _guard = lock_probe();
    let mut instance = load_probe();
    let meta = instance.metadata().clone();
    let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, BLOCK);

    let note_on = [MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::new(1),
        60,
        100,
    )];
    render_block(&mut instance, &mut scratch, &note_on);

    let note_off = [MidiEvent::note_off(
        MidiGroup::FIRST,
        MidiChannel::new(1),
        60,
        0,
    )];
    render_block(&mut instance, &mut scratch, &note_off);
}

#[test]
fn process_with_transport() {
    let _guard = lock_probe();
    let mut instance = load_probe();
    let meta = instance.metadata().clone();
    let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, BLOCK);

    let transport = TransportInfo::new()
        .with_playing(true)
        .with_position_quarters(2.0)
        .with_position_samples(44_100)
        .with_tempo(120.0)
        .with_time_signature(TimeSignature::default())
        .with_loop(true, 0.0, 4.0);

    let input_data = vec![vec![0.0f32; BLOCK]; meta.num_inputs.count() as usize];
    let mut output_data = vec![vec![0.0f32; BLOCK]; meta.num_outputs.count() as usize];

    let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
    let mut output_slices: Vec<&mut [f32]> =
        output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

    let ctx = ProcessContext::new(SAMPLE_RATE).transport(&transport);
    instance.process_f32(&input_slices, &mut output_slices, BLOCK, &ctx, &mut scratch);
}

#[test]
fn process_empty_buffer() {
    let _guard = lock_probe();
    let mut instance = load_probe();
    let mut scratch =
        RenderScratch::new(ChannelLayout::from(0u16), ChannelLayout::from(0u16), BLOCK);
    let input_slices: Vec<&[f32]> = vec![];
    let mut output_slices: Vec<&mut [f32]> = vec![];
    let ctx = ProcessContext::new(SAMPLE_RATE);
    instance.process_f32(&input_slices, &mut output_slices, 0, &ctx, &mut scratch);
}

#[test]
fn many_midi_events_in_one_block() {
    let _guard = lock_probe();
    let mut instance = load_probe();
    let meta = instance.metadata().clone();
    let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, BLOCK);

    let events: Vec<MidiEvent> = (0..10u32)
        .map(|i| {
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 60 + i as u8, 100)
                .with_frame_offset(i)
        })
        .collect();
    render_block(&mut instance, &mut scratch, &events);

    let note_offs: Vec<MidiEvent> = (0..10)
        .map(|i| MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(1), 60 + i as u8, 0))
        .collect();
    render_block(&mut instance, &mut scratch, &note_offs);
}

#[test]
fn load_nonexistent_path() {
    let result = Vst2Instance::load(Path::new("/nonexistent/plugin.vst"), SAMPLE_RATE, BLOCK);
    assert!(result.is_err());
}
