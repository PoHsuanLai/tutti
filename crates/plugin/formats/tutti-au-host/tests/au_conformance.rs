//! Host-conformance harness — an in-process `auval` for *our host*.
//!
//! `auval` validates that a *plugin* obeys the AU contract. Nothing validated
//! the other half: that this **host** drives the plugin the way AudioToolbox
//! expects. These tests do that, against the Apple Audio Units that ship with
//! macOS (see `support/corpus.rs` for why those and not a reference plugin).
//!
//! What is asserted here is the host's side of the contract:
//!
//! - the `Loaded`/`Ready` typestate matches what the AU will actually accept
//! - parameter metadata is read faithfully and writes round-trip
//! - `ClassInfo` state save/restore actually restores
//! - render honours block-size bounds, `OutputIsSilence`, and channel geometry
//! - instruments — which have **no input bus** — can be hosted at all
//!
//! That last one is not a hypothetical. Every AU instrument failed
//! `initialize` with `-10877` before this suite existed, because the host
//! installed an input render callback on a unit with no input element. The
//! whole instrument path was dead and nothing noticed, because the only AU
//! test was an effect-only no-alloc check. See
//! `instrument_with_no_input_bus_initializes`.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_conformance
//! ```
//!
//! No env vars, no SDK, no display: the corpus is part of macOS. A missing
//! unit **fails** rather than skipping — see `support/corpus.rs`.

#![cfg(target_os = "macos")]

use std::sync::Mutex;

mod support;
use support::corpus::{
    all_finite, impulse, peak, render, silence, DELAY, DLS_SYNTH, DYNAMICS, EFFECTS, INSTRUMENTS,
    LOWPASS, N_BAND_EQ, SAMPLER,
};

use tutti_au_host::component::AuType;
use tutti_au_host::types::K_AUDIO_UNIT_ERR_UNINITIALIZED;
use tutti_au_host::AuError;

/// AudioToolbox tolerates concurrent use of *distinct* units, but component
/// discovery walks a process-global registry and several tests here open the
/// same unit. Serializing keeps one test's instantiate/dispose from racing
/// another's enumeration. Mirrors `PLUGIN_LOAD_LOCK` in
/// `au_process_no_alloc.rs` and `PLUGIN_LOCK` in the VST3 suite.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// `AU_LOCK` is poisoned by any panicking test, and a poisoned lock would
/// convert one real failure into N spurious ones. The guard is only a
/// serializer — there is no shared state to be left inconsistent — so recover.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

// ---------------------------------------------------------------- lifecycle

/// The typestate must agree with reality: rendering is refused until
/// `AudioUnitInitialize` has run, because the AU has no render resources
/// allocated before that and `AudioUnitRender` would fail inside the plugin.
#[test]
fn render_before_initialize_is_refused() {
    let _g = lock();
    let mut au = DELAY.open_uninitialized(RATE, BLOCK);
    assert!(
        !au.is_initialized(),
        "a freshly instantiated AU must not claim to be initialized"
    );

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    let err = render(&mut au, &input, &mut output, BLOCK)
        .expect_err("process() in the Loaded state must be an error, not silence");
    // Assert the exact OSStatus, not a substring of the Debug output: the host
    // reports the AU's own `kAudioUnitErr_Uninitialized` rather than inventing
    // an error, so the caller sees what AudioToolbox would have produced.
    assert!(
        matches!(
            err,
            AuError::OsStatus {
                code: K_AUDIO_UNIT_ERR_UNINITIALIZED,
                ..
            }
        ),
        "expected kAudioUnitErr_Uninitialized ({K_AUDIO_UNIT_ERR_UNINITIALIZED}), \
         got {err:?}"
    );
}

/// `initialize`/`uninitialize` are documented as idempotent no-ops when already
/// in the target state. A host that toggles bypass or changes rate leans on
/// that; a double-initialize that reached `AudioUnitInitialize` twice would
/// leak the AU's render resources.
#[test]
fn initialize_and_uninitialize_are_idempotent() {
    let _g = lock();
    let mut au = DELAY.open_uninitialized(RATE, BLOCK);

    au.initialize().expect("first initialize");
    assert!(au.is_initialized());
    au.initialize().expect("second initialize must be a no-op");
    assert!(au.is_initialized());

    au.uninitialize().expect("first uninitialize");
    assert!(!au.is_initialized());
    au.uninitialize()
        .expect("second uninitialize must be a no-op");
    assert!(!au.is_initialized());

    // And the cycle is re-enterable: back to Ready, and actually rendering.
    au.initialize().expect("re-initialize after uninitialize");
    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("render after an init/uninit/init cycle");
}

/// An AU instrument has **no input bus**. Installing an input render callback
/// on its input scope is a property error (`-10877`), and the host used to do
/// exactly that — gating the install on the render scratch's input buffer
/// count, which `RenderScratch::new` over-allocates to `max(in, out)` so a
/// 0-in/2-out instrument still reported 2 input buffers.
///
/// The result: `initialize` failed for every instrument on the system, so the
/// host could not load a single AU synth. This asserts the gate now keys off
/// the AU's own `has_input`.
#[test]
fn instrument_with_no_input_bus_initializes() {
    let _g = lock();
    for unit in INSTRUMENTS {
        let mut au = unit.open_uninitialized(RATE, BLOCK);
        assert_eq!(
            au.num_inputs(),
            0,
            "{}: an instrument must report no input channels",
            unit.label
        );
        assert!(
            au.num_outputs() >= 1,
            "{}: an instrument must have an output bus",
            unit.label
        );
        au.initialize().unwrap_or_else(|e| {
            panic!(
                "{}: initialize failed: {e:?} — an instrument has no input \
                 element, so the host must not set a render callback on the \
                 input scope",
                unit.label
            )
        });
        assert!(au.is_initialized(), "{}", unit.label);
    }
}

/// The type classification drives MIDI routing, so it has to survive the trip
/// through `AudioComponentGetDescription` and the fourcc mapping.
#[test]
fn component_type_classification_matches_the_registry() {
    let _g = lock();
    for unit in EFFECTS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        assert_eq!(au.au_type(), AuType::Effect, "{}", unit.label);
        assert!(
            !au.au_type().receives_midi(),
            "{}: an effect must not be routed MIDI",
            unit.label
        );
    }
    for unit in INSTRUMENTS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        assert_eq!(au.au_type(), AuType::Instrument, "{}", unit.label);
        assert!(
            au.au_type().receives_midi(),
            "{}: an instrument must be routed MIDI",
            unit.label
        );
    }
}

// --------------------------------------------------------------- parameters

/// Parameter metadata is what a generic UI renders from, so every field has to
/// be read faithfully: a range that does not contain its own default, or a
/// non-finite bound, produces a control the user cannot operate.
#[test]
fn parameter_metadata_is_self_consistent() {
    let _g = lock();
    for unit in EFFECTS {
        let au = unit.open(RATE, BLOCK);
        let params = au.get_parameter_list();
        assert!(
            !params.is_empty(),
            "{}: every effect in the corpus exposes parameters",
            unit.label
        );

        for p in &params {
            let ctx = format!("{} param {} ({:?})", unit.label, p.id, p.name);
            assert!(
                p.range.min.is_finite() && p.range.max.is_finite(),
                "{ctx}: bounds must be finite, got [{}, {}]",
                p.range.min,
                p.range.max
            );
            assert!(
                p.range.min <= p.range.max,
                "{ctx}: min {} exceeds max {}",
                p.range.min,
                p.range.max
            );
            assert!(
                p.range.default >= p.range.min && p.range.default <= p.range.max,
                "{ctx}: default {} lies outside [{}, {}]",
                p.range.default,
                p.range.min,
                p.range.max
            );
            assert!(!p.name.is_empty(), "{ctx}: name must not be empty");
        }
    }
}

/// Parameter ids must be unique — they are the key a host stores in a preset
/// and replays on load. A duplicate id silently overwrites.
#[test]
fn parameter_ids_are_unique() {
    let _g = lock();
    for unit in EFFECTS {
        let au = unit.open(RATE, BLOCK);
        let params = au.get_parameter_list();
        let mut ids: Vec<u32> = params.iter().map(|p| p.id).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            before,
            "{}: parameter ids contain duplicates",
            unit.label
        );
    }
}

/// A write must be observable by the matching read. This is the round-trip a
/// host performs on every automation frame; if it does not hold, automation
/// silently does nothing.
#[test]
fn parameter_writes_round_trip() {
    let _g = lock();
    for unit in EFFECTS {
        let mut au = unit.open(RATE, BLOCK);
        let params = au.get_parameter_list();

        for p in params.iter().filter(|p| p.writable) {
            // Drive to both ends of the range rather than the midpoint: a host
            // that clamps or ignores writes still looks correct at the value
            // the parameter already held.
            for target in [p.range.min, p.range.max] {
                au.set_parameter(p.id, target)
                    .unwrap_or_else(|e| panic!("{} param {}: set failed {e:?}", unit.label, p.id));
                let got = au
                    .get_parameter(p.id)
                    .unwrap_or_else(|e| panic!("{} param {}: get failed {e:?}", unit.label, p.id));
                // Tolerance is relative to the range, since these span 1e-4
                // (reverb size, in seconds) to 2.4e4 (EQ frequency, in hertz).
                let tolerance = ((p.range.max - p.range.min).abs() * 1e-4).max(1e-4);
                assert!(
                    (got - target).abs() <= tolerance,
                    "{} param {} ({:?}): wrote {target}, read back {got} \
                     (tolerance {tolerance})",
                    unit.label,
                    p.id,
                    p.name
                );
            }
        }
    }
}

/// Reading a parameter the AU never declared must fail rather than return a
/// plausible-looking zero — a host that trusts a fabricated value writes it
/// into the user's preset.
#[test]
fn unknown_parameter_id_is_an_error() {
    let _g = lock();
    let au = LOWPASS.open(RATE, BLOCK);
    let declared: Vec<u32> = au.get_parameter_list().iter().map(|p| p.id).collect();
    let unknown = (0u32..10_000)
        .find(|id| !declared.contains(id))
        .expect("some id in 0..10000 is undeclared");
    assert!(
        au.get_parameter(unknown).is_err(),
        "reading undeclared parameter {unknown} must be an error, not a value"
    );
}

// -------------------------------------------------------------------- state

/// `ClassInfo` save/restore is what a project file stores. The test mutates
/// *away* from the saved value before restoring, so a no-op `load_state` — the
/// failure mode where the blob is accepted and ignored — cannot pass.
#[test]
fn state_save_restore_round_trips() {
    let _g = lock();
    for unit in EFFECTS {
        let mut au = unit.open(RATE, BLOCK);
        // Not `continue`: every corpus effect has writable parameters (AUDelay
        // 4/4, AUNBandEQ 41/41, AULowpass 2/2, AUDynamicsProcessor 7/10), so
        // skipping one would mean the corpus changed under us — and a loop that
        // skipped all four would report `ok` having asserted nothing, which is
        // the silent pass this suite exists to refuse.
        let param = au
            .get_parameter_list()
            .into_iter()
            .find(|p| p.writable)
            .unwrap_or_else(|| {
                panic!(
                    "{}: no writable parameter, so the state round-trip cannot \
                     be proven — see support/corpus.rs on why absence is loud",
                    unit.label
                )
            });

        let original = au.get_parameter(param.id).expect("read original");
        let blob = au
            .save_state()
            .unwrap_or_else(|e| panic!("{}: save_state failed {e:?}", unit.label));
        assert!(
            !blob.is_empty(),
            "{}: save_state produced an empty blob",
            unit.label
        );

        // Move to a value that is definitely different from the saved one.
        let moved = if (original - param.range.max).abs() > 1e-6 {
            param.range.max
        } else {
            param.range.min
        };
        au.set_parameter(param.id, moved).expect("mutate");
        let after_mutate = au.get_parameter(param.id).expect("read mutated");
        assert!(
            (after_mutate - original).abs() > 1e-6,
            "{}: could not move parameter {} away from {original}; the test \
             would be vacuous",
            unit.label,
            param.id
        );

        au.load_state(&blob)
            .unwrap_or_else(|e| panic!("{}: load_state failed {e:?}", unit.label));
        let restored = au.get_parameter(param.id).expect("read restored");
        let tolerance = ((param.range.max - param.range.min).abs() * 1e-4).max(1e-4);
        assert!(
            (restored - original).abs() <= tolerance,
            "{} param {}: state restored {restored}, expected the saved \
             {original} (had moved it to {moved})",
            unit.label,
            param.id
        );
    }
}

/// An empty blob is documented as a no-op. A host hits this with a project
/// saved before the plugin had state; it must not clobber the AU or error.
#[test]
fn loading_empty_state_is_a_noop() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    let param = au
        .get_parameter_list()
        .into_iter()
        .find(|p| p.writable)
        .expect("AUDelay has writable parameters");

    au.set_parameter(param.id, param.range.mid()).expect("set");
    let before = au.get_parameter(param.id).expect("read");
    au.load_state(&[]).expect("empty state must be accepted");
    let after = au.get_parameter(param.id).expect("read");
    assert_eq!(before, after, "an empty state blob must not disturb the AU");
}

/// A corrupt blob must be rejected, not fed to `ClassInfo` where it becomes
/// the plugin's problem. This is the untrusted-input path: project files get
/// truncated and hand-edited.
#[test]
fn corrupt_state_is_rejected() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    // Not a binary plist: no `bplist00` magic, no XML prologue.
    let garbage = vec![0xDEu8, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03];
    assert!(
        au.load_state(&garbage).is_err(),
        "a blob that is not a property list must be rejected"
    );
    // And the AU is still usable afterwards.
    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("AU still renders after a rejected state");
}

// ------------------------------------------------------------------- render

/// `num_frames` above the configured `block_size` must be refused. The AU
/// allocated its render buffers for `MaximumFramesPerSlice` at initialize
/// time, so rendering more is an out-of-bounds write inside the plugin — the
/// host must catch it rather than pass it through.
#[test]
fn oversized_block_is_refused() {
    let _g = lock();
    let mut au = DELAY.open(RATE, 64);
    let input = silence(2, 128);
    let mut output = silence(2, 128);
    assert!(
        render(&mut au, &input, &mut output, 128).is_err(),
        "a block larger than block_size must be refused"
    );
    // The exact boundary is legal.
    let input = silence(2, 64);
    let mut output = silence(2, 64);
    render(&mut au, &input, &mut output, 64).expect("a block of exactly block_size must render");
}

/// Silence in must give silence out for these units, and — the part that
/// actually catches bugs — the output must be *finite*. An uninitialized
/// scratch buffer or a mis-sized `AudioBufferList` shows up as NaN here long
/// before it is audible.
#[test]
fn silence_in_gives_finite_silence_out() {
    let _g = lock();
    for unit in EFFECTS {
        let mut au = unit.open(RATE, BLOCK);
        let input = silence(2, BLOCK as usize);
        let mut output = vec![vec![f32::NAN; BLOCK as usize]; 2];

        // Several blocks: a delay line only reveals stale scratch once its
        // buffer wraps, which the first block never reaches.
        for _ in 0..8 {
            render(&mut au, &input, &mut output, BLOCK)
                .unwrap_or_else(|e| panic!("{}: render failed {e:?}", unit.label));
            assert!(
                all_finite(&output),
                "{}: render produced a non-finite sample from silent input",
                unit.label
            );
        }
        assert!(
            peak(&output) < 1e-6,
            "{}: silent input produced output with peak {}",
            unit.label,
            peak(&output)
        );
    }
}

/// The host must write every frame it was asked for. Pre-filling the output
/// with a sentinel catches the off-by-one where the last frame is left
/// untouched — inaudible per block, but a periodic click at block rate.
#[test]
fn render_writes_every_requested_frame() {
    let _g = lock();
    let mut au = LOWPASS.open(RATE, BLOCK);
    const SENTINEL: f32 = -12_345.0;

    for frames in [1u32, 63, 64, 511, BLOCK] {
        let input = impulse(2, frames as usize);
        let mut output = vec![vec![SENTINEL; frames as usize]; 2];
        render(&mut au, &input, &mut output, frames).expect("render");
        for (ch, buf) in output.iter().enumerate() {
            for (i, &s) in buf.iter().enumerate() {
                assert_ne!(
                    s, SENTINEL,
                    "frames={frames}: channel {ch} frame {i} was never written"
                );
            }
        }
    }
}

/// A partial block — fewer frames than `block_size` — must leave the tail of
/// the caller's buffer alone. Writing past `num_frames` corrupts whatever the
/// host had staged there.
#[test]
fn partial_block_does_not_write_past_num_frames() {
    let _g = lock();
    let mut au = LOWPASS.open(RATE, BLOCK);
    const GUARD: f32 = 7.5;
    let frames = 100u32;

    let input = impulse(2, BLOCK as usize);
    let mut output = vec![vec![GUARD; BLOCK as usize]; 2];
    render(&mut au, &input, &mut output, frames).expect("render");

    for (ch, buf) in output.iter().enumerate() {
        for (i, &s) in buf.iter().enumerate().skip(frames as usize) {
            assert_eq!(
                s, GUARD,
                "channel {ch} frame {i} lies past num_frames={frames} but was \
                 overwritten"
            );
        }
    }
}

/// An effect that is passing signal must actually alter or forward it. This is
/// the "is the input reaching the plugin at all" check: the host stages input
/// through a render callback, and if that callback is not wired the AU renders
/// from silence and every effect outputs nothing.
///
/// AUMatrixReverb is deliberately excluded — at its default 100% wet with a
/// long pre-delay it genuinely produces silence for the first blocks, so it
/// would fail a signal-present assertion while behaving correctly.
#[test]
fn input_reaches_the_plugin() {
    let _g = lock();
    for unit in [DELAY, LOWPASS, N_BAND_EQ] {
        let mut au = unit.open(RATE, BLOCK);
        let input = impulse(2, BLOCK as usize);
        let mut output = silence(2, BLOCK as usize);
        render(&mut au, &input, &mut output, BLOCK)
            .unwrap_or_else(|e| panic!("{}: render failed {e:?}", unit.label));
        assert!(
            peak(&output) > 1e-4,
            "{}: an impulse produced a silent block (peak {}), so the input \
             never reached the AU",
            unit.label,
            peak(&output)
        );
    }
}

/// Latency is reported by the AU in *seconds* and converted to samples here.
/// AUDynamicsProcessor has a genuine 256-sample lookahead, so the conversion
/// has something real to land on — and because the AU scales its own seconds
/// value with the rate, the sample count is rate-invariant.
#[test]
fn reported_latency_is_the_plugins_own_in_samples() {
    let _g = lock();
    for rate in [44_100.0, 48_000.0, 96_000.0] {
        let au = DYNAMICS.open(rate, BLOCK);
        let latency = au.get_latency().expect("latency");
        assert_eq!(
            latency, 256,
            "AUDynamicsProcessor advertises a 256-sample lookahead; at {rate} \
             Hz the host reported {latency}"
        );
    }
    // A unit that advertises no latency must report 0, not a stale or
    // fabricated value.
    let au = DELAY.open(RATE, BLOCK);
    assert_eq!(au.get_latency().expect("latency"), 0);
}

/// Changing the sample rate must be reflected in what the host reports, and
/// the AU must still render afterwards — the host tears down and rebuilds the
/// render scratch across this transition.
#[test]
fn sample_rate_change_reconfigures_and_still_renders() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    assert_eq!(au.sample_rate(), RATE);

    for rate in [44_100.0, 96_000.0, 48_000.0] {
        au.set_sample_rate(rate)
            .unwrap_or_else(|e| panic!("set_sample_rate({rate}) failed: {e:?}"));
        assert_eq!(
            au.sample_rate(),
            rate,
            "the host must report the rate it actually applied"
        );
        assert!(
            au.is_initialized(),
            "the AU must be left initialized after a rate change"
        );

        let input = impulse(2, BLOCK as usize);
        let mut output = vec![vec![f32::NAN; BLOCK as usize]; 2];
        render(&mut au, &input, &mut output, BLOCK).expect("render after a rate change");
        assert!(
            all_finite(&output),
            "render after switching to {rate} Hz produced non-finite samples"
        );
    }
}

/// Channel geometry has to be stable and self-consistent: the counts the host
/// reports are what the caller sizes its buffers from, so a mismatch between
/// them and what render consumes is an out-of-bounds read.
#[test]
fn channel_counts_are_stable_across_initialize() {
    let _g = lock();
    for unit in EFFECTS {
        let mut au = unit.open_uninitialized(RATE, BLOCK);
        let (in_before, out_before) = (au.num_inputs(), au.num_outputs());
        au.initialize().expect("initialize");
        assert_eq!(
            (au.num_inputs(), au.num_outputs()),
            (in_before, out_before),
            "{}: channel counts changed across initialize",
            unit.label
        );
        assert!(
            out_before >= 1,
            "{}: an effect must have at least one output channel",
            unit.label
        );
    }
}

// --------------------------------------------------------------------- MIDI

/// An instrument must turn a NoteOn into audio. This exercises the whole MIDI
/// path — UMP decode, MIDI 2.0 → 1.0 velocity scaling, and delivery through
/// `MusicDeviceMIDIEvent` — and it is only reachable now that instruments
/// initialize at all.
#[test]
fn note_on_produces_audio_from_an_instrument() {
    let _g = lock();
    use tutti_midi_types::MidiEvent;

    for unit in INSTRUMENTS {
        let mut au = unit.open(RATE, BLOCK);
        au.send_midi(&[MidiEvent::note_on(0, 0, 60, 0xC000)]);

        // Instruments have no input bus, so render from an empty input.
        let input: Vec<Vec<f32>> = Vec::new();
        let mut loudest = 0.0f32;
        for _ in 0..20 {
            let mut output = silence(au.num_outputs() as usize, BLOCK as usize);
            render(&mut au, &input, &mut output, BLOCK)
                .unwrap_or_else(|e| panic!("{}: render failed {e:?}", unit.label));
            assert!(
                all_finite(&output),
                "{}: instrument produced a non-finite sample",
                unit.label
            );
            loudest = loudest.max(peak(&output));
        }
        assert!(
            loudest > 1e-3,
            "{}: a NoteOn produced no audio (peak {loudest}) across 20 blocks",
            unit.label
        );
    }
}

/// A NoteOff must silence the voice a NoteOn started. Without this, only the
/// "does anything come out" half of the MIDI path is covered — a host that
/// dropped note-offs would still pass the test above.
#[test]
fn note_off_silences_the_voice() {
    let _g = lock();
    use tutti_midi_types::MidiEvent;

    let mut au = DLS_SYNTH.open(RATE, BLOCK);
    let input: Vec<Vec<f32>> = Vec::new();
    let channels = au.num_outputs() as usize;

    au.send_midi(&[MidiEvent::note_on(0, 0, 60, 0xC000)]);
    let mut sounding = 0.0f32;
    for _ in 0..10 {
        let mut output = silence(channels, BLOCK as usize);
        render(&mut au, &input, &mut output, BLOCK).expect("render");
        sounding = sounding.max(peak(&output));
    }
    assert!(
        sounding > 1e-3,
        "the note never sounded, so the note-off assertion would be vacuous"
    );

    au.send_midi(&[MidiEvent::note_off(0, 0, 60, 0)]);
    // The DLS release is a long exponential, not a gate: measured on a quiet
    // machine it is still at ~5.9% of the held peak 60 blocks after the
    // note-off, 1.3% at 120, 0.15% at 240, and 0.002% at 480. So wait 240
    // blocks (2.56 s at 48 kHz) and assert against 1% — about 7x clear of the
    // measured value there, which is headroom for a different DLS release
    // envelope but not for a note-off that was dropped, since that stays near
    // 100%. Values are bit-identical across runs: this is offline block
    // rendering, so the margin cannot flake under CPU load.
    //
    // Every block is checked for finiteness as it goes. `peak` folds with
    // `f32::max`, which returns the NON-NaN operand, so an all-NaN buffer has a
    // peak of 0.0 — without this guard a host that corrupted the release tail
    // into NaN would read as a perfectly silenced voice and pass. This is the
    // one assertion in the suite that reads a *decaying* signal, so it is the
    // one where a quiet-looking result must be proven to be real silence.
    let render_block = |au: &mut _| -> f32 {
        let mut output = silence(channels, BLOCK as usize);
        render(au, &input, &mut output, BLOCK).expect("render");
        assert!(
            all_finite(&output),
            "the release tail contained a non-finite sample; `peak` would \
             report that as silence"
        );
        peak(&output)
    };

    for _ in 0..240 {
        render_block(&mut au);
    }
    let mut after = 0.0f32;
    for _ in 0..10 {
        after = after.max(render_block(&mut au));
    }
    assert!(
        after < sounding * 0.01,
        "after a NoteOff the voice still sounds at peak {after} (it was \
         {sounding} while held)"
    );
}

/// Sending MIDI to an effect must not crash or corrupt its audio. The host
/// gates MIDI routing on `receives_midi`, but `send_midi` is public and a
/// caller may reach it anyway — `MusicDeviceMIDIEvent` on a plain effect
/// returns an error per event, which the host is documented to swallow.
#[test]
fn midi_to_an_effect_is_harmless() {
    let _g = lock();
    use tutti_midi_types::MidiEvent;

    let mut au = DELAY.open(RATE, BLOCK);
    au.send_midi(&[
        MidiEvent::note_on(0, 0, 60, 0xC000),
        MidiEvent::cc(0, 0, 7, 0x4000_0000),
    ]);

    let input = impulse(2, BLOCK as usize);
    let mut output = vec![vec![f32::NAN; BLOCK as usize]; 2];
    render(&mut au, &input, &mut output, BLOCK).expect("an effect still renders after stray MIDI");
    assert!(all_finite(&output), "stray MIDI corrupted the output");
}

/// Messages with no legacy 3-byte form — SysEx, per-note MIDI 2.0 — are
/// documented as skipped rather than truncated into a wrong message. Feeding a
/// mixed block must deliver the representable events and drop the rest without
/// erroring.
#[test]
fn unrepresentable_midi_is_dropped_not_mangled() {
    let _g = lock();
    use tutti_midi_types::MidiEvent;

    let mut au = SAMPLER.open(RATE, BLOCK);
    let input: Vec<Vec<f32>> = Vec::new();
    let channels = au.num_outputs() as usize;

    // A per-note pitch bend has no legacy form; the note-on beside it does.
    au.send_midi(&[
        MidiEvent::per_note_pitch_bend(0, 0, 60, 0x4000_0000),
        MidiEvent::note_on(0, 0, 60, 0xC000),
    ]);

    let mut loudest = 0.0f32;
    for _ in 0..20 {
        let mut output = silence(channels, BLOCK as usize);
        render(&mut au, &input, &mut output, BLOCK).expect("render");
        assert!(all_finite(&output), "unrepresentable MIDI corrupted output");
        loudest = loudest.max(peak(&output));
    }
    assert!(
        loudest > 1e-3,
        "the representable note-on beside the dropped message never sounded"
    );
}
