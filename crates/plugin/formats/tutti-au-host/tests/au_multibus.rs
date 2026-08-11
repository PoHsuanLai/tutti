//! Multi-bus conformance: does this host read an AU's bus topology correctly?
//!
//! `au_conformance.rs` drives the single-bus path — one input, one output,
//! element 0 throughout. This suite covers the axis that path assumes away:
//!
//! - bus (element) counts per direction, including the zero-input case
//! - per-bus channel layout, and that an out-of-range bus **errors**
//! - `kAudioUnitProperty_SupportedNumChannels`, whose negative entries are
//!   sentinels rather than channel counts
//! - per-element parameters: the same id on two elements is two values
//! - and that a genuinely multi-bus unit still renders
//!
//! Every number asserted below was measured on this machine (macOS 15.6) with a
//! throwaway probe before the assertion was written, and the measurement is
//! recorded beside it. Nothing here is inferred from Apple's documentation about
//! what a unit *ought* to report.
//!
//! ## The corpus, and why mixers joined it
//!
//! Effects and instruments are nearly all single-bus: all 22 Apple effects
//! measured are exactly 1 in / 1 out, and only DLSMusicDevice (0 in / **2 out**)
//! breaks the pattern among instruments. The mixers are where AUv2's multi-bus
//! and per-element-parameter features are actually used — AUMatrixMixer is 64 in
//! / 4 out — so `support/corpus.rs` gained three of them.
//!
//! ## Mixers are hosted uninitialized here, deliberately
//!
//! AUMatrixMixer and AUMultiSplitter both fail `AudioUnitInitialize` with
//! `kAudioUnitErr_FailedInitialization` (`-10875`) as this host configures them:
//! a matrix mixer wants an explicit per-bus channel configuration written before
//! initialize, which is a *writing* feature this change does not add. Verified
//! to pre-date this change by running the same probe against `main`.
//!
//! That is not an obstacle to what is asserted here. Bus topology, channel
//! layouts, supported configurations, and parameter values are all readable in
//! the `Loaded` state — the state a host is in precisely when it needs the
//! topology in order to decide how to configure the unit. The tests that need a
//! rendering unit use DLSMusicDevice, which is multi-bus *and* initializes.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_multibus
//! ```

#![cfg(target_os = "macos")]

use std::sync::Mutex;

mod support;
use support::corpus::{
    all_finite, peak, render, silence, DLS_SYNTH, EFFECTS, INSTRUMENTS, MATRIX_MIXER, MIXERS,
    MULTI_CHANNEL_MIXER, MULTI_SPLITTER, SAMPLER,
};

use tutti_au_host::parameters::{self, ParamAddress};
use tutti_au_host::types::K_AUDIO_UNIT_ERR_INVALID_ELEMENT;
use tutti_au_host::{AuChannelConfig, AuChannelCount, AuError, BusDirection};
use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};

/// Same rationale as `au_conformance.rs`'s `AU_LOCK`: AudioToolbox tolerates
/// concurrent use of distinct units, but component discovery walks a
/// process-global registry and these tests open the same units the conformance
/// suite does.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// Recover from poisoning: the guard only serializes, it protects no shared
/// state, so one panicking test must not convert into N spurious failures.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// Assert that `result` failed with the AU's own invalid-element status rather
/// than any other error — and, critically, rather than succeeding.
///
/// The distinction matters because `ChannelLayout` has no "absent" value: a
/// host handed `Stereo` for a bus that does not exist would allocate buffers
/// for it and never learn otherwise.
fn assert_invalid_element<T: std::fmt::Debug>(result: Result<T, AuError>, what: &str) {
    match result {
        Err(AuError::OsStatus {
            code: K_AUDIO_UNIT_ERR_INVALID_ELEMENT,
            ..
        }) => {}
        Err(other) => panic!(
            "{what}: expected kAudioUnitErr_InvalidElement \
             ({K_AUDIO_UNIT_ERR_INVALID_ELEMENT}), got {other:?}"
        ),
        Ok(value) => panic!(
            "{what}: an out-of-range element returned {value:?} instead of an \
             error — a fabricated layout here is what a caller sizes buffers from"
        ),
    }
}

// ------------------------------------------------------------- bus topology

/// Every Apple effect is exactly one bus in, one bus out.
///
/// Measured across all 22 Apple `aufx` units on macOS 15.6: input element count
/// 1, output element count 1, without exception. The single-bus render path in
/// `AuInstance::process` is correct *because* of this, so if it ever stops
/// holding the render path needs revisiting rather than this assertion relaxing.
#[test]
fn effects_have_exactly_one_bus_per_direction() {
    let _g = lock();
    for unit in EFFECTS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        assert_eq!(
            au.bus_count(BusDirection::Input),
            1,
            "{}: an effect has exactly one input bus",
            unit.label
        );
        assert_eq!(
            au.bus_count(BusDirection::Output),
            1,
            "{}: an effect has exactly one output bus",
            unit.label
        );
    }
}

/// DLSMusicDevice has no input buses and **two** output buses.
///
/// This is the one genuinely multi-bus unit among the instruments, and the
/// reason the zero-input and multi-output cases can be asserted on a unit that
/// also initializes and renders. Measured on macOS 15.6: input element count 0,
/// output element count 2, both output buses reporting a stereo stream format.
///
/// AUSampler is checked alongside it as the contrast — also zero-input, but only
/// one output bus — so a `bus_count` that returned a constant could not pass
/// both.
#[test]
fn the_dls_synth_reports_no_inputs_and_two_output_buses() {
    let _g = lock();
    let au = DLS_SYNTH.open_uninitialized(RATE, BLOCK);
    assert_eq!(
        au.bus_count(BusDirection::Input),
        0,
        "DLSMusicDevice is an instrument: it has no input element"
    );
    assert_eq!(
        au.bus_count(BusDirection::Output),
        2,
        "DLSMusicDevice publishes two output buses"
    );

    let sampler = SAMPLER.open_uninitialized(RATE, BLOCK);
    assert_eq!(sampler.bus_count(BusDirection::Input), 0);
    assert_eq!(
        sampler.bus_count(BusDirection::Output),
        1,
        "AUSampler has a single output bus — so a constant 2 would fail here"
    );
}

/// Every instrument reports zero input buses, and that zero is what the host
/// keys the input-render-callback install off.
///
/// `au_conformance.rs::instrument_with_no_input_bus_initializes` asserts the
/// *consequence* (initialize succeeds). This asserts the *input* to that
/// decision, so a regression is localized rather than showing up as a confusing
/// initialize failure.
#[test]
fn instruments_report_no_input_buses() {
    let _g = lock();
    for unit in INSTRUMENTS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        assert_eq!(
            au.bus_count(BusDirection::Input),
            0,
            "{}: an instrument has no input element",
            unit.label
        );
        assert!(
            au.bus_count(BusDirection::Output) >= 1,
            "{}: an instrument must have somewhere to put its audio",
            unit.label
        );
        // And the host's own derived view agrees with the raw element count.
        assert_eq!(
            au.num_inputs(),
            0,
            "{}: num_inputs must agree with the input element count",
            unit.label
        );
    }
}

/// The host's view of "is there an input bus" must agree with the AU's own
/// input element count, across every unit in the corpus.
///
/// ## What this does and does not pin
///
/// `StreamConfig::probe` derives `has_input` from the input **element count**.
/// Inferring it from whether the input stream-format read succeeded answers a
/// different question: an AU may legally have an input element whose format it
/// declines to report, and for that unit the format-based inference says "no
/// input" — so the host skips installing the input render callback and the unit
/// renders from silence.
///
/// **This test does not catch a swap to the format-based form, and neither can
/// any other test on this machine.** Measured across all 132 installed AUs, the
/// two inference methods agree on every single one — 55 with an input element,
/// 77 without, zero disagreements. So the element-count form is *defensive*:
/// correct per Apple's model, but with no locally observable behavioural
/// difference. A mutation swapping it passes the entire suite, and that is a
/// property of the available corpus, not a gap in the assertions.
///
/// What it does pin is the invariant itself — the host's derived view never
/// drifts from the AU's element count — which is what would break first if
/// either side were rewritten independently. A unit that genuinely disagrees is
/// the case the change exists for, and this is where it would surface.
#[test]
fn has_input_is_derived_from_the_element_count() {
    let _g = lock();
    let mut with_input = 0;
    let mut without_input = 0;

    for unit in EFFECTS.iter().chain(INSTRUMENTS).chain(MIXERS) {
        let au = unit.open_uninitialized(RATE, BLOCK);
        let elements = au.bus_count(BusDirection::Input);
        // `num_inputs` is `probe`'s `has_input` decision made visible: it
        // reports 0 exactly when the host believes there is no input bus.
        let host_sees_input = au.num_inputs() > 0;
        assert_eq!(
            host_sees_input,
            elements > 0,
            "{}: the host sees input={host_sees_input} but the AU reports \
             {elements} input element(s) — `probe` must key off the element \
             count, which is the AU's direct answer",
            unit.label
        );
        if elements > 0 {
            with_input += 1;
        } else {
            without_input += 1;
        }
    }

    // Both branches must be exercised or the assertion above is half-vacuous:
    // a corpus of only-instruments would pass it with `has_input` hard-wired
    // false, and only-effects with it hard-wired true.
    assert!(
        with_input >= 2 && without_input >= 2,
        "the corpus must cover both branches; saw {with_input} with an input \
         element and {without_input} without"
    );
}

/// Mixers are the wide case: many input buses, sometimes several output buses.
///
/// Measured on macOS 15.6 — AUMatrixMixer 64 in / 4 out, AUMultiChannelMixer
/// 8 in / 1 out, AUMultiSplitter 1 in / 2 out. Asserted exactly rather than as
/// "> 1" because an exact count is what a host builds its channel strip from,
/// and a silently-halved count would still satisfy an inequality.
#[test]
fn mixers_report_their_full_bus_topology() {
    let _g = lock();
    for (unit, expect_in, expect_out) in [
        (MATRIX_MIXER, 64, 4),
        (MULTI_CHANNEL_MIXER, 8, 1),
        (MULTI_SPLITTER, 1, 2),
    ] {
        let au = unit.open_uninitialized(RATE, BLOCK);
        assert_eq!(
            (
                au.bus_count(BusDirection::Input),
                au.bus_count(BusDirection::Output)
            ),
            (expect_in, expect_out),
            "{}: bus topology",
            unit.label
        );
    }
}

// -------------------------------------------------------------- bus layouts

/// Every bus a unit claims to have must report a usable layout, and the first
/// bus past that count must error.
///
/// The two halves belong in one test because either alone is passable by a
/// broken implementation: always returning `Ok(Stereo)` passes the first, and
/// always returning `Err` passes the second.
#[test]
fn every_declared_bus_has_a_layout_and_the_next_one_errors() {
    let _g = lock();
    for unit in EFFECTS.iter().chain(INSTRUMENTS).chain(MIXERS) {
        let au = unit.open_uninitialized(RATE, BLOCK);
        for direction in BusDirection::ALL {
            let count = au.bus_count(direction);
            for bus in 0..count {
                let layout = au
                    .bus_layout(direction, bus)
                    .unwrap_or_else(|e| panic!("{} {direction} bus {bus}: {e:?}", unit.label));
                assert!(
                    layout.count() >= 1,
                    "{} {direction} bus {bus}: a declared bus reported {} channels",
                    unit.label,
                    layout.count()
                );
            }
            // The first index past the end. Measured: AudioToolbox answers
            // -10877 here for every unit in the corpus.
            assert_invalid_element(
                au.bus_layout(direction, count),
                &format!("{} {direction} bus {count} (one past the end)", unit.label),
            );
        }
    }
}

/// A wildly out-of-range bus index errors rather than wrapping onto a real bus.
///
/// Distinct from the one-past-the-end case above: an index that is merely large
/// exercises whatever arithmetic sits between the caller and the AU, where a
/// truncation to `u16` or a modulo would quietly alias 65_536 onto bus 0 and
/// return a plausible layout.
#[test]
fn a_far_out_of_range_bus_does_not_alias_onto_a_real_one() {
    let _g = lock();
    let au = DLS_SYNTH.open_uninitialized(RATE, BLOCK);
    for bus in [2u32, 3, 99, 65_536, u32::MAX] {
        assert_invalid_element(
            au.bus_layout(BusDirection::Output, bus),
            &format!("DLSMusicDevice output bus {bus}"),
        );
    }
}

/// Both of DLSMusicDevice's output buses are real and separately addressable.
///
/// Measured on macOS 15.6: output bus 0 and bus 1 each report a stereo stream
/// format. Reading bus 1 specifically is what proves `bus_layout` honours its
/// index instead of always querying element 0 — the exact bug the pre-change
/// code had, where every query was hard-wired to bus 0.
#[test]
fn each_output_bus_of_a_multi_bus_unit_is_addressable() {
    let _g = lock();
    let au = DLS_SYNTH.open_uninitialized(RATE, BLOCK);
    for bus in 0..2 {
        let layout = au
            .bus_layout(BusDirection::Output, bus)
            .unwrap_or_else(|e| panic!("DLSMusicDevice output bus {bus}: {e:?}"));
        assert_eq!(
            layout.count(),
            2,
            "DLSMusicDevice output bus {bus} is stereo"
        );
    }
}

/// Asking an instrument for its input bus 0 errors — it has none.
///
/// A host that got a layout back here would install an input render callback on
/// a unit with no input element, which is the `-10877` failure that made every
/// AU instrument unloadable before the conformance suite existed. This asserts
/// the topology query cannot recreate that mistake.
#[test]
fn an_instrument_has_no_input_bus_zero() {
    let _g = lock();
    for unit in INSTRUMENTS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        assert_invalid_element(
            au.bus_layout(BusDirection::Input, 0),
            &format!(
                "{}: input bus 0 on a unit with no input element",
                unit.label
            ),
        );
    }
}

// ------------------------------------------------- supported channel configs

/// The declared configurations parse, and every entry is self-consistent.
///
/// An empty list is a legitimate answer — all 22 Apple effects publish nothing
/// for this property, meaning "unconstrained, consult the stream format" — so
/// emptiness is not asserted against. What is asserted is that whatever *is*
/// published decodes into the typed form without an `Exactly` variant holding a
/// value that came from a negative field.
#[test]
fn declared_channel_configs_decode_without_coercing_sentinels() {
    let _g = lock();
    for unit in EFFECTS.iter().chain(INSTRUMENTS).chain(MIXERS) {
        let au = unit.open_uninitialized(RATE, BLOCK);
        for config in au.supported_channel_configs() {
            for (side, count) in [("in", config.inputs), ("out", config.outputs)] {
                match count {
                    // The whole point of the typed representation: a negative
                    // raw field can only ever land in `Any` or
                    // `TotalAcrossBuses`, never in a literal count.
                    AuChannelCount::Exactly(n) => assert!(
                        n <= 64,
                        "{} {side}: a literal channel count of {n} is implausible \
                         and suggests a sentinel was coerced into a count",
                        unit.label
                    ),
                    AuChannelCount::TotalAcrossBuses { max_total } => assert!(
                        max_total >= 1,
                        "{} {side}: a total-across-buses cap of {max_total} \
                         channels means the unit can carry no audio at all",
                        unit.label
                    ),
                    AuChannelCount::Any { wildcard } => assert!(
                        wildcard == -1 || wildcard == -2,
                        "{} {side}: `Any` may only hold the documented sentinels \
                         -1 or -2, got {wildcard}",
                        unit.label
                    ),
                    AuChannelCount::NoElements => {}
                }
            }
        }
    }
}

/// AUSampler and AUMIDISynth declare `{0, -16}`, and `-16` must decode as a
/// total-across-buses cap rather than as a channel count.
///
/// This is the concrete instance of the coercion bug the typed representation
/// exists to prevent. Measured on macOS 15.6: AUSampler publishes exactly one
/// entry, `{0, -16}`. Read as a count that would be *negative sixteen channels*;
/// `abs()`-ed it becomes "16 channels on a single bus", which would have the
/// host offer a 16-channel output bus on a unit whose only bus is stereo.
#[test]
fn the_samplers_negative_entry_is_a_total_not_a_channel_count() {
    let _g = lock();
    let au = SAMPLER.open_uninitialized(RATE, BLOCK);
    let configs = au.supported_channel_configs();
    assert_eq!(
        configs.len(),
        1,
        "AUSampler publishes exactly one channel configuration, got {configs:?}"
    );
    let config = configs[0];
    assert_eq!(
        config.inputs,
        AuChannelCount::NoElements,
        "AUSampler's `0` input field means it has no input elements"
    );
    assert_eq!(
        config.outputs,
        AuChannelCount::TotalAcrossBuses { max_total: 16 },
        "AUSampler's `-16` means at most 16 channels across the output scope"
    );
    // The bug this shape prevents, stated as the assertion it would fail.
    assert_ne!(
        config.outputs,
        AuChannelCount::Exactly(16),
        "-16 must not be read as a literal 16-channel bus"
    );
    // And the unit's actual output bus is stereo, well under the declared cap.
    assert!(
        config.outputs.admits(2),
        "the stereo output bus AUSampler actually runs must satisfy its own \
         declared cap"
    );
}

/// DLSMusicDevice declares `{0, 2}` — a literal count on the output side, with
/// no sentinel at all.
///
/// The counterpart to the test above: the same field position that carries a
/// sentinel on AUSampler carries a plain count here, so a decoder that treated
/// the whole property as sentinels would fail this one.
#[test]
fn the_dls_synth_declares_a_literal_stereo_output() {
    let _g = lock();
    let au = DLS_SYNTH.open_uninitialized(RATE, BLOCK);
    let configs = au.supported_channel_configs();
    assert_eq!(
        configs.len(),
        1,
        "DLSMusicDevice publishes exactly one channel configuration, got {configs:?}"
    );
    assert_eq!(configs[0].inputs, AuChannelCount::NoElements);
    assert_eq!(configs[0].outputs, AuChannelCount::Exactly(2));
    assert!(configs[0].admits(0, 2));
    assert!(
        !configs[0].admits(2, 2),
        "DLSMusicDevice declares no input elements, so a 2-in topology is not \
         one it offers"
    );
}

/// The two wildcard spellings survive the trip through a real AU.
///
/// Measured on macOS 15.6: AUMultiSplitter publishes `{-1, -1}` ("any width, but
/// input and output must match") while AUMatrixMixer and AUMultiChannelMixer
/// publish `{-1, -2}` ("any width on each side, independently"). A decoder that
/// collapsed both onto one "any" variant would report the splitter as accepting
/// a 6-in/2-out configuration it cannot run — so the distinction is asserted
/// here against live units, not only against synthetic values in the unit tests.
#[test]
fn the_matching_constraint_survives_a_real_au() {
    let _g = lock();

    let splitter = MULTI_SPLITTER.open_uninitialized(RATE, BLOCK);
    let configs = splitter.supported_channel_configs();
    assert_eq!(
        configs,
        vec![AuChannelConfig {
            inputs: AuChannelCount::Any { wildcard: -1 },
            outputs: AuChannelCount::Any { wildcard: -1 },
        }],
        "AUMultiSplitter publishes the matched-width spelling {{-1,-1}}"
    );
    assert!(configs[0].requires_matching_counts());
    assert!(configs[0].admits(6, 6));
    assert!(
        !configs[0].admits(6, 2),
        "{{-1,-1}} requires the two sides to match"
    );

    for unit in [MATRIX_MIXER, MULTI_CHANNEL_MIXER] {
        let au = unit.open_uninitialized(RATE, BLOCK);
        let configs = au.supported_channel_configs();
        assert_eq!(
            configs,
            vec![AuChannelConfig {
                inputs: AuChannelCount::Any { wildcard: -1 },
                outputs: AuChannelCount::Any { wildcard: -2 },
            }],
            "{}: publishes the independent-width spelling {{-1,-2}}",
            unit.label
        );
        assert!(!configs[0].requires_matching_counts(), "{}", unit.label);
        assert!(
            configs[0].admits(8, 2),
            "{}: a mixer's whole job is N-in to M-out",
            unit.label
        );
    }
}

// ------------------------------------------------------ per-element params

/// A mixer publishes an independent parameter strip on each input bus.
///
/// Measured on macOS 15.6: AUMultiChannelMixer lists 7 parameter ids on the
/// input scope, and the list is identical for elements 0, 1 and 2 — the strip is
/// per-element, not a single shared set. The global scope, by contrast, is where
/// effects keep everything and where this mixer keeps nothing.
#[test]
fn a_mixer_publishes_a_parameter_strip_per_input_bus() {
    let _g = lock();
    let au = MULTI_CHANNEL_MIXER.open_uninitialized(RATE, BLOCK);
    let unit = au.raw_unit();

    let strip0 = parameters::list_at(unit, ParamAddress::on_bus(BusDirection::Input, 0));
    assert_eq!(
        strip0.len(),
        7,
        "AUMultiChannelMixer publishes 7 parameters on each input element"
    );

    let strip1 = parameters::list_at(unit, ParamAddress::on_bus(BusDirection::Input, 1));
    let ids0: Vec<u32> = strip0.iter().map(|p| p.id).collect();
    let ids1: Vec<u32> = strip1.iter().map(|p| p.id).collect();
    assert_eq!(
        ids0, ids1,
        "every input element carries the same parameter ids — the strip is \
         replicated per bus, not partitioned across them"
    );

    // The output scope has its own, different strip: 5 parameters, measured.
    let out = parameters::list_at(unit, ParamAddress::on_bus(BusDirection::Output, 0));
    assert_eq!(
        out.len(),
        5,
        "AUMultiChannelMixer publishes 5 parameters on its output element"
    );
    assert_ne!(
        ids0,
        out.iter().map(|p| p.id).collect::<Vec<_>>(),
        "input and output strips are different parameter sets"
    );
}

/// The same parameter id on two different elements holds two independent values.
///
/// This is the assertion that makes per-element addressing worth having, and the
/// one that fails outright against the pre-change code: with every access
/// hard-wired to element 0, the second write below would land on the first
/// element and both reads would return `0.75`.
///
/// Measured on macOS 15.6 before writing this: setting input element 0's volume
/// (id 0) to 0.25 and element 1's to 0.75 reads back exactly 0.25 and 0.75.
#[test]
fn the_same_parameter_id_on_two_elements_holds_two_values() {
    let _g = lock();
    let au = MULTI_CHANNEL_MIXER.open_uninitialized(RATE, BLOCK);
    let raw = au.raw_unit();

    let bus0 = ParamAddress::on_bus(BusDirection::Input, 0);
    let bus1 = ParamAddress::on_bus(BusDirection::Input, 1);
    let volume = parameters::list_at(raw, bus0)
        .into_iter()
        .find(|p| p.writable)
        .expect("AUMultiChannelMixer's input strip has writable parameters");

    parameters::set_at(raw, bus0, volume.id, 0.25).expect("write element 0");
    parameters::set_at(raw, bus1, volume.id, 0.75).expect("write element 1");

    let read0 = parameters::get_at(raw, bus0, volume.id).expect("read element 0");
    let read1 = parameters::get_at(raw, bus1, volume.id).expect("read element 1");

    assert!(
        (read0 - 0.25).abs() < 1e-4,
        "input element 0 param {} read back {read0}, expected 0.25 — element 1's \
         write leaked into it",
        volume.id
    );
    assert!(
        (read1 - 0.75).abs() < 1e-4,
        "input element 1 param {} read back {read1}, expected 0.75",
        volume.id
    );
}

/// Addressing an element the AU does not have fails instead of silently
/// landing on element 0.
///
/// A write that aliased onto element 0 would move a control the user can see
/// while the caller believed it had addressed a different bus — a corruption
/// with no error to notice. Measured: AudioToolbox returns -10877 for element
/// 9999 on AUMultiChannelMixer.
#[test]
fn a_parameter_write_to_a_missing_element_fails_rather_than_aliasing() {
    let _g = lock();
    let au = MULTI_CHANNEL_MIXER.open_uninitialized(RATE, BLOCK);
    let raw = au.raw_unit();

    let bus0 = ParamAddress::on_bus(BusDirection::Input, 0);
    let volume = parameters::list_at(raw, bus0)
        .into_iter()
        .find(|p| p.writable)
        .expect("a writable input parameter");

    // Park a known value on element 0, then aim past the last real element.
    parameters::set_at(raw, bus0, volume.id, 0.5).expect("seed element 0");
    let missing = ParamAddress::on_bus(BusDirection::Input, 9_999);

    assert_invalid_element(
        parameters::get_at(raw, missing, volume.id),
        "read of input element 9999",
    );
    assert_invalid_element(
        parameters::set_at(raw, missing, volume.id, 0.9),
        "write to input element 9999",
    );

    let after = parameters::get_at(raw, bus0, volume.id).expect("re-read element 0");
    assert!(
        (after - 0.5).abs() < 1e-4,
        "element 0 now holds {after}; the rejected write to element 9999 aliased \
         onto it instead of failing cleanly"
    );
}

/// The un-suffixed parameter functions still mean global / element 0.
///
/// The `_at` variants were added underneath the existing API, and every call
/// site in `tutti-plugin-server` uses the un-suffixed form. This pins that the
/// default did not move: `get` must agree with `get_at(GLOBAL)` on a unit whose
/// parameters live on the global scope.
#[test]
fn the_default_parameter_address_is_still_global_element_zero() {
    let _g = lock();
    let mut au = support::corpus::DELAY.open(RATE, BLOCK);
    let raw = au.raw_unit();

    let params = au.get_parameter_list();
    assert_eq!(
        params.len(),
        parameters::list_at(raw, ParamAddress::GLOBAL).len(),
        "the un-suffixed list must be the global/element-0 list"
    );

    let param = params
        .into_iter()
        .find(|p| p.writable)
        .expect("AUDelay has writable parameters");
    let target = param.range.mid();
    au.set_parameter(param.id, target).expect("set via default");

    let via_default = au.get_parameter(param.id).expect("get via default");
    let via_address =
        parameters::get_at(raw, ParamAddress::GLOBAL, param.id).expect("get via address");
    assert_eq!(
        via_default, via_address,
        "the default path and the explicit global address must reach the same \
         parameter"
    );
}

// ------------------------------------------------------------------ render

/// A multi-bus unit still renders.
///
/// DLSMusicDevice is the corpus's only unit that is both genuinely multi-bus
/// (two output elements) and able to initialize, so it is the one subject where
/// "does multi-bus topology break rendering?" can be asked at all.
///
/// Only bus 0 is rendered — `AuInstance::process` drives the primary bus, and
/// this change adds topology *discovery*, not per-bus rendering. The assertion
/// is therefore that discovering a second bus did not disturb the first: a
/// NoteOn still produces finite, audible audio, exactly as
/// `au_conformance.rs::note_on_produces_audio_from_an_instrument` requires.
#[test]
fn a_multi_bus_instrument_still_renders_its_primary_bus() {
    let _g = lock();
    use tutti_midi_types::MidiEvent;

    let mut au = DLS_SYNTH.open(RATE, BLOCK);
    assert_eq!(
        au.bus_count(BusDirection::Output),
        2,
        "the premise: this unit really does have two output buses"
    );

    au.send_midi(&[MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0xC000,
    )]);

    // No input bus, so render from an empty input.
    let input: Vec<Vec<f32>> = Vec::new();
    let channels = au.num_outputs() as usize;
    let mut loudest = 0.0f32;
    for _ in 0..20 {
        let mut output = silence(channels, BLOCK as usize);
        render(&mut au, &input, &mut output, BLOCK).expect("render on a multi-bus instrument");
        assert!(
            all_finite(&output),
            "a multi-bus instrument produced a non-finite sample"
        );
        loudest = loudest.max(peak(&output));
    }
    assert!(
        loudest > 1e-3,
        "a NoteOn on the two-bus DLSMusicDevice produced no audio (peak \
         {loudest}) across 20 blocks"
    );
}

/// Reading the topology must not disturb the unit it is asked about.
///
/// `bus_count` / `bus_layout` / `supported_channel_configs` are pure reads, but
/// AudioToolbox has no type-level distinction between a property get and a set,
/// and `kAudioUnitProperty_ElementCount` is a *writable* property on units with
/// a dynamic bus topology — so a mistaken `set_property` here would silently
/// reconfigure the AU. This renders, walks the whole topology, and renders
/// again, asserting the audio is unchanged.
#[test]
fn querying_the_topology_does_not_reconfigure_the_unit() {
    let _g = lock();
    use tutti_midi_types::MidiEvent;

    let mut au = DLS_SYNTH.open(RATE, BLOCK);
    let input: Vec<Vec<f32>> = Vec::new();
    let channels = au.num_outputs() as usize;

    let before = (
        au.num_inputs(),
        au.num_outputs(),
        au.sample_rate(),
        au.bus_count(BusDirection::Output),
    );

    au.send_midi(&[MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0xC000,
    )]);
    let mut baseline = 0.0f32;
    for _ in 0..10 {
        let mut output = silence(channels, BLOCK as usize);
        render(&mut au, &input, &mut output, BLOCK).expect("render");
        baseline = baseline.max(peak(&output));
    }
    assert!(
        baseline > 1e-3,
        "the note must sound, or this proves nothing"
    );

    // Walk every query this change adds, including out-of-range ones.
    for direction in BusDirection::ALL {
        let count = au.bus_count(direction);
        for bus in 0..count.saturating_add(2) {
            let _ = au.bus_layout(direction, bus);
        }
    }
    let _ = au.supported_channel_configs();

    assert_eq!(
        (
            au.num_inputs(),
            au.num_outputs(),
            au.sample_rate(),
            au.bus_count(BusDirection::Output)
        ),
        before,
        "walking the bus topology changed the unit's configuration"
    );
    assert!(
        au.is_initialized(),
        "the AU must still be initialized after a topology walk"
    );

    au.send_midi(&[MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        64,
        0xC000,
    )]);
    let mut after = 0.0f32;
    for _ in 0..10 {
        let mut output = silence(channels, BLOCK as usize);
        render(&mut au, &input, &mut output, BLOCK).expect("render after a topology walk");
        after = after.max(peak(&output));
    }
    assert!(
        after > 1e-3,
        "the unit stopped producing audio after its topology was queried \
         (peak {after})"
    );
}
