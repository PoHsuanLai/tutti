//! Conformance for the two host APIs that reach past parameters: factory
//! presets and effect bypass.
//!
//! These are the halves of AU hosting that a parameter-only suite cannot see.
//! Both are *optional* AU properties, and both fail in the same quiet way: the
//! property read errors, the host absorbs it, and the DAW shows a preset list
//! or a bypass button that does nothing. So what is asserted here is not merely
//! "the call returns `Ok`" but that the AU's own state moved:
//!
//! - preset **counts** are pinned per unit, so a truncated `CFArray` walk fails
//!   rather than merely returning fewer presets
//! - loading a preset is proved non-trivial by mutating parameters away first
//!   and watching them come back
//! - a rejected preset number is an error, and the AU still renders after one
//! - bypass is proved against **rendered audio**, not against the property
//!   round-trip, because an AU that accepted the property and kept processing
//!   would pass a round-trip check
//!
//! ## The CoreFoundation ownership half
//!
//! `kAudioUnitProperty_FactoryPresets` hands back a **copied** `CFArrayRef` the
//! host owns, whose elements are `AUPreset` structs holding `CFStringRef`s the
//! host does **not** own. Getting either half wrong is invisible in a single
//! call: a leak shows up as memory growth, an over-release corrupts the AU's
//! own preset table and crashes on some *later* enumeration. So
//! `enumerating_presets_repeatedly_is_stable` runs the enumeration 200 times
//! and a `PresentPreset` read 500 times — both counts chosen to be far past
//! anything a single-shot test would exercise, so an ownership error has room
//! to become a crash instead of staying latent.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_presets_bypass
//! ```
//!
//! Measurements in the assertions below were taken on macOS 15.6 (Apple
//! silicon) with a scratch probe; each threshold cites the number it came from.
//! As in `au_conformance.rs`, a missing unit **fails** rather than skipping.

#![cfg(target_os = "macos")]

use std::sync::Mutex;

mod support;
use support::corpus::{
    all_finite, impulse, peak, render, silence, DISTORTION, INSTRUMENTS, PRESETLESS_EFFECTS,
    PRESET_EFFECTS,
};

use tutti_au_host::AuError;

/// Serializes these tests against each other and, in spirit, against the rest
/// of the crate's suites: component discovery walks a process-global registry
/// and several tests here open the same unit. Mirrors `AU_LOCK` in
/// `au_conformance.rs` — a separate `static` because each test binary links its
/// own copy, and cargo runs the binaries as separate processes anyway.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// `AU_LOCK` is poisoned by any panicking test, and a poisoned lock would
/// convert one real failure into N spurious ones. The guard is only a
/// serializer — there is no shared state to be left inconsistent — so recover.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

// ------------------------------------------------------------ preset listing

/// The enumeration must recover *every* preset the AU advertises, with usable
/// names and distinct selectors.
///
/// The count is pinned per unit rather than merely checked non-empty because
/// the realistic bug is a short walk — an off-by-one on `CFArrayGetCount`, or
/// entries dropped by the `filter_map` on a null element — and any
/// `assert!(!presets.is_empty())` would sail past all of those. Measured on
/// macOS 15.6: AUDistortion 22, AUMatrixReverb 13, AUReverb2 13,
/// AUDynamicsProcessor 6.
///
/// Uniqueness matters because `number` is the AU's own selector, not a position:
/// duplicate numbers would mean two rows of a preset menu that load the same
/// thing, and the duplicate is exactly what a mis-strided array walk produces.
#[test]
fn a_unit_with_presets_enumerates_all_of_them() {
    let _g = lock();
    for (unit, expected) in PRESET_EFFECTS {
        let au = unit.open(RATE, BLOCK);
        let presets = au.factory_presets();
        assert_eq!(
            presets.len(),
            *expected,
            "{} advertised {} factory presets, expected {expected} — a changed \
             count means either the AU shipped a different table or the CFArray \
             walk is truncating",
            unit.label,
            presets.len()
        );

        for p in &presets {
            assert!(
                !p.name.is_empty(),
                "{}: preset {} has an empty name. An empty name is what a \
                 mis-read CFStringRef produces, and it is also unusable in a \
                 preset menu.",
                unit.label,
                p.number
            );
        }

        let mut numbers: Vec<i32> = presets.iter().map(|p| p.number).collect();
        numbers.sort_unstable();
        let unique = {
            let mut n = numbers.clone();
            n.dedup();
            n.len()
        };
        assert_eq!(
            unique,
            presets.len(),
            "{}: duplicate preset numbers in {numbers:?} — the number is the \
             AU's selector, so duplicates would make two menu entries load the \
             same preset",
            unit.label
        );
    }
}

/// An AU that answers the FactoryPresets property with an OSStatus error means
/// "I have no presets", and the host reports that as an empty vec.
///
/// This is not a hypothetical shape: on macOS 15.6 AUDelay, AULowpass and
/// AUNBandEQ all fail the read outright rather than returning an empty array.
/// Treating that as a hard error would make three perfectly functional Apple
/// effects un-hostable, and would push the same `unwrap_or_default` into every
/// caller.
#[test]
fn a_unit_without_presets_reports_an_empty_list_not_an_error() {
    let _g = lock();
    for unit in PRESETLESS_EFFECTS {
        let au = unit.open(RATE, BLOCK);
        assert!(
            au.factory_presets().is_empty(),
            "{} is expected to advertise no factory presets",
            unit.label
        );
        // The unit is otherwise entirely healthy — the absent property is not a
        // symptom of a broken instance.
        assert!(
            !au.get_parameter_list().is_empty(),
            "{}: a unit with no presets must still expose its parameters",
            unit.label
        );
    }
}

/// Instruments ship no factory presets either, and must not error for it.
///
/// Worth asserting separately from the effects above because instruments take a
/// different path through the host — no input bus, a different initialize —
/// and because a preset browser will call this for every plugin on a track
/// regardless of type.
#[test]
fn instruments_report_an_empty_preset_list() {
    let _g = lock();
    for unit in INSTRUMENTS {
        let au = unit.open(RATE, BLOCK);
        assert!(
            au.factory_presets().is_empty(),
            "{} is expected to advertise no factory presets",
            unit.label
        );
    }
}

// ------------------------------------------------------------ preset loading

/// Every advertised preset must load, and `current_preset` must then report
/// back the number *and the name* the enumeration gave for it.
///
/// Checking the name as well as the number is the point: the number is echoed
/// straight back from what the host wrote, so a `PresentPreset` implementation
/// that stored the number and did nothing else would satisfy a number-only
/// assertion. The name can only come from the AU's own preset table, so
/// matching it proves the AU actually resolved the selector.
#[test]
fn every_factory_preset_loads_and_reads_back() {
    let _g = lock();
    for (unit, _) in PRESET_EFFECTS {
        let mut au = unit.open(RATE, BLOCK);
        let presets = au.factory_presets();
        for p in &presets {
            au.load_factory_preset(p.number).unwrap_or_else(|e| {
                panic!(
                    "{}: loading advertised preset {} ({}) failed: {e:?}",
                    unit.label, p.number, p.name
                )
            });
            let current = au
                .current_preset()
                .unwrap_or_else(|e| panic!("{}: current_preset failed: {e:?}", unit.label));
            assert_eq!(
                &current, p,
                "{}: after loading preset {}, the AU reports {current:?}",
                unit.label, p.number
            );
        }
    }
}

/// A preset number the AU does not advertise is an error, and the AU survives
/// it.
///
/// Both halves matter. Without the error a host would silently keep whatever
/// preset was loaded while its UI moved on — the same class of lie as a
/// swallowed bypass. And a rejected `AudioUnitSetProperty` must leave the AU
/// renderable rather than wedged, because a preset browser will hit this every
/// time a saved project names a preset a newer plugin version dropped.
///
/// Measured on macOS 15.6: AUDistortion rejects `9999`, `-1` and `12345` with
/// `-10851` (`kAudioUnitErr_InvalidPropertyValue`). The assertion does not pin
/// that specific status — a different AU may reasonably answer
/// `kAudioUnitErr_InvalidProperty` — only that it is an `OsStatus` refusal
/// rather than a success.
#[test]
fn loading_a_nonexistent_preset_is_an_error_and_the_au_still_renders() {
    let _g = lock();
    let mut au = DISTORTION.open(RATE, BLOCK);

    let presets = au.factory_presets();
    let highest = presets.iter().map(|p| p.number).max().unwrap_or(0);
    // `-1` is AU's "no preset selected" sentinel and must be rejected as a
    // *load*; the others are simply past the end of the table.
    for bad in [highest + 1, -1, 9999] {
        let err = au
            .load_factory_preset(bad)
            .expect_err("loading an unadvertised preset number must not report success");
        assert!(
            matches!(err, AuError::OsStatus { .. }),
            "expected the AU's own OSStatus refusal for preset {bad}, got {err:?}"
        );
    }

    // Load a real preset last so the AU is in a known state, then render: a
    // rejected property write must not have left the unit unable to process.
    au.load_factory_preset(presets[0].number)
        .expect("a valid preset still loads after rejected ones");
    let input = impulse(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK)
        .expect("the AU must still render after a rejected preset load");
    assert!(
        all_finite(&output),
        "a rejected preset load must not leave the AU producing NaN/inf"
    );
}

/// Loading a preset must actually move the AU's parameters — the assertion
/// this whole API rests on.
///
/// A `load_factory_preset` that only stored a number would pass every test
/// above, including the name read-back if the AU resolved the name itself. So
/// this drives the parameters away from the preset's values first and watches
/// them return.
///
/// Measured on macOS 15.6 with AUDistortion (16 parameters, 22 presets):
/// slamming every parameter to its minimum moves **13 of 16** away from
/// preset 0's values (the other 3 already sit at their minimum there), and
/// reloading preset 0 restores **16 of 16**. Distinct presets differ from each
/// other in 13–15 of the 16 parameters. The assertions below are written to
/// those measurements: "at least half moved" for the mutation, and *exact*
/// restoration, because restoration is the property that must hold completely.
#[test]
fn loading_a_preset_changes_parameter_values() {
    let _g = lock();
    let mut au = DISTORTION.open(RATE, BLOCK);
    let presets = au.factory_presets();
    let params = au.get_parameter_list();
    assert!(
        presets.len() >= 2 && !params.is_empty(),
        "this test needs a unit with at least two presets and some parameters"
    );

    let snapshot = |au: &tutti_au_host::AuInstance| -> Vec<f32> {
        params
            .iter()
            .map(|p| au.get_parameter(p.id).expect("parameter is readable"))
            .collect()
    };

    au.load_factory_preset(presets[0].number)
        .expect("load the reference preset");
    let baseline = snapshot(&au);

    // Drive every writable parameter to its minimum. This is the "prove it is
    // not a no-op" step: without it, a preset load that did nothing at all
    // would leave the parameters already matching and the comparison below
    // would pass vacuously.
    for p in &params {
        if p.writable {
            let _ = au.set_parameter(p.id, p.range.min);
        }
    }
    let mutated = snapshot(&au);
    let moved = baseline
        .iter()
        .zip(&mutated)
        .filter(|(a, b)| (*a - *b).abs() > 1e-6)
        .count();
    assert!(
        moved * 2 >= params.len(),
        "only {moved} of {} parameters moved away from preset {}'s values, so \
         the restore below would be vacuous (measured: 13 of 16 move)",
        params.len(),
        presets[0].number
    );

    au.load_factory_preset(presets[0].number)
        .expect("reload the reference preset");
    let restored = snapshot(&au);
    for (i, (want, got)) in baseline.iter().zip(&restored).enumerate() {
        assert!(
            (want - got).abs() <= 1e-6,
            "parameter {} ({}) was {want} under preset {}, was driven to its \
             minimum, and came back as {got} — reloading a preset must restore \
             every parameter it owns",
            params[i].id,
            params[i].name,
            presets[0].number
        );
    }

    // And two different presets must not be the same state, or "loading a
    // preset" would be indistinguishable from loading any other.
    au.load_factory_preset(presets[1].number)
        .expect("load a second preset");
    let second = snapshot(&au);
    let differing = baseline
        .iter()
        .zip(&second)
        .filter(|(a, b)| (*a - *b).abs() > 1e-6)
        .count();
    assert!(
        differing > 0,
        "presets {} ({}) and {} ({}) produced identical parameter state",
        presets[0].number,
        presets[0].name,
        presets[1].number,
        presets[1].name
    );
}

/// Repeated enumeration must be stable, which is how the CoreFoundation
/// ownership rules are pinned.
///
/// The array `kAudioUnitProperty_FactoryPresets` returns is host-owned and must
/// be released; the `CFStringRef` inside each `AUPreset` is **not** and must
/// not be. Both mistakes are silent on a single call. Over-releasing the name
/// corrupts the AU's own preset table, so the symptom is a crash or garbage
/// names on some *later* read — which is why this loops 200 times and compares
/// against the first result rather than checking one enumeration.
///
/// `current_preset` gets the mirror-image treatment: there the name *is*
/// host-owned (Copy rule), so 500 reads would leak 500 CFStrings if it were
/// wrapped under the Get rule instead.
#[test]
fn enumerating_presets_repeatedly_is_stable() {
    let _g = lock();
    let au = DISTORTION.open(RATE, BLOCK);

    let first = au.factory_presets();
    assert!(!first.is_empty());
    for round in 0..200 {
        let again = au.factory_presets();
        assert_eq!(
            again, first,
            "enumeration {round} disagreed with the first — a preset name read \
             under the wrong CoreFoundation ownership rule corrupts the AU's \
             table and shows up exactly here"
        );
    }

    for _ in 0..500 {
        au.current_preset()
            .expect("PresentPreset stays readable across repeated reads");
    }
}

// ------------------------------------------------------------------- bypass

/// Bypass round-trips on every effect that has presets or not: writing the
/// property and reading it back must agree, in both directions.
///
/// Reading back `false` after `set_bypass(false)` is not redundant with the
/// `true` case — a property write that was silently ignored would leave the AU
/// at its initial `false` and pass a `true`-only test in reverse.
#[test]
fn bypass_round_trips_on_effects() {
    let _g = lock();
    for unit in PRESETLESS_EFFECTS
        .iter()
        .chain(PRESET_EFFECTS.iter().map(|(u, _)| u))
    {
        let mut au = unit.open(RATE, BLOCK);
        assert!(
            !au.is_bypassed().unwrap_or_else(|e| panic!(
                "{}: every Apple effect implements BypassEffect: {e:?}",
                unit.label
            )),
            "{}: a freshly initialized effect must not start bypassed",
            unit.label
        );

        au.set_bypass(true)
            .unwrap_or_else(|e| panic!("{}: set_bypass(true) failed: {e:?}", unit.label));
        assert!(
            au.is_bypassed().unwrap(),
            "{}: set_bypass(true) did not take",
            unit.label
        );

        au.set_bypass(false)
            .unwrap_or_else(|e| panic!("{}: set_bypass(false) failed: {e:?}", unit.label));
        assert!(
            !au.is_bypassed().unwrap(),
            "{}: set_bypass(false) did not take",
            unit.label
        );
    }
}

/// An instrument has no input to pass through, so it implements no bypass
/// property — and the host must say so rather than pretend the call worked.
///
/// Measured on macOS 15.6: AUSampler and DLSMusicDevice reject **both** the
/// read and the write with `-10879` (`kAudioUnitErr_InvalidProperty`). The
/// write half is the one that matters: a `set_bypass` that absorbed the failure
/// into `Ok(())` would leave a DAW showing a lit bypass button over an
/// instrument that is still sounding, and the only report would be "the bypass
/// button does nothing".
#[test]
fn bypass_on_an_instrument_is_an_error() {
    let _g = lock();
    for unit in INSTRUMENTS {
        let mut au = unit.open(RATE, BLOCK);

        let read = au
            .is_bypassed()
            .err()
            .unwrap_or_else(|| panic!("{}: an instrument has no bypass to report", unit.label));
        assert!(
            matches!(read, AuError::OsStatus { .. }),
            "{}: expected the AU's OSStatus refusal, got {read:?}",
            unit.label
        );

        let write = au.set_bypass(true).expect_err(
            "set_bypass on an instrument must surface the refusal, not report a \
             bypass that never happened",
        );
        assert!(
            matches!(write, AuError::OsStatus { .. }),
            "{}: expected the AU's OSStatus refusal, got {write:?}",
            unit.label
        );
    }
}

/// The assertion the property round-trip cannot make: a bypassed effect must
/// pass audio through, and an un-bypassed one must not.
///
/// An AU that accepted `BypassEffect` and kept on processing would satisfy
/// `bypass_round_trips_on_effects` completely. Only rendered audio can tell.
///
/// Measured on macOS 15.6, one 512-frame block of a unit impulse through
/// AUDistortion at 48 kHz:
///
/// | | peak | block energy | max |diff| vs input | non-zero tail |
/// |---|---|---|---|---|
/// | bypassed  | 1.000000 | 1.000000 | **0.000000000** | 1 sample |
/// | processed | 0.587368 | 0.348367 | 1.000000000     | 512 samples |
///
/// Bypass is **bit-exact** — the same held for AUDelay, AULowpass,
/// AUDynamicsProcessor and AUMatrixReverb, all at max |diff| exactly 0.0. That
/// is why the passthrough assertion below uses a tight `1e-6` rather than a
/// tuned tolerance: the measurement said zero, so anything a tolerance would
/// buy is slack the AU does not need.
#[test]
fn a_bypassed_effect_passes_audio_through_unchanged() {
    let _g = lock();
    let frames = BLOCK as usize;
    let input = impulse(2, frames);

    let render_once = |bypass: bool| -> Vec<Vec<f32>> {
        // A fresh instance per case: AUDistortion has a 512-sample decay tail
        // (measured: per-block energy 0.348 → 9.9e-6 → 6.0e-8 → 3.5e-10), so
        // toggling bypass on one instance would leave the previous mode's tail
        // bleeding into the next block and the comparison would be measuring
        // the tail, not the bypass.
        let mut au = DISTORTION.open(RATE, BLOCK);
        au.set_bypass(bypass)
            .expect("AUDistortion implements bypass");
        let mut output = silence(2, frames);
        render(&mut au, &input, &mut output, BLOCK).expect("render");
        output
    };

    let bypassed = render_once(true);
    let processed = render_once(false);

    assert!(all_finite(&bypassed) && all_finite(&processed));

    let max_diff = |a: &[Vec<f32>], b: &[Vec<f32>]| -> f32 {
        a.iter()
            .zip(b.iter())
            .flat_map(|(x, y)| x.iter().zip(y.iter()))
            .fold(0.0f32, |m, (p, q)| m.max((p - q).abs()))
    };

    // The bypassed render is the input, sample for sample. Measured: exactly 0.
    let passthrough_error = max_diff(&bypassed, &input);
    assert!(
        passthrough_error <= 1e-6,
        "a bypassed AUDistortion diverged from its input by {passthrough_error} \
         (measured on macOS 15.6: exactly 0.0)"
    );

    // The processed render is not. Measured: max |diff| 1.0, because the
    // distortion delays the impulse by one sample and scales it to 0.587.
    let processed_error = max_diff(&processed, &input);
    assert!(
        processed_error > 1e-3,
        "an un-bypassed AUDistortion produced its input back (max |diff| \
         {processed_error}) — either bypass is stuck on or the AU is not \
         processing at all. Measured: 1.0."
    );

    // And the two renders differ from each other, which is the same fact stated
    // without reference to the input — it survives an AU whose processing
    // happens to be near-identity.
    let between = max_diff(&bypassed, &processed);
    assert!(
        between > 1e-3,
        "bypassed and processed renders were identical (max |diff| {between})"
    );

    // Peak is the coarse, shape-independent version of the same claim: the
    // impulse survives bypass at unity (measured 1.000000) and is attenuated by
    // the distortion (measured 0.587368).
    assert!(
        (peak(&bypassed) - 1.0).abs() <= 1e-6,
        "bypassed peak was {} — a unit impulse must survive bypass at unity",
        peak(&bypassed)
    );
    assert!(
        peak(&processed) < peak(&bypassed),
        "processed peak {} was not below the bypassed peak {}",
        peak(&processed),
        peak(&bypassed)
    );
}

/// Bypass survives an uninitialize/initialize cycle.
///
/// `set_sample_rate` uninitializes and re-initializes under the hood, so a
/// bypass that silently reset there would drop out of bypass the first time a
/// user changed the project sample rate — an audible failure with no error
/// anywhere. Measured on macOS 15.6: AUDistortion reports `true` before, during
/// (in the `Loaded` state) and after the cycle.
#[test]
fn bypass_survives_an_initialize_cycle() {
    let _g = lock();
    let mut au = DISTORTION.open(RATE, BLOCK);
    au.set_bypass(true).expect("set bypass");

    au.uninitialize().expect("uninitialize");
    assert!(
        au.is_bypassed()
            .expect("bypass stays readable while Loaded"),
        "bypass must be readable, and still set, in the Loaded state"
    );

    au.initialize().expect("re-initialize");
    assert!(
        au.is_bypassed()
            .expect("bypass stays readable after re-init"),
        "an initialize cycle must not silently clear bypass"
    );
}
