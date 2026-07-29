//! Conformance tests for the VST2 host, driven by the in-repo reference
//! plugin (`tutti-vst2-test-plugin`).
//!
//! # These were dead for the life of the crate
//!
//! Every test here used to be `#[ignore]`d against
//! `/Library/Audio/Plug-Ins/VST/TAL-NoiseMaker.vst` — a macOS path, on a Linux
//! host, for a commercial plugin nobody had installed. Thirteen tests reported
//! as a clean suite while executing nothing, and four host bugs lived
//! underneath them. They now load the probe, which is built from this tree by a
//! dev-dependency edge, and they assert.
//!
//! # What the probe buys that a real plugin cannot
//!
//! A commercial synth can only be observed. The probe can be *told to
//! misbehave* — to answer `effCanDo` with an explicit `-1`, to publish no
//! editor, to expose no chunk — which is what turns "the host did not crash"
//! into "the host read the plugin's answer correctly". Every question in the
//! bug class this suite exists for ("is a refusal being read as a success?")
//! needs a plugin that will actually refuse.
//!
//! # Serialization and the env-var seam
//!
//! `ProbeConfig::from_env()` runs inside `Plugin::new`, i.e. during
//! `Vst2Instance::load`, so a test can reshape the plugin by setting
//! `TUTTI_VST2_PROBE_*` immediately before loading. `std::env::set_var` is
//! process-global and `cargo test` runs test fns on parallel threads, so every
//! test that touches the environment — and every test that reads the probe's
//! single process-global capture — holds [`PROBE_LOCK`] across the whole
//! set→load→assert sequence. Tests that do neither still take it, because the
//! probe's switches are global too.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

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

/// Every `TUTTI_VST2_PROBE_*` key a test in this file sets.
///
/// Listed explicitly rather than scanned from the environment: a test that
/// leaks one variable silently reshapes every plugin loaded afterwards in the
/// same process, and the resulting failure appears in an unrelated test. This
/// is the "sticky env var" hazard `switches.rs` documents.
const PROBE_ENV_KEYS: &[&str] = &[
    "TUTTI_VST2_PROBE_EDITOR",
    "TUTTI_VST2_PROBE_IS_SYNTH",
    "TUTTI_VST2_PROBE_NO_CHUNKS",
    "TUTTI_VST2_PROBE_PARAMS",
    "TUTTI_VST2_PROBE_SERVICED_PARAMS",
    "TUTTI_VST2_PROBE_MIDI_INPUTS",
    "TUTTI_VST2_PROBE_MIDI_OUTPUTS",
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
/// The linked rlib is a *different* image with its own statics; only the
/// cdylib's globals see the host's calls. `dlopen` on the same path returns a
/// handle to the already-loaded image, so this reaches the right ones.
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

/// `CanDoAnswer` discriminants, mirrored from the probe's `switches.rs`.
/// Kept as named constants so the call sites read as the spec's three answers
/// rather than as bare integers.
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
// Bug 1 — `has_editor` must ask the plugin
// ---------------------------------------------------------------------------

/// A plugin that publishes no GUI must report `has_editor == false`.
///
/// This is the whole of Bug 1. `Vst2Handle::new` calls `get_editor()` once and
/// stores the `Option`; the fork's `get_editor` used to consult only its own
/// `is_editor_active` flag and return `Some` on the first call for *every*
/// plugin, never reading `effFlagsHasEditor`. So `metadata().has_editor` was a
/// constant `true` — a DAW advertised an "open editor" affordance for every
/// VST2 it scanned.
///
/// The probe's `get_editor` returns `None` unless `TUTTI_VST2_PROBE_EDITOR` is
/// set, and `vst::main` keys `effFlagsHasEditor` off exactly that, so this
/// loads a genuinely editor-less plugin rather than a mocked one.
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

/// The converse, so the fix cannot be "always report false".
///
/// Without this, `has_editor() -> false` would pass the test above, and the
/// suite would have swapped a constant `true` for a constant `false`.
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

/// The downstream consequence of Bug 1: `open_editor`'s "Plugin has no editor"
/// path was unreachable.
///
/// With `has_editor` a constant `true`, `handle.editor` was always `Some`, so
/// the `ok_or_else` in `open_editor` never fired. The host instead dispatched
/// `effEditOpen` at a plugin with no editor and reported the resulting failure
/// as the *plugin* refusing — a different and more confusing error than the
/// truth, which is that there was never anything to open.
#[test]
fn opening_an_editor_on_an_editorless_plugin_says_so() {
    let _guard = lock_probe();
    let mut instance = load_probe_with(&[]);

    // SAFETY: a null parent is never dereferenced, because the correct host
    // rejects this call before dispatching `effEditOpen` — which is precisely
    // the property under test. Should the fix regress, the host would dispatch
    // with a null parent and the probe's editor (which refuses to open) would
    // still return cleanly, so this cannot crash the suite either way.
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
// Bug 4 — an explicit `canDo == -1` must beat the category inference
// ---------------------------------------------------------------------------

/// A synth that explicitly answers `-1` to `receiveVstMidiEvent` must not be
/// classified as receiving MIDI.
///
/// VST 2.4's `effCanDo` has three answers: `1` yes, `0` don't-know, `-1`
/// **explicitly no**. The host used to collapse `No` and `Maybe` into one falsy
/// bucket and then OR the result with `category == Synth`, so an explicit
/// refusal had no way to win: the plugin said "I do not receive MIDI" and the
/// host recorded that it did.
///
/// `Category::Synth` is exactly the term that made the OR unbeatable, which is
/// why this fixture is a synth.
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

/// `Maybe` (`0`, "don't know") must keep deferring to the category.
///
/// The fix must not over-rotate: a plugin that simply does not answer is the
/// common case the OR was written for, and MIDI-effect plugins declaring zero
/// MIDI pins depend on it. This pins the tolerance the fix preserves.
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
///
/// Real plugins have returned integers outside `{-1, 0, 1}`. A host that tests
/// "non-zero, therefore yes" turns `-1` into a capability — the exact shape of
/// the bug class. `Custom` is treated as "did not answer", so a plain effect
/// stays audio-only.
#[test]
fn an_undocumented_can_do_answer_is_not_an_affirmative() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_switches(&path);
    set_can_do(&path, can_do::CUSTOM, 42);

    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK).expect("probe should load");
    assert!(
        !instance.metadata().receives_midi,
        "effCanDo returned an undocumented 42; a plain Effect must not be \
         promoted to a MIDI receiver by a value the spec does not define"
    );

    drop(instance);
    reset_switches(&path);
}

// ---------------------------------------------------------------------------
// Bugs 2 & 3 — state save / restore must not paper over refusals
// ---------------------------------------------------------------------------

/// The happy path: a chunk-capable plugin round-trips its own blob.
///
/// The probe stores whatever `effSetChunk` hands it and returns it from
/// `effGetChunk`, so this asserts the host framed and unframed the payload
/// correctly rather than that any particular plugin state survived.
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

/// Bug 3: a plugin advertising `effFlagsProgramChunks` whose chunk save
/// *fails* must produce an error, not a silent parameter snapshot.
///
/// `copy_chunk` used to fold `len == 0` and `len == -1` into the same empty
/// `Vec`, so `save_state` could not distinguish "nothing saved" from "save
/// failed" and treated both as "fall through to parameters" — discarding
/// exactly the non-parameter state the chunk mechanism exists to carry, with
/// no error and nothing logged.
///
/// The fork now reports the two separately (`ChunkError::Failed` vs
/// `Ok(empty)`), which is asserted directly in `vst-tutti`'s unit tests
/// (`failed_chunk_save_is_distinguishable_from_an_empty_one`). Here we pin the
/// *other* half of that split at the host level: an empty chunk is a legitimate
/// "nothing saved yet" and must still fall back rather than erroring.
///
/// The probe seeds its chunk non-empty at construction, so the empty state has
/// to be established explicitly — restoring a one-byte chunk and then emptying
/// it is not possible through `load_state` (which rejects an empty `CHK\0`
/// payload as malformed framing), so the plugin is instead loaded without chunk
/// support at all. Both routes reach the same host branch: `save_state` falls
/// through to the parameter snapshot without erroring.
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

// Bug 2's core assertion — that a plugin's chunk *refusal* is representable at
// all, and therefore that `load_state` can report it — lives in `vst-tutti`'s
// own unit tests (`a_chunk_refusal_is_representable`), because constructing a
// refusing plugin needs the `PluginParameters` trait in scope and the shared
// probe deliberately accepts every chunk. The tests below cover the host-side
// half: the framing, the fallback, and the acceptance path.

/// Restoring a chunk into a plugin that does not advertise chunk support still
/// goes through the plugin and reports what it answered.
#[test]
fn restoring_a_chunk_into_a_chunkless_plugin_reports_the_plugin_answer() {
    let _guard = lock_probe();
    let instance = load_probe_with(&[("TUTTI_VST2_PROBE_NO_CHUNKS", "1")]);

    let mut chunk = b"CHK\0".to_vec();
    chunk.extend_from_slice(b"state-for-a-plugin-that-cannot-take-it");

    // The probe's `load_preset_data` is unconditional, so it accepts. What
    // matters is that the host relayed the plugin's answer rather than
    // inventing one; the refusal direction is pinned above, where a plugin that
    // actually refuses can be constructed.
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
/// `numParams` is read raw off the `AEffect` with no clamp, and `save_state`
/// computed `(param_count as usize) * 4` inside `Vec::with_capacity` — a
/// negative `i32` became a request for ~16 EiB and aborted the process. The
/// other AEffect counts (`numInputs`, `numOutputs`, `initialDelay`) were
/// already clamped where they are consumed; this one was not.
///
/// The loop underneath (`0..param_count`) was always harmless for a negative
/// count — it simply does not run — so the crash was entirely in the capacity
/// hint, and the correct save is an empty parameter snapshot.
///
/// `NO_CHUNKS` is required to reach that code at all: the probe seeds a
/// non-empty chunk, and a chunk-capable plugin returns from `save_state` before
/// the parameter path. Without it this test would pass while never executing
/// the line under test.
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
///
/// Asserting exact samples rather than "something non-zero happened" is what
/// makes this catch a host that swaps channels or renders the wrong block.
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

    let note_on = [MidiEvent::note_on(0, 1, 60, 100)];
    render_block(&mut instance, &mut scratch, &note_on);

    let note_off = [MidiEvent::note_off(0, 1, 60, 0)];
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
        .map(|i| MidiEvent::note_on(0, 1, 60 + i as u8, 100).with_frame_offset(i))
        .collect();
    render_block(&mut instance, &mut scratch, &events);

    let note_offs: Vec<MidiEvent> = (0..10)
        .map(|i| MidiEvent::note_off(0, 1, 60 + i as u8, 0))
        .collect();
    render_block(&mut instance, &mut scratch, &note_offs);
}

#[test]
fn load_nonexistent_path() {
    let result = Vst2Instance::load(Path::new("/nonexistent/plugin.vst"), SAMPLE_RATE, BLOCK);
    assert!(result.is_err());
}
