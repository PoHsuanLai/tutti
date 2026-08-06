//! `kAudioUnitProperty_PresentationLatency` (40) and `_DependentParameters` (45).
//!
//! Two AU properties the host previously had no surface for: one the host
//! **writes** to tell a plugin how far downstream the listener is, one it
//! **reads** to learn which parameters a meta-parameter silently moves.
//!
//! # The corpus cannot witness either, and that is the headline measurement
//!
//! Probed on macOS 15.6 across **all 39 instantiable registered units** — the 35
//! Apple Audio Units plus TDR Nova, TAL Reverb 4 and TAL-NoiseMaker:
//!
//! * `PresentationLatency`: **0 of 39 accept it.** Every one answers
//!   `kAudioUnitErr_InvalidProperty` (-10879), and the refusal survives every
//!   spelling tried — input scope and output scope and global, before and after
//!   `AudioUnitInitialize`, at `Float64` and at `Float32` width.
//!   `AudioUnitGetPropertyInfo` likewise reports the property absent rather than
//!   present-but-unwritable, so the refusal is honest: these units do not
//!   implement it, they are not rejecting a malformed write.
//!
//! * `DependentParameters`: **0 of 39 answer it** — at the global scope, and at
//!   the address of each of the **28 parameters that carry a meta flag**
//!   (AUNBandEQ's 8 per-band "Type" controls, AUGraphicEQ's "Number of Bands",
//!   AUPitch's 9, AURoundTripAAC's 3, AURogerBeep's "Sensitivity", AUNewPitch's
//!   "Spectral Coherence", TDR Nova's 5).
//!
//! That second pair of numbers is the useful finding rather than a null result.
//! 28 real parameters on this machine announce "writing me may silently move
//! others" and not one will say which — so a host cannot treat
//! `DependentParameters` as the mechanism and must treat the *flag* as the
//! signal, invalidating broadly. That is why [`AuParameter::meta`] exists as a
//! carried field rather than being derived from the property read, and it is
//! the assertion [`the_corpus_publishes_meta_parameters_that_name_no_dependents`]
//! pins.
//!
//! # Why the positive paths run against the probe AU
//!
//! Because no installed unit implements either property, a suite resting on the
//! corpus alone could only ever assert that both calls fail — which a host
//! that did nothing at all would satisfy. `support/probe_au.rs` registers an AU
//! that *does* implement both, so the write is observable and the decode is
//! falsifiable. This is the same division `support/corpus.rs` and
//! `support/probe_au.rs` document: Apple's units cover "everything went right",
//! the probe covers everything else.

#![cfg(target_os = "macos")]

mod support;

use support::corpus;
use support::probe_au::{
    last_presentation_latency, Misbehaviour, PROBE_DEPENDENT_PARAMS, PROBE_META_PARAM_ID,
};

use tutti_au_host::bus::BusDirection;
use tutti_au_host::parameters::{self, DependentParam, MetaScope, ParamAddress};
use tutti_au_host::types::{
    K_AUDIO_UNIT_SCOPE_GLOBAL, K_AUDIO_UNIT_SCOPE_INPUT, K_AUDIO_UNIT_SCOPE_OUTPUT,
};
use tutti_types::Seconds;

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// `kAudioUnitErr_InvalidProperty`, the refusal every corpus unit gives to both
/// properties.
const ERR_INVALID_PROPERTY: i32 = -10879;

// ---------------------------------------------------------------------------
// PresentationLatency — the write reaches the AU, at the address it was aimed
// ---------------------------------------------------------------------------

/// The seconds the host writes arrive at the AU unchanged.
///
/// Kills the mutation that matters most on this path: the value is a `Float64`
/// count of *seconds*, and the host holds it as [`Seconds`]. A writer that
/// converted to samples, or that wrote the `f32` bit pattern into an `f64`
/// slot, would still return `noErr` — the probe refuses any width but 8 bytes,
/// and the read-back pins the number rather than merely its presence.
#[test]
fn a_presentation_latency_write_arrives_with_its_seconds_intact() {
    let au = Misbehaviour::None.open_initialized(RATE, BLOCK);

    // Not a round number, and not representable exactly in binary — so a value
    // that survived a detour through samples-and-back, or through an f32, lands
    // somewhere else.
    let written = Seconds(0.031_25 + 1.0 / 3.0);
    au.set_presentation_latency(BusDirection::Output, 0, written)
        .expect("the probe implements PresentationLatency");

    let seen = last_presentation_latency(&au, K_AUDIO_UNIT_SCOPE_OUTPUT, 0)
        .expect("the probe recorded a write at output/0");
    assert_eq!(
        seen,
        f64::from(written.0),
        "the AU received {seen} s, host wrote {} s",
        written.0
    );
}

/// A write aimed at one bus lands on that bus and nowhere else.
///
/// The property is per `(scope, element)` — Apple asks a host to set it "on each
/// active input and output bus" — so a writer that ignored its arguments and
/// always wrote the global scope, or always element 0, would be caught here and
/// nowhere else. The three addresses carry three distinct values, so a writer
/// that collapsed them would have to also produce the right value at each, which
/// last-write-wins makes impossible.
#[test]
fn presentation_latency_is_recorded_per_bus_not_per_unit() {
    let au = Misbehaviour::None.open_initialized(RATE, BLOCK);

    let out0 = Seconds(0.010);
    let out1 = Seconds(0.020);
    let in0 = Seconds(0.005);
    au.set_presentation_latency(BusDirection::Output, 0, out0)
        .expect("probe accepts output/0");
    au.set_presentation_latency(BusDirection::Output, 1, out1)
        .expect("probe accepts output/1");
    au.set_presentation_latency(BusDirection::Input, 0, in0)
        .expect("probe accepts input/0");

    assert_eq!(
        last_presentation_latency(&au, K_AUDIO_UNIT_SCOPE_OUTPUT, 0),
        Some(f64::from(out0.0)),
    );
    assert_eq!(
        last_presentation_latency(&au, K_AUDIO_UNIT_SCOPE_OUTPUT, 1),
        Some(f64::from(out1.0)),
        "output bus 1 must not receive bus 0's value"
    );
    assert_eq!(
        last_presentation_latency(&au, K_AUDIO_UNIT_SCOPE_INPUT, 0),
        Some(f64::from(in0.0)),
        "the input scope must not receive the output scope's value"
    );
    // Nothing was written globally, and `None` is how the probe spells that —
    // distinct from a zero, which would be a real value.
    assert_eq!(
        last_presentation_latency(&au, K_AUDIO_UNIT_SCOPE_GLOBAL, 0),
        None,
        "a bus-addressed write must not leak onto the global scope"
    );
}

/// Zero is a value the host can write, not a value it swallows.
///
/// Apple's header gives zero a meaning — "either no latency or an unknown
/// latency" — so a host that skipped the call for a zero would leave a plugin
/// holding a stale non-zero figure from an earlier chain. The probe's `None`
/// spelling for "never told" is what makes this assertable at all.
#[test]
fn a_zero_presentation_latency_is_still_written() {
    let au = Misbehaviour::None.open_initialized(RATE, BLOCK);

    au.set_presentation_latency(BusDirection::Output, 0, Seconds(0.5))
        .expect("probe accepts the first write");
    au.set_presentation_latency(BusDirection::Output, 0, Seconds(0.0))
        .expect("probe accepts the zero write");

    assert_eq!(
        last_presentation_latency(&au, K_AUDIO_UNIT_SCOPE_OUTPUT, 0),
        Some(0.0),
        "the zero must overwrite the earlier 0.5, not be skipped"
    );
}

/// Every registered AU on this machine refuses the property, and the host
/// reports that refusal rather than absorbing it.
///
/// The measurement is in the module docs: 0 of 39 accept it. This pins the
/// *host's* half — that the refusal reaches the caller as an `Err` carrying the
/// AU's own status. A host that flattened it to `Ok(())` would leave every
/// caller's error handling unreachable, which is the bug `get_latency` carried
/// before it was fixed.
#[test]
fn every_corpus_unit_refuses_presentation_latency_and_the_refusal_is_reported() {
    let mut checked = 0usize;
    for unit in corpus::EFFECTS.iter().chain(corpus::INSTRUMENTS) {
        let au = unit.open(RATE, BLOCK);
        let err = au
            .set_presentation_latency(BusDirection::Output, 0, Seconds(0.01))
            .expect_err(&format!(
                "{} accepted PresentationLatency — the corpus measurement in this \
                 module's docs (0 of 39) no longer holds and must be re-taken",
                unit.label
            ));
        match err {
            tutti_au_host::AuError::OsStatus { code, .. } => assert_eq!(
                code, ERR_INVALID_PROPERTY,
                "{} refused with {code}, expected -10879",
                unit.label
            ),
            other => panic!("{}: unexpected error shape {other:?}", unit.label),
        }
        checked += 1;
    }
    // Guards against the vacuous pass: an empty corpus slice would satisfy every
    // assertion above by never running one.
    assert_eq!(
        checked,
        corpus::EFFECTS.len() + corpus::INSTRUMENTS.len(),
        "not every corpus unit was reached"
    );
}

// ---------------------------------------------------------------------------
// DependentParameters — the decode, and the three-state answer
// ---------------------------------------------------------------------------

/// The dependents decode in order, with each entry's scope and id in the right
/// halves.
///
/// `AUDependentParameter` is two adjacent `u32`s, so a transposed decode is
/// invisible to the type system and to any fixture whose scopes and ids happen
/// to coincide. The probe's table is deliberately neither scope-uniform nor
/// id-sorted (see `PROBE_DEPENDENT_PARAMS`), so a transposition and a reordering
/// both fail here.
#[test]
fn dependent_parameters_decode_in_order_with_scope_and_id_unswapped() {
    let au = Misbehaviour::None.open_initialized(RATE, BLOCK);

    let got = au
        .dependent_parameters(PROBE_META_PARAM_ID)
        .expect("the probe implements DependentParameters for its meta parameter");

    let want: Vec<DependentParam> = PROBE_DEPENDENT_PARAMS
        .iter()
        .map(|&(scope, id)| DependentParam { scope, id })
        .collect();
    assert_eq!(got, want);
}

/// "The AU did not say" and "the AU said nothing depends on it" stay distinct.
///
/// This is the whole reason the return type is `Option<Vec<_>>` and not `Vec<_>`.
/// A host caching parameter ranges must re-read everything on the first and may
/// skip on the second; collapsing them turns "I cannot tell you" into "there is
/// nothing to tell", and the stale range is then never refreshed. The probe
/// answers for exactly one parameter id, so its neighbours give the `None`.
#[test]
fn an_unanswered_dependents_query_is_none_not_an_empty_list() {
    let au = Misbehaviour::None.open_initialized(RATE, BLOCK);

    assert!(
        au.dependent_parameters(PROBE_META_PARAM_ID).is_some(),
        "control: the meta parameter must answer, or the None below proves nothing"
    );

    let non_meta = PROBE_META_PARAM_ID + 1;
    assert_eq!(
        au.dependent_parameters(non_meta),
        None,
        "a parameter the AU declines to answer for must be None, never Some(vec![])"
    );
}

/// The query is addressed with the parameter id in the **element** position.
///
/// `DependentParameters` shares that addressing quirk with `ParameterInfo` and
/// `ParameterValueStrings` — see `parameters::info_at`. A reader that passed
/// `addr.element` through instead would query whatever parameter shares that
/// number. The probe answers only at `(global, PROBE_META_PARAM_ID)`, so a
/// reader using the element field reaches an address the probe refuses.
#[test]
fn the_dependents_query_carries_the_parameter_id_in_the_element_position() {
    let au = Misbehaviour::None.open_initialized(RATE, BLOCK);

    // A non-zero element with the meta id: the id must still be what selects the
    // answer, so this succeeds and returns the same table.
    let addr = ParamAddress {
        scope: K_AUDIO_UNIT_SCOPE_GLOBAL,
        element: 9,
    };
    let got = parameters::dependents_of_at(au.raw_unit(), addr, PROBE_META_PARAM_ID)
        .expect("the id selects the answer, not the element");
    assert_eq!(got.len(), PROBE_DEPENDENT_PARAMS.len());
}

/// A trailing partial entry is dropped, not decoded from memory past the array.
///
/// The property is an array of a fixed-width struct, so a byte count that is not
/// a multiple of 8 is the AU contradicting itself — but nothing validates it and
/// the host sizes its buffer from the AU's own `GetPropertyInfo`. Walking that
/// buffer in 8-byte steps without requiring whole entries reads 4 bytes of the
/// fragment plus 4 bytes past the end and reports them as a real dependent.
///
/// This test exists because the mutation it kills — `chunks_exact` relaxed to
/// `chunks` — **survived** against the corpus and against a well-behaved probe:
/// every honest answer is a whole multiple, so nothing in the suite could tell
/// the two apart. Rather than assert something weaker, the ragged answer was
/// added to the probe so the distinction has an input that witnesses it.
#[test]
fn a_ragged_dependents_array_drops_the_partial_entry() {
    let au = Misbehaviour::RaggedDependentParameters.open_initialized(RATE, BLOCK);

    let got = au
        .dependent_parameters(PROBE_META_PARAM_ID)
        .expect("the probe answers, it just answers raggedly");

    let want: Vec<DependentParam> = PROBE_DEPENDENT_PARAMS
        .iter()
        .map(|&(scope, id)| DependentParam { scope, id })
        .collect();
    assert_eq!(
        got, want,
        "the 4-byte tail must be dropped, not decoded into a fourth dependent"
    );
}

// ---------------------------------------------------------------------------
// The meta flag itself — the signal a host actually gets on this machine
// ---------------------------------------------------------------------------

/// The meta flag is read off exactly the parameter that carries it.
///
/// A decoder that masked the wrong bit, or that reported every parameter meta,
/// passes a test that only checks the flagged one. The probe flags parameter 0
/// and leaves the rest clear, so both directions fail.
#[test]
fn the_meta_flag_is_read_off_the_parameter_that_carries_it() {
    let au = Misbehaviour::None.open_initialized(RATE, BLOCK);

    let params = au.get_parameter_list();
    assert!(
        params.len() >= 2,
        "need a flagged and an unflagged parameter to tell the two apart"
    );

    for p in &params {
        let want = if p.id == PROBE_META_PARAM_ID {
            Some(MetaScope::Global)
        } else {
            None
        };
        assert_eq!(p.meta, want, "parameter {} ({})", p.id, p.name);
    }
}

/// The corpus's 28 meta-flagged parameters name no dependents — the finding this
/// whole item turns on.
///
/// A host cannot use `DependentParameters` as its staleness mechanism on this
/// machine, because every unit that admits to having meta-parameters declines to
/// enumerate them. So the flag has to be the signal, and this test pins both
/// halves of that: the flags are present *and* the property is absent. If a
/// future macOS starts answering, this fails and the host gains a cheaper path
/// — which is a result worth being told about, not a regression.
#[test]
fn the_corpus_publishes_meta_parameters_that_name_no_dependents() {
    // AUNBandEQ's 8 per-band "Type" controls are the largest meta cluster among
    // Apple's units, and the one that is unambiguously a real meta-parameter:
    // changing a band's filter type changes which of that band's other
    // parameters apply.
    let au = corpus::N_BAND_EQ.open(RATE, BLOCK);

    let metas: Vec<_> = au
        .get_parameter_list()
        .into_iter()
        .filter(|p| p.meta.is_some())
        .collect();
    assert_eq!(
        metas.len(),
        8,
        "AUNBandEQ published {} meta-flagged parameters, measured 8 on macOS 15.6",
        metas.len()
    );

    for p in &metas {
        assert_eq!(
            au.dependent_parameters(p.id),
            None,
            "AUNBandEQ parameter {} ('{}') answered DependentParameters — the \
             measurement in this module's docs (0 of 39) no longer holds",
            p.id,
            p.name
        );
    }
}

/// A parameter with no meta flag reports `None`, on a real unit.
///
/// The corpus half of the flag decode: the probe pins that the bit is read
/// correctly, this pins that a real AU's ordinary parameters are not swept up by
/// it. AULowpass has two parameters and neither is meta.
#[test]
fn an_ordinary_corpus_parameter_carries_no_meta_scope() {
    let au = corpus::LOWPASS.open(RATE, BLOCK);
    let params = au.get_parameter_list();
    assert!(!params.is_empty(), "AULowpass publishes parameters");
    for p in &params {
        assert_eq!(
            p.meta, None,
            "AULowpass '{}' is not a meta parameter",
            p.name
        );
    }
}

// ---------------------------------------------------------------------------
// ABI
// ---------------------------------------------------------------------------

/// `AUDependentParameter` is two `u32`s in `(scope, id)` order, 8 bytes.
///
/// The decode reads the struct out of a byte buffer, so a width or ordering
/// change in the binding silently reinterprets every entry. Pinned here rather
/// than trusted, the same way `parameters::tests` pins the hand-declared
/// `ClumpNameRequest` layout.
#[test]
fn dependent_parameter_matches_the_c_abi() {
    use tutti_au_host::types::AUDependentParameter;

    assert_eq!(std::mem::size_of::<AUDependentParameter>(), 8);
    assert_eq!(std::mem::align_of::<AUDependentParameter>(), 4);

    let probe = AUDependentParameter {
        mScope: K_AUDIO_UNIT_SCOPE_INPUT,
        mParameterID: 0xDEAD_BEEF,
    };
    // Field order, not just size: a swapped binding would still be 8 bytes.
    let bytes: [u8; 8] = unsafe { std::mem::transmute(probe) };
    assert_eq!(
        u32::from_ne_bytes(bytes[0..4].try_into().unwrap()),
        K_AUDIO_UNIT_SCOPE_INPUT,
        "mScope must be the first word"
    );
    assert_eq!(
        u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
        0xDEAD_BEEF,
        "mParameterID must be the second word"
    );
}
