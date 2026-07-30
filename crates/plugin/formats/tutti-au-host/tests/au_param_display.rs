//! Parameter *display* metadata: value strings, clumps, display curves, and the
//! string↔value conversions.
//!
//! Everything here was verified absent from the host before this suite. The
//! user-visible consequences, in the order the tests below cover them:
//!
//! - **Value strings.** An enum parameter showed as `0.000 / 1.000 / 2.000` in
//!   automation lanes instead of `Parametric / Butterworth Low Pass / …`.
//! - **Clumps.** A 400-parameter synth was one flat list rather than grouped by
//!   section.
//! - **Display curves.** Only 2 of ~15 `AudioUnitParameterInfo` flags were read,
//!   so a log-taper knob drew as linear — on AULowpass's cutoff that puts the
//!   linear midpoint at 11 kHz where the musical midpoint is ~470 Hz.
//! - **`MeterReadOnly`.** Meter pseudo-parameters cluttered the automation menu
//!   as automatable targets that silently discard writes.
//!
//! ## The flag that under-reports
//!
//! `kAudioUnitParameterFlag_ValuesHaveStrings` is NOT a usable gate:
//! `a_value_strings_parameter_enumerates_them` pins that AUNBandEQ answers the
//! property with the flag *clear*. A host must attempt the read regardless.
//!
//! ## What cannot be proven against Apple's units
//!
//! `ParameterStringFromValue` / `ParameterValueFromString` are **unimplemented by
//! every Apple AU** on macOS 15.6 (probed: 15 units × every parameter × several
//! candidate values and strings; every call failed). So the round-trip is asserted
//! as the honest `None`, not as a success — see
//! `the_string_conversions_report_absence_rather_than_fabricating`. Relaxing that
//! into "either works or doesn't" would make it unfalsifiable; the absence is the
//! measurement, and it is what a third-party AU's behaviour will be compared
//! against.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_param_display
//! ```

#![cfg(target_os = "macos")]

use std::sync::Mutex;

mod support;
use support::corpus::{
    CLUMPED_EFFECTS, DELAY, DISTORTION, EFFECTS, LOWPASS, METER_PARAM_UNITS, N_BAND_EQ,
    VALUE_STRING_PARAMS,
};

use tutti_au_host::parameters::{self, DisplayCurve};

/// As in the sibling suites: serialize AudioToolbox instantiation/discovery.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// Poison recovery — the guard only serializes, so one panic must not cascade.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

// -------------------------------------------------------------- value strings

/// A parameter with value strings enumerates them, in order, with the exact
/// labels the AU publishes.
///
/// Measured on macOS 15.6. The counts and the first/last labels are pinned rather
/// than merely asserting non-empty, because the failure this guards is a
/// *truncated* or *misordered* `CFArray` walk — an off-by-one, or elements dropped
/// by a `filter_map` — and "more than zero strings" would pass all of those.
#[test]
fn a_value_strings_parameter_enumerates_them() {
    let _g = lock();
    for &(unit_ref, param_id, count, first, last) in VALUE_STRING_PARAMS {
        let au = unit_ref.open(RATE, BLOCK);
        let strings = parameters::value_strings(au.raw_unit(), param_id);

        assert_eq!(
            strings.len(),
            count,
            "{} param {param_id}: expected {count} value strings, got {strings:?}",
            unit_ref.label
        );
        assert_eq!(
            strings.first().map(String::as_str),
            Some(first),
            "{} param {param_id}: wrong first label",
            unit_ref.label
        );
        assert_eq!(
            strings.last().map(String::as_str),
            Some(last),
            "{} param {param_id}: wrong last label",
            unit_ref.label
        );
        // No empty labels: an empty string is what a mis-read CFStringRef
        // produces, and it is unusable in a menu.
        for (i, s) in strings.iter().enumerate() {
            assert!(
                !s.is_empty(),
                "{} param {param_id}: label {i} is empty",
                unit_ref.label
            );
        }
    }
}

/// The `ValuesHaveStrings` flag must NOT be treated as a gate.
///
/// This is the trap the implementation had to avoid, pinned so a later "tidy-up"
/// cannot reintroduce it: AUNBandEQ's "Type" answers the value-strings property
/// with 11 labels while leaving the flag clear (flags `0xd8100000`, measured). A
/// host that checked the flag first would render a filter-type menu as bare
/// floats.
#[test]
fn value_strings_are_not_gated_on_the_flag_that_under_reports() {
    let _g = lock();
    let au = N_BAND_EQ.open(RATE, BLOCK);
    let params = au.get_parameter_list();

    let type_param = params
        .iter()
        .find(|p| p.id == 2000)
        .expect("AUNBandEQ publishes parameter 2000 (\"Type\")");

    assert!(
        !type_param.values_have_strings,
        "AUNBandEQ param 2000 was measured to leave ValuesHaveStrings CLEAR. If \
         this now reports true, re-measure — but the read below must not become \
         conditional on it either way."
    );
    let strings = parameters::value_strings(au.raw_unit(), 2000);
    assert_eq!(
        strings.len(),
        11,
        "the property answers with 11 filter names even though the flag is clear; \
         got {strings:?}"
    );
    assert_eq!(strings[0], "Parametric");
}

/// A parameter with no value strings yields an empty vec, not an error and not a
/// fabricated single entry.
#[test]
fn a_continuous_parameter_has_no_value_strings() {
    let _g = lock();
    let au = LOWPASS.open(RATE, BLOCK);
    // AULowpass is two continuous parameters (cutoff, resonance); neither is
    // indexed, so neither publishes value strings.
    for p in au.get_parameter_list() {
        let strings = parameters::value_strings(au.raw_unit(), p.id);
        assert!(
            strings.is_empty(),
            "AULowpass param {} ({:?}) is continuous but reported {strings:?}",
            p.id,
            p.name
        );
    }
}

/// An undeclared parameter id must yield no value strings rather than reading
/// whatever the AU has at that slot.
#[test]
fn an_unknown_id_has_no_value_strings() {
    let _g = lock();
    let au = N_BAND_EQ.open(RATE, BLOCK);
    let declared: Vec<u32> = au.get_parameter_list().iter().map(|p| p.id).collect();
    let unknown = (0u32..100_000)
        .find(|id| !declared.contains(id))
        .expect("some id is undeclared");
    assert!(parameters::value_strings(au.raw_unit(), unknown).is_empty());
}

// --------------------------------------------------------------------- clumps

/// Every clump id a parameter claims resolves to a non-empty section name.
///
/// The direction matters and only one direction holds. An id without a name would
/// leave a UI with an unlabelled group, so that is asserted. The converse does
/// **not** hold: AUDistortion names 7 clumps but only 6 are claimed by a
/// parameter (nothing carries clump 6, "Filter"), so a name with no parameters is
/// legal and `CLUMPED_EFFECTS` counts the claimed set. See
/// `distortion_names_its_seven_sections` for the naming side.
#[test]
fn clumped_parameters_resolve_to_section_names() {
    let _g = lock();
    for &(unit_ref, expected_clumps) in CLUMPED_EFFECTS {
        let au = unit_ref.open(RATE, BLOCK);
        let params = au.get_parameter_list();

        let clumped: Vec<_> = params.iter().filter(|p| p.clump.is_some()).collect();
        assert!(
            !clumped.is_empty(),
            "{}: measured to group its parameters, but none reported a clump",
            unit_ref.label
        );

        let mut ids: Vec<u32> = clumped.iter().filter_map(|p| p.clump).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            expected_clumps,
            "{}: expected {expected_clumps} distinct clumps, got {ids:?}",
            unit_ref.label
        );

        for id in ids {
            let name = parameters::clump_name(au.raw_unit(), id).unwrap_or_else(|| {
                panic!(
                    "{}: clump {id} is claimed by a parameter but has no name — a \
                     UI would show an unlabelled section",
                    unit_ref.label
                )
            });
            assert!(
                !name.is_empty(),
                "{}: clump {id} resolved to an empty name",
                unit_ref.label
            );
            // Clump 0 is Apple's "ungrouped" sentinel and must never appear as a
            // real group.
            assert_ne!(
                id, 0,
                "{}: clump 0 is the ungrouped sentinel",
                unit_ref.label
            );
        }
    }
}

/// The exact section names AUDistortion publishes, in clump order.
///
/// Pinned because these are what a user reads. A generic "some non-empty string"
/// assertion would pass if the host returned the *parameter* name, or the same
/// name for every clump — both of which are plausible mis-wirings of a property
/// whose request struct carries the id inside it.
#[test]
fn distortion_names_its_seven_sections() {
    let _g = lock();
    let au = DISTORTION.open(RATE, BLOCK);
    // Measured on macOS 15.6.
    let expected = [
        (1u32, "Delay"),
        (2, "Ring Modulation"),
        (3, "Decimation"),
        (4, "Cubic Polynomial"),
        (5, "Soft Clip"),
        (6, "Filter"),
        (7, "Mix"),
    ];
    for (id, want) in expected {
        assert_eq!(
            parameters::clump_name(au.raw_unit(), id).as_deref(),
            Some(want),
            "AUDistortion clump {id}"
        );
    }
    // And the names are genuinely distinct, so nothing is returning a constant.
    let names: Vec<_> = expected
        .iter()
        .filter_map(|&(id, _)| parameters::clump_name(au.raw_unit(), id))
        .collect();
    let mut deduped = names.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), names.len(), "clump names must be distinct");
}

/// An unclumped AU reports `None` for every parameter, and naming a clump it does
/// not have yields `None` rather than a stray label.
#[test]
fn an_unclumped_unit_reports_no_groups() {
    let _g = lock();
    let au = DELAY.open(RATE, BLOCK);
    for p in au.get_parameter_list() {
        assert!(
            p.clump.is_none(),
            "AUDelay param {} ({:?}) reported clump {:?}",
            p.id,
            p.name,
            p.clump
        );
    }
    // Ids well past anything AUDelay could define.
    for id in [1u32, 2, 99] {
        assert_eq!(parameters::clump_name(au.raw_unit(), id), None);
    }
}

// -------------------------------------------------------------- display curves

/// The display-curve flags are read, and the specific parameters measured to
/// carry a logarithmic taper report one.
///
/// Before this change every parameter reported linear, because only `IsWritable`
/// and `HasCFNameString` were examined. The named parameters are frequency and
/// time controls — exactly the ones where a linear taper is unusable.
#[test]
fn a_log_taper_parameter_reports_a_logarithmic_curve() {
    let _g = lock();
    // (unit, param id, param name) measured to carry DisplayLogarithmic
    // (flags & DisplayMask == 0x400000) on macOS 15.6.
    let cases: &[(support::corpus::AuRef, u32, &str)] = &[
        (LOWPASS, 0, "Cutoff Frequency"),
        (DELAY, 3, "Lowpass Cutoff Frequency"),
    ];
    for &(unit_ref, id, name) in cases {
        let au = unit_ref.open(RATE, BLOCK);
        let params = au.get_parameter_list();
        let p = params
            .iter()
            .find(|p| p.id == id)
            .unwrap_or_else(|| panic!("{}: no parameter {id}", unit_ref.label));
        assert_eq!(
            p.name, name,
            "{}: parameter {id} is not the one this case was measured against",
            unit_ref.label
        );
        assert_eq!(
            p.display,
            DisplayCurve::Logarithmic,
            "{} {:?}: measured to carry DisplayLogarithmic; a Linear here means \
             the display flags are being dropped (or the mask lost bit 22)",
            unit_ref.label,
            p.name
        );
    }
}

/// Across the corpus, at least one non-linear curve is seen and every reported
/// curve is one of the defined variants.
///
/// The lower bound is what stops the decode from silently collapsing to `Linear`
/// everywhere — which is precisely what a contiguous `7 << 16` mask would do,
/// since the logarithmic bit lives at 22.
#[test]
fn non_linear_curves_are_actually_observed() {
    let _g = lock();
    let mut non_linear = 0usize;
    let mut logarithmic = 0usize;
    for unit in EFFECTS {
        let au = unit.open(RATE, BLOCK);
        for p in au.get_parameter_list() {
            if p.display != DisplayCurve::Linear {
                non_linear += 1;
            }
            if p.display == DisplayCurve::Logarithmic {
                logarithmic += 1;
            }
        }
    }
    assert!(
        non_linear > 0,
        "no non-linear display curve found across the corpus effects — the \
         display flags are not being read"
    );
    assert!(
        logarithmic > 0,
        "no LOGARITHMIC curve found, which is the variant a contiguous 3-bit \
         display mask would drop (the bit is at 22, outside 16..=18)"
    );
}

// -------------------------------------------------------------- meter params

/// Meter pseudo-parameters are flagged, so a host can keep them out of its
/// automation menu.
///
/// Measured on macOS 15.6: AUMultibandCompressor publishes 12 and AUSampler 2.
/// The counts are pinned because the failure mode is under-detection — a host that
/// flagged none would still pass an "at least zero" assertion, and the meters
/// would reappear as automatable targets that discard writes.
#[test]
fn meter_parameters_are_flagged_as_read_only_readings() {
    let _g = lock();
    for &(unit_ref, expected) in METER_PARAM_UNITS {
        let au = unit_ref.open(RATE, BLOCK);
        let params = au.get_parameter_list();
        let meters: Vec<_> = params.iter().filter(|p| p.meter_read_only).collect();
        assert_eq!(
            meters.len(),
            expected,
            "{}: expected {expected} MeterReadOnly parameters, got {:?}",
            unit_ref.label,
            meters.iter().map(|p| (&p.name, p.id)).collect::<Vec<_>>()
        );
    }
}

/// The meter flag must discriminate: most parameters on a corpus effect are
/// controls and must NOT be flagged, or the filter that hides meters would hide
/// the whole automation menu.
///
/// Not a blanket "no effect has a meter" assertion — that was the first version of
/// this test and it was **wrong**. AUDynamicsProcessor genuinely publishes three
/// meters ("Comp Amount" 1000, "Input Amplitude" 2000, "Output Amplitude" 3000,
/// all flags `0x48108010`), which the host reads correctly. The real invariant is
/// that the flag separates the two populations rather than being stuck on or off,
/// so this pins the exact split measured on macOS 15.6.
#[test]
fn the_meter_flag_discriminates_meters_from_controls() {
    let _g = lock();
    // (unit, meter count) measured across the corpus effects. AUDelay, AUNBandEQ
    // and AULowpass publish none; AUDynamicsProcessor publishes three.
    let expected: &[(&str, usize)] = &[
        ("AUDelay", 0),
        ("AUNBandEQ", 0),
        ("AULowpass", 0),
        ("AUDynamicsProcessor", 3),
    ];
    let mut total_controls = 0usize;
    let mut total_meters = 0usize;

    for unit in EFFECTS {
        let au = unit.open(RATE, BLOCK);
        let params = au.get_parameter_list();
        let meters: Vec<_> = params.iter().filter(|p| p.meter_read_only).collect();
        let want = expected
            .iter()
            .find(|(label, _)| *label == unit.label)
            .map(|(_, n)| *n)
            .unwrap_or_else(|| panic!("{}: no measured meter count", unit.label));
        assert_eq!(
            meters.len(),
            want,
            "{}: expected {want} meter parameter(s), got {:?}",
            unit.label,
            meters.iter().map(|p| (&p.name, p.id)).collect::<Vec<_>>()
        );
        total_meters += meters.len();
        total_controls += params.len() - meters.len();
    }

    // Both populations must be non-empty, or the flag is not discriminating —
    // a host that hard-coded `false` would pass every per-unit count above
    // except AUDynamicsProcessor's, and one that hard-coded `true` would fail
    // them all, so this is the summary guard.
    assert!(
        total_meters > 0,
        "no meter parameter found across the corpus"
    );
    assert!(
        total_controls > 40,
        "expected many control parameters, got {total_controls}"
    );
}

// ------------------------------------------------------- string <-> value

/// The two string-conversion properties report the AU's refusal honestly rather
/// than fabricating a value.
///
/// **Measured: no Apple AU on macOS 15.6 implements either property.** Probed
/// across 15 units × every parameter × the min/mid/max of each range for
/// `StringFromValue`, and × 5 candidate strings for `ValueFromString`; every call
/// failed. So the assertion here is `None` — which is the *correct* answer, and
/// the one a host needs, because the alternative that was live in the design was
/// returning `Some(0.0)` on a failed parse and committing that to the user's
/// preset.
///
/// This test is deliberately NOT written as "either `Some` or `None` is fine": that
/// would be unfalsifiable. If a future macOS implements these, this test fails and
/// should be *re-measured and tightened* into a real round-trip — not loosened.
#[test]
fn the_string_conversions_report_absence_rather_than_fabricating() {
    let _g = lock();
    let mut probed = 0usize;
    for unit in EFFECTS {
        let au = unit.open(RATE, BLOCK);
        for p in au.get_parameter_list() {
            for value in [p.range.min, p.range.mid(), p.range.max] {
                assert_eq!(
                    parameters::string_from_value(au.raw_unit(), p.id, value),
                    None,
                    "{} param {} ({:?}): no Apple AU was measured to implement \
                     ParameterStringFromValue. A Some() here means macOS gained \
                     the property — re-measure and tighten this into a real \
                     round-trip rather than relaxing it.",
                    unit.label,
                    p.id,
                    p.name
                );
                probed += 1;
            }
            for text in ["1.0", "-6", "Band Pass", "Parametric", ""] {
                assert_eq!(
                    parameters::value_from_string(au.raw_unit(), p.id, text),
                    None,
                    "{} param {} ({:?}): {text:?} must not parse to a value — no \
                     Apple AU implements ParameterValueFromString, and a \
                     fabricated number here would be written into a preset",
                    unit.label,
                    p.id,
                    p.name
                );
                probed += 1;
            }
        }
    }
    // The loop must have actually run; an empty corpus would pass vacuously.
    assert!(
        probed > 100,
        "expected to probe many (parameter, value) pairs, only did {probed}"
    );
}

/// Even for AUNBandEQ's "Type" — the one parameter whose value *names* the host
/// can enumerate — the parsing property is unimplemented.
///
/// Worth its own test because it is the case that looks like it should work: the
/// AU knows "Band Pass" is one of its labels, and a reader might assume
/// enumeration implies parsing. The two properties are independent, and Apple
/// implements only the former.
#[test]
fn enumerable_labels_do_not_imply_a_parsable_parameter() {
    let _g = lock();
    let au = N_BAND_EQ.open(RATE, BLOCK);
    let strings = parameters::value_strings(au.raw_unit(), 2000);
    assert_eq!(strings.len(), 11, "precondition: the labels are enumerable");

    for (index, label) in strings.iter().enumerate() {
        assert_eq!(
            parameters::value_from_string(au.raw_unit(), 2000, label),
            None,
            "AUNBandEQ can name value {index} as {label:?} but does not implement \
             ParameterValueFromString; a host must map the label back through the \
             enumeration itself"
        );
    }

    // The supported path: a host resolves a label to a value by its index in the
    // enumeration, which is what the positional contract of `value_strings`
    // guarantees. Verified by writing it and reading it back.
    let mut au = au;
    let target_index = strings
        .iter()
        .position(|s| s == "Band Pass")
        .expect("\"Band Pass\" is one of the 11 labels");
    let params = au.get_parameter_list();
    let p = params.iter().find(|p| p.id == 2000).unwrap();
    let value = p.range.min + target_index as f32;
    au.set_parameter(2000, value)
        .expect("write the filter type");
    let got = au.get_parameter(2000).expect("read it back");
    assert!(
        (got - value).abs() < 1e-4,
        "index-based label resolution must round-trip: wrote {value}, read {got}"
    );
}

// ------------------------------------------------------------------ coherence

/// Every parameter's display metadata must be self-consistent, across the whole
/// corpus.
///
/// The combinations asserted here are the ones a UI would mis-render:
/// a meter that claims to be writable, or a clump id of 0 presented as a real
/// group.
#[test]
fn display_metadata_is_self_consistent() {
    let _g = lock();
    for unit in EFFECTS {
        let au = unit.open(RATE, BLOCK);
        for p in au.get_parameter_list() {
            let ctx = format!("{} param {} ({:?})", unit.label, p.id, p.name);
            // A `Some(0)` clump is the sentinel leaking through as a real group.
            assert_ne!(
                p.clump,
                Some(0),
                "{ctx}: clump 0 is Apple's ungrouped sentinel and must be None"
            );
            // A meter is a reading, so it must not also be advertised as a
            // writable control — a host would offer automation that goes nowhere.
            if p.meter_read_only {
                assert!(
                    !p.writable,
                    "{ctx}: flagged MeterReadOnly and IsWritable at once"
                );
            }
        }
    }
}
