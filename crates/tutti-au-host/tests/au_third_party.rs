//! Third-party AU conformance — the half of the contract Apple's units hide.
//!
//! Every other suite in this crate runs against the ~30 Audio Units that ship
//! with macOS, for the good reasons `support/corpus.rs` gives: they are part of
//! the OS, so their absence is a broken environment rather than a missing
//! optional install. The cost of that choice is the whole reason this file
//! exists: **Apple's units are unusually well-behaved**, and a host validated
//! only against them passes while being wrong about real plugins.
//!
//! Each of these was found by running the host against three third-party units
//! and is invisible to the entire Apple corpus:
//!
//! * **An infinite tail time.** TAL Reverb 4 answers
//!   `kAudioUnitProperty_TailTime` with `f64::INFINITY`; no Apple unit exceeds
//!   ~21 s. `Seconds::to_samples` maps non-finite to zero, so "infinite tail"
//!   and "no tail" become the same number downstream.
//! * **An instrument with input channels.** Every Apple instrument reports 0
//!   inputs; TAL-NoiseMaker reports 2. A host that infers the bus topology from
//!   the `aumu` type code instead of asking gets this one wrong.
//! * **`AudioUnitProcess` universally refused.** 7 Apple effects implement the
//!   push-render selector; all three third-party units answer `unimpErr` (-4).
//! * **A real sidechain bus.** TDR Nova and TAL Reverb 4 each publish a second
//!   input element *named* "Sidechain". `support/corpus.rs` records that there is
//!   no working AU sidechain among Apple's units at all.
//!
//! ## Installed units only
//!
//! The units are an optional install, so this target is compiled only with the
//! `installed-units` feature (`cargo xtask test-au-installed`); CI lints it. With
//! the feature on, a missing unit **fails** the case that needs it with the
//! component codes in the message ([`absent`]), so a green run has exercised
//! every unit. The codes are printed because the failure to rule out is a wrong
//! code, not a missing plugin: check them against `auval -a`.
//!
//! ## What the tests pin
//!
//! A test that cannot fail is worse than no test, so each assertion below pins one
//! behaviour of `src/`.
//!
//! The oversized-block test names `AuError::InvalidBuffer` specifically rather than "some
//! error": without the host guard the render still fails, but with the *AU's*
//! `kAudioUnitErr_TooManyFramesToProcess` (-10874), which arrives only after the host has
//! already built a buffer list whose `mDataByteSize` overstates storage it allocated for
//! `BLOCK` frames. A test that accepted any error would pass on the buggy host.
//!
//! ## Running
//!
//! ```bash
//! cargo xtask test-au-installed
//! ```

#![cfg(all(target_os = "macos", feature = "installed-units"))]

use std::sync::Mutex;
use tutti_fixtures::main_thread;

mod support;
use support::corpus::{
    INFINITE_TAIL_UNIT, NOISEMAKER_IDLE_FLOOR, NOISEMAKER_NOTE_PEAK, TAL_NOISEMAKER, TDR_NOVA,
    THIRD_PARTY, THIRD_PARTY_LATENCY, THIRD_PARTY_NAMED_ELEMENTS, THIRD_PARTY_PARAM_COUNTS,
    THIRD_PARTY_PRESET_COUNTS, THIRD_PARTY_VERSION, THIRD_PARTY_WITHOUT_OPTIONAL_PROPS,
    THIRD_PARTY_WITHOUT_PUSH_RENDER, ThirdPartyRef, UNIMP_ERR, all_finite, peak, render, silence,
    sine,
};

use crate::support::au::Au;
use tutti_au_host::component::AuComponentInfo;
use tutti_au_host::{AuError, BusDirection};
use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_ump::{MidiChannel, MidiGroup};

/// AudioToolbox tolerates concurrent use of *distinct* units, but component
/// discovery walks a process-global registry and these plugins load shared
/// bundles into the process. Serializing keeps one test's instantiate from racing
/// another's enumeration. Mirrors `AU_LOCK` in `au_conformance.rs`.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// `AU_LOCK` is poisoned by any panicking test, and a poisoned lock would convert
/// one real failure into N spurious ones. The guard is only a serializer — there
/// is no shared state to be left inconsistent — so recover.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: tutti_types::SampleRate = tutti_types::SampleRate::SR_48K;
const BLOCK: u32 = 512;

/// Fails a case whose unit is not installed, naming its component codes.
fn absent(unit: &ThirdPartyRef, what: &str) -> ! {
    panic!(
        "{} ({}/{}) is not installed, so {what} cannot run. This target is built \
         only with `installed-units`, which asks for the units. Verify the codes \
         against `auval -a` before assuming absence: a wrong subtype fails the \
         same way.",
        unit.label,
        String::from_utf8_lossy(unit.sub_type),
        String::from_utf8_lossy(unit.manufacturer),
    );
}

/// Resolves every unit in `table`, calling `f` for each; returns how many.
fn each<T: Copy>(
    table: &[(ThirdPartyRef, T)],
    what: &str,
    mut f: impl FnMut(&ThirdPartyRef, &AuComponentInfo, T),
) -> usize {
    for (unit, expected) in table {
        let info = unit.find().unwrap_or_else(|| absent(unit, what));
        f(unit, &info, *expected);
    }
    table.len()
}

/// As [`each`], for a table with no expectation column.
fn each_unit(
    table: &[ThirdPartyRef],
    what: &str,
    mut f: impl FnMut(&ThirdPartyRef, &AuComponentInfo),
) -> usize {
    for unit in table {
        let info = unit.find().unwrap_or_else(|| absent(unit, what));
        f(unit, &info);
    }
    table.len()
}

/// Instantiate `info` initialized, as `corpus::open_info` does for the ramp test.
fn open(info: &AuComponentInfo, label: &str) -> Au {
    let mut au = Au::new(info.component, RATE, BLOCK)
        .unwrap_or_else(|e| panic!("{label}: instantiate failed: {e:?}"));
    au.initialize()
        .unwrap_or_else(|e| panic!("{label}: initialize failed: {e:?}"));
    au
}

// ------------------------------------------------------------------- census

/// The corpus table is non-empty and internally consistent (no duplicate codes,
/// which would mean two entries silently testing one unit), and every unit
/// instantiates and reports the type its code claims. A `Td5a` that enumerated
/// under `Instrument` would be a corpus bug, not a plugin one.
#[test]
fn the_third_party_corpus_is_accounted_for() {
    let _g = lock();

    assert!(
        !THIRD_PARTY.is_empty(),
        "the third-party corpus is empty, so this whole suite is vacuous"
    );

    // Duplicate codes would mean two rows testing one unit while appearing to
    // cover two — a corpus bug no behavioural test could reveal.
    for (i, a) in THIRD_PARTY.iter().enumerate() {
        for b in &THIRD_PARTY[i + 1..] {
            assert!(
                !(a.sub_type == b.sub_type && a.manufacturer == b.manufacturer),
                "{} and {} carry the same codes ({}/{}) — one of them is a typo, \
                 and the corpus covers one fewer unit than it appears to",
                a.label,
                b.label,
                String::from_utf8_lossy(a.sub_type),
                String::from_utf8_lossy(a.manufacturer),
            );
        }
    }

    let mut found = Vec::new();
    for unit in THIRD_PARTY {
        match unit.find() {
            Some(info) => {
                // The enumeration scope is chosen by `au_type`, so a match proves
                // the type code agrees with where it was found. Assert the
                // instance reports it too: those are read from different places.
                let au = open(&info, unit.label);
                assert_eq!(
                    au.au_type(),
                    unit.au_type,
                    "{}: enumerated as {:?} but the instance reports {:?}",
                    unit.label,
                    unit.au_type,
                    au.au_type(),
                );
                found.push(unit.label);
            }
            None => absent(unit, "the census"),
        }
    }

    eprintln!(
        "third-party corpus: {}/{} installed — found [{}]",
        found.len(),
        THIRD_PARTY.len(),
        found.join(", "),
    );
}

// -------------------------------------------------------------- the tail

/// An infinite tail time must reach the caller **as infinite**, and the loss must
/// happen at the unit conversion rather than at the property read.
///
/// If the host clamped `inf` at the read — to zero, or to some "sane" maximum — it
/// would look correct and be unable to tell a reverb with infinite decay from one
/// with no tail at all. Those need opposite handling at bounce time: one wants a
/// user-chosen fade, the other wants nothing.
///
/// The finding this pins is that the collapse is real but happens *later*:
/// `Seconds::to_samples` maps every non-finite input to `Samples::ZERO` (stated in
/// its own docs, so a caller can check `is_finite()` first). So a bounce that
/// sizes its tail with `to_samples` and does not check gets **zero** tail samples
/// for the one unit whose tail is unbounded — it truncates the reverb entirely.
/// Measured: TAL Reverb 4 answers `f64::INFINITY`; no Apple unit exceeds ~21 s.
#[test]
fn an_infinite_tail_survives_the_host_and_collapses_in_conversion() {
    let _g = lock();

    // The conversion's behaviour is a property of the unit vocabulary, not of any
    // plugin: the half a host author has to know about.
    let inf = tutti_types::Seconds(f64::INFINITY);
    assert!(
        !inf.get().is_finite(),
        "Seconds must carry a non-finite value without normalizing it, or the \
         host could not report an infinite tail at all"
    );
    assert_eq!(
        inf.to_samples(RATE).get(),
        0,
        "Seconds::to_samples must map non-finite to zero (its documented \
         behaviour). If this changes, the bounce-truncation hazard this test \
         describes has changed shape and the docs on INFINITE_TAIL_UNIT need \
         revisiting."
    );

    let unit = INFINITE_TAIL_UNIT;
    let info = unit
        .find()
        .unwrap_or_else(|| absent(&unit, "the infinite-tail read"));

    let au = open(&info, unit.label);
    let tail = au
        .get_tail_time()
        .unwrap_or_else(|e| panic!("{}: tail time failed: {e:?}", unit.label));

    assert!(
        tail.get().is_infinite() && tail.get() > 0.0,
        "{}: reported a tail of {:?}, but this unit is in the corpus precisely \
         because it answers kAudioUnitProperty_TailTime with +infinity. A finite \
         value here means either the host started clamping — which erases the \
         difference between an unbounded tail and no tail — or the plugin \
         changed, in which case INFINITE_TAIL_UNIT needs a new subject because \
         nothing else on this machine reports a non-finite tail.",
        unit.label,
        tail.get(),
    );

    // And the downstream consequence, on the real value rather than a literal.
    assert_eq!(
        tail.to_samples(RATE).get(),
        0,
        "{}: an infinite tail converts to zero tail samples — a bounce that does \
         not check is_finite() first truncates this reverb completely",
        unit.label,
    );
}

// -------------------------------------------------------- parameters / state

/// The full parameter list must come back at its measured width, for units far
/// wider than any Apple AU.
///
/// If it regresses, a plugin's parameter list is silently truncated: the missing
/// parameters cannot be automated, do not appear in a generic UI, and are dropped
/// from saved state. 75 and 88 parameters are both well past AUNBandEQ's 41 — the
/// widest Apple surface — so these are the only rows that exercise a list long
/// enough for a `CFArray` walk to lose its tail.
///
/// Counts are pinned rather than checked non-empty for the reason
/// `PRESET_EFFECTS` documents: every off-by-one passes "more than zero".
#[test]
fn wide_parameter_lists_are_read_whole() {
    let _g = lock();

    let found = each(
        THIRD_PARTY_PARAM_COUNTS,
        "its wide parameter list",
        |unit, info, expected| {
            let au = open(info, unit.label);
            let params = au.get_parameter_list();
            assert_eq!(
                params.len(),
                expected,
                "{}: reported {} parameters, measured {expected} on macOS 15.6. \
                 A short count is a truncated CFArray walk — those parameters \
                 become unautomatable and drop out of saved state.",
                unit.label,
                params.len(),
            );

            // Ids must be distinct: a duplicated id would make two parameters
            // alias, so writing one moves the other, and the count above would
            // still pass.
            let mut ids: Vec<u32> = params.iter().map(|p| p.id).collect();
            ids.sort_unstable();
            let before = ids.len();
            ids.dedup();
            assert_eq!(
                ids.len(),
                before,
                "{}: the parameter list contains duplicate ids, so two entries \
                 address one parameter",
                unit.label,
            );

            // Every name must survive `cfstring_to_string_checked`. These are
            // plugin-supplied strings, and the short ones are arm64 tagged
            // pointers — the case an alignment-only guard silently dropped.
            for p in &params {
                assert!(
                    !p.name.is_empty(),
                    "{}: parameter {} has an empty name — a plugin-supplied \
                     CFString was dropped by the checked converter",
                    unit.label,
                    p.id,
                );
                assert!(
                    p.range.min.is_finite() && p.range.max.is_finite(),
                    "{}: parameter {} ({}) has a non-finite range {:?}, which a \
                     UI cannot draw and a clamp cannot use",
                    unit.label,
                    p.id,
                    p.name,
                    p.range,
                );
            }
        },
    );
    eprintln!("wide_parameter_lists_are_read_whole: exercised {found} unit(s)");
}

/// `get_state`/`set_state` must restore the parameter values that were saved.
///
/// If it regresses, reopening a project silently loses every plugin setting —
/// the single most damaging class of host bug, because it is invisible until the
/// user's work is already gone.
///
/// Note the tolerance, which is a measured third-party quirk rather than
/// slack: TDR Nova **quantizes** some parameters on restore (measured: 0.73
/// written to "Band 1 Frequency" comes back 0.7311007) because it round-trips
/// through its own internal representation. TAL Reverb 4 and TAL-NoiseMaker
/// restore bit-exactly. So an exact-equality assertion would fail on a correct
/// host, and the useful claim is that the restored value tracks the *saved* one
/// rather than the perturbed one.
#[test]
fn state_round_trips_through_the_plugins_own_blob() {
    let _g = lock();

    // How far a restored value may sit from the saved one. Measured worst case is
    // TDR Nova's 0.0011007 on "Band 1 Frequency"; this is an order of magnitude
    // above it and still ~50x below the 0.52 gap to the perturbed value, so the
    // test cannot pass by restoring the wrong one.
    const QUANTIZE_TOLERANCE: f32 = 0.02;

    let found = each_unit(THIRD_PARTY, "its state round-trip", |unit, info| {
        let mut au = open(info, unit.label);
        let params = au.get_parameter_list();
        let writable: Vec<_> = params
            .iter()
            .filter(|p| p.writable && !p.meter_read_only)
            .take(6)
            .collect();
        assert!(
            !writable.is_empty(),
            "{}: no writable parameters, so this test proves nothing here",
            unit.label
        );

        // Move each to 73% of range, save, move to 21%, restore, compare.
        let saved_values: Vec<(u32, f32)> = writable
            .iter()
            .map(|p| {
                let target = p.range.min + (p.range.max - p.range.min) * 0.73;
                au.set_parameter(p.id, target)
                    .unwrap_or_else(|e| panic!("{}: set {} failed: {e:?}", unit.label, p.id));
                (p.id, au.get_parameter(p.id).unwrap())
            })
            .collect();

        let blob = au
            .get_state(&main_thread())
            .unwrap_or_else(|e| panic!("{}: get_state failed: {e:?}", unit.label));
        assert!(
            !blob.is_empty(),
            "{}: get_state returned an empty blob, so nothing was saved",
            unit.label
        );

        for p in &writable {
            let other = p.range.min + (p.range.max - p.range.min) * 0.21;
            au.set_parameter(p.id, other).unwrap();
        }
        // Confirm the perturbation actually moved something, or the restore
        // assertion below could pass without a restore having happened.
        let moved = saved_values
            .iter()
            .any(|(id, saved)| (au.get_parameter(*id).unwrap() - saved).abs() > QUANTIZE_TOLERANCE);
        assert!(
            moved,
            "{}: perturbing the parameters changed nothing, so the restore below \
             would pass even if set_state did nothing at all",
            unit.label
        );

        au.set_state(&main_thread(), &blob)
            .unwrap_or_else(|e| panic!("{}: set_state failed: {e:?}", unit.label));

        for (id, saved) in &saved_values {
            let restored = au.get_parameter(*id).unwrap();
            assert!(
                (restored - saved).abs() <= QUANTIZE_TOLERANCE,
                "{}: parameter {id} was saved at {saved} but restored to \
                 {restored}. Reopening a project loses this setting. (TDR Nova \
                 quantizes by up to 0.0011 on restore, which is why this is a \
                 tolerance and not an equality — but {} exceeds it.)",
                unit.label,
                (restored - saved).abs(),
            );
        }

        // The AU must still render after a state load: a blob that half-applies
        // can leave a plugin in a state where it produces NaN.
        assert_renders_finite(&mut au, unit.label, "after set_state");
    });
    eprintln!("state_round_trips_through_the_plugins_own_blob: exercised {found} unit(s)");
}

/// A state blob from a **different plugin** must be refused, and the AU left
/// renderable.
///
/// If it regresses, loading a project whose plugin was replaced feeds one
/// plugin's opaque blob to another. Measured on macOS 15.6: TDR Nova answers
/// `kAudioUnitErr_InvalidPropertyValue` (-10851) to TAL-NoiseMaker's blob, so the
/// refusal is the plugin's own — but the host must propagate it rather than
/// reporting success, and must leave the unit usable.
///
/// This is the `ClassInfo` counterpart to the `.aupreset` identity check, which
/// the host performs *itself* because — as `src/aupreset.rs` documents — an AU
/// handed a dictionary bearing its own identity keys but another plugin's `data`
/// accepts it and adopts nonsense.
#[test]
fn a_foreign_state_blob_is_refused_and_leaves_the_unit_usable() {
    let _g = lock();

    let find = |unit: ThirdPartyRef| {
        unit.find()
            .unwrap_or_else(|| absent(&unit, "the cross-plugin state rejection check"))
    };
    let (nova, nm) = (find(TDR_NOVA), find(TAL_NOISEMAKER));

    let nm_blob = open(&nm, TAL_NOISEMAKER.label)
        .get_state(&main_thread())
        .expect("NoiseMaker get_state");
    let mut nova_au = open(&nova, TDR_NOVA.label);

    let err = nova_au
        .set_state(&main_thread(), &nm_blob)
        .expect_err("TDR Nova must not accept TAL-NoiseMaker's ClassInfo blob");
    // The status is the plugin's, not one the host invented — measured -10851.
    assert!(
        matches!(err, AuError::OsStatus { .. }),
        "expected the AU's own OSStatus refusal, got {err:?}"
    );

    assert_renders_finite(&mut nova_au, TDR_NOVA.label, "after a refused foreign blob");
}

// ------------------------------------------------------------ render / latency

/// Reported latency must match the measured value, and must **not** be rescaled
/// when the sample rate changes.
///
/// If it regresses, plugin delay compensation is wrong by the error: tracks drift
/// out of alignment by 184 samples (3.8 ms) on any chain containing TDR Nova.
///
/// The rate-independence half is the third-party finding.
/// `kAudioUnitProperty_Latency` is a `Float64` in **seconds**, and the host
/// multiplies by the sample rate — so a plugin reporting a fixed *duration* would
/// yield a different sample count at 44.1 kHz. TDR Nova reports a fixed duration
/// that happens to convert to 184 samples at 48 kHz, and measured **184 again at
/// 44.1 kHz**, meaning it adjusts its reported seconds when the rate changes. A
/// host that cached the seconds value across a rate change would be wrong here;
/// The reported version matches what the bundle declares.
///
/// `AudioComponentGetVersion` packs `major.minor.dot` one byte each, which the
/// header's `0xMMMMmmDD` does not make obvious — it reads as a 16-bit major.
/// The two decodings agree on every unit installed here, because all their
/// majors are single-digit, so the encoding cannot be settled from the raw value
/// alone. `CFBundleShortVersionString` is the independent source, and
/// [`THIRD_PARTY_VERSION`] carries what it says.
///
/// Apple's units cannot pin this: they all report `1.6.0`, so a decode that
/// swapped minor and dot would agree with itself across the whole Apple corpus.
/// These three have distinct values in all three fields.
///
/// Read off the component, not an instance — the version is a property of the
/// registered component, so this needs no instantiation.
#[test]
fn reported_version_matches_the_bundle() {
    let _g = lock();

    let found = each(
        THIRD_PARTY_VERSION,
        "its reported version",
        |unit, info, expected| {
            assert_eq!(
                info.version, expected,
                "{}: AudioComponentGetVersion decoded to {:?} but the bundle \
                 declares {expected:?} in CFBundleShortVersionString. Either the \
                 byte layout is wrong or the plugin was updated — check the \
                 bundle before changing the table.",
                unit.label, info.version,
            );
        },
    );
    eprintln!("checked {found} third-party versions");
}

/// re-reading the property is what makes it right.
#[test]
fn reported_latency_matches_and_survives_a_rate_change() {
    let _g = lock();

    let found = each(
        THIRD_PARTY_LATENCY,
        "its reported latency",
        |unit, info, expected| {
            let mut au = open(info, unit.label);
            let got = au
                .get_latency()
                .unwrap_or_else(|e| panic!("{}: get_latency failed: {e:?}", unit.label));
            assert_eq!(
                got, expected,
                "{}: reports {got:?} of latency at {RATE} Hz, measured \
                 {expected:?} on macOS 15.6. PDC is wrong by the difference, so \
                 every track through this plugin drifts.",
                unit.label,
            );

            // Re-read after a rate change rather than trusting a cached value.
            au.uninitialize().expect("uninitialize for rate change");
            au.set_sample_rate(tutti_types::SampleRate::SR_44K1)
                .unwrap_or_else(|e| panic!("{}: set_sample_rate failed: {e:?}", unit.label));
            au.initialize().expect("re-initialize at 44.1 kHz");
            let at_44 = au.get_latency().expect("latency at 44.1 kHz");
            assert_eq!(
                at_44, expected,
                "{}: reports {at_44:?} at 44.1 kHz but {expected:?} at 48 \
                 kHz. Measured equal on macOS 15.6 — this plugin reports a fixed \
                 sample count, so a host that scaled a cached seconds value \
                 across the rate change would produce {at_44:?} here and \
                 mis-compensate.",
                unit.label,
            );
        },
    );
    eprintln!("reported_latency_matches_and_survives_a_rate_change: exercised {found} unit(s)");
}

/// Every third-party unit must render finite audio, and an oversized block must
/// be refused before the AU sees it.
///
/// If the render half regresses, the plugin outputs NaN and poisons the whole
/// mix bus — one NaN sample propagates through every downstream sum. If the bound
/// half regresses, the host hands the AU a buffer list whose `mDataByteSize`
/// overstates storage it actually allocated, which is a heap overflow inside the
/// plugin rather than an error.
///
/// **Both SMALL and FINITE are asserted**, and the order matters: `peak` folds
/// with `f32::max`, which returns the non-NaN operand, so an all-NaN buffer has a
/// peak of exactly 0.0. A "peak is small" check alone therefore *passes* on
/// totally corrupt output — verified: `fold(0.0, f32::max)` over `[NaN; 8]` is
/// 0.0.
#[test]
fn every_unit_renders_finite_audio_and_refuses_an_oversized_block() {
    let _g = lock();

    let found = each_unit(THIRD_PARTY, "its render path", |unit, info| {
        let mut au = open(info, unit.label);
        assert_renders_finite(&mut au, unit.label, "on a steady sine");

        // An oversized render must be refused by the host's own bound check,
        // before AudioToolbox is reached — the AU's own
        // kAudioUnitErr_TooManyFramesToProcess would come too late, since the
        // buffer list has already been built against storage the host sized for
        // BLOCK frames.
        let oversized = BLOCK * 8;
        let input = sine(2, oversized as usize, 0, 0.5, RATE.get() as f32);
        let mut output = silence(2, oversized as usize);
        let err = render(&mut au, &input, &mut output, oversized)
            .expect_err("a render wider than the configured block must be refused");
        assert!(
            matches!(err, AuError::InvalidBuffer(_)),
            "{}: expected the host's own InvalidBuffer refusal for {oversized} \
             frames at block size {BLOCK}, got {err:?}. The host must refuse \
             before handing the AU a buffer list that overstates its storage.",
            unit.label,
        );
    });
    eprintln!(
        "every_unit_renders_finite_audio_and_refuses_an_oversized_block: exercised {found} unit(s)"
    );
}

/// Render a sine through `au` and assert the output is both bounded and finite.
///
/// Shared because several tests need it after a state change. The FINITE half is
/// not redundant with the bounded half: see
/// `every_unit_renders_finite_audio_and_refuses_an_oversized_block` for why
/// `peak` cannot see an all-NaN buffer.
fn assert_renders_finite(au: &mut Au, label: &str, when: &str) {
    let ch_in = au.num_inputs().max(1) as usize;
    let ch_out = au.num_outputs() as usize;
    let input = sine(ch_in, BLOCK as usize, 0, 0.5, RATE.get() as f32);
    let mut output = silence(ch_out, BLOCK as usize);

    au.reset().ok();
    // Several blocks: a reverb's first block can be near-silent while its steady
    // state is where a coefficient bug shows.
    for _ in 0..8 {
        render(au, &input, &mut output, BLOCK)
            .unwrap_or_else(|e| panic!("{label}: render {when} failed: {e:?}"));
    }

    assert!(
        all_finite(&output),
        "{label}: produced non-finite samples {when}. One NaN poisons every \
         downstream sum on the mix bus. (Checked before the peak assertion \
         because peak() folds with f32::max and reports 0.0 for an all-NaN \
         buffer.)"
    );
    let p = peak(&output);
    assert!(
        p <= 8.0,
        "{label}: peak {p} {when} — a 0.5-amplitude sine in should not produce \
         16x gain, which indicates runaway feedback"
    );
}

// ---------------------------------------------------------- buses / sidechain

/// A named bus must report the name the plugin gave it, and a nonexistent bus
/// must stay distinguishable from a nameless one.
///
/// If it regresses, a sidechain input is indistinguishable from a second audio
/// input in the host UI, and a user patching a compressor's key input has no way
/// to tell which bus to use.
///
/// This is where the Apple corpus is thinnest: `support/corpus.rs` records that
/// **every Apple mixer answers -10850 for all of its real buses** — Apple's units
/// publish essentially no element names — so DLSMusicDevice's two outputs are the
/// only Apple data point. Both TDR Nova and TAL Reverb 4 publish a second input
/// element actually named "Sidechain", which no Apple unit provides at all.
#[test]
fn plugin_named_buses_including_sidechains_are_reported() {
    let _g = lock();

    let mut checked = 0usize;
    for (unit, is_output, element, expected) in THIRD_PARTY_NAMED_ELEMENTS {
        let info = unit
            .find()
            .unwrap_or_else(|| absent(unit, "the named-bus check"));
        let au = open(&info, unit.label);
        let direction = if *is_output {
            BusDirection::Output
        } else {
            BusDirection::Input
        };
        let got = au.element_name(direction, *element).unwrap_or_else(|e| {
            panic!(
                "{}: {} element {element} should be named {expected:?}: {e:?}",
                unit.label,
                if *is_output { "output" } else { "input" },
            )
        });
        assert_eq!(
            got,
            *expected,
            "{}: {} element {element} is named {got:?}, measured {expected:?}",
            unit.label,
            if *is_output { "output" } else { "input" },
        );
        checked += 1;

        // One past the last real bus must be InvalidElement (-10877), not a
        // clamped read of the last valid bus. A host that clamped would show
        // "Sidechain" for a bus that does not exist and size a buffer for it.
        let count = au.bus_count(direction);
        let err = au
            .element_name(direction, count)
            .expect_err("one past the last bus must not resolve");
        assert!(
            matches!(
                err,
                AuError::OsStatus {
                    code: support::corpus::INVALID_ELEMENT,
                    ..
                }
            ),
            "{}: {} element {count} is one past the real count and must report \
             InvalidElement ({}), got {err:?}. A clamped index would return the \
             last valid bus's name for a bus that does not exist.",
            unit.label,
            if *is_output { "output" } else { "input" },
            support::corpus::INVALID_ELEMENT,
        );
    }

    eprintln!("plugin_named_buses_including_sidechains_are_reported: checked {checked} element(s)");
}

/// An **instrument that reports input channels** must still be hosted correctly.
///
/// If it regresses, TAL-NoiseMaker is silent or fails to initialize. This is the
/// exact shape the Apple corpus cannot produce: every Apple instrument reports 0
/// inputs, and this crate's history includes the opposite bug — the host once
/// installed an input render callback on units with no input element and every
/// instrument failed `initialize` with -10877. A host that hardcodes either
/// answer from the `aumu` type code is wrong for one of the two families.
///
/// The audible assertion is what makes this more than a property read: the unit
/// must be *quiet* before a note and *loud* after one, so a host that silently
/// dropped MIDI would fail. Note the idle floor is 1.12e-7 rather than zero
/// (measured, reproducible), so silence is a threshold and not an equality.
#[test]
fn an_instrument_that_reports_inputs_still_plays_midi() {
    let _g = lock();

    let unit = TAL_NOISEMAKER;
    let info = unit
        .find()
        .unwrap_or_else(|| absent(&unit, "the instrument-with-inputs check"));

    let mut au = open(&info, unit.label);

    // The topology finding: an instrument with a real input element.
    assert_eq!(
        au.num_inputs(),
        2,
        "{}: this unit is in the corpus because it is an INSTRUMENT reporting 2 \
         input channels, unlike every Apple instrument (which report 0). It now \
         reports {} — if the plugin changed, the corpus note about inferring \
         topology from the type code needs a new subject.",
        unit.label,
        au.num_inputs(),
    );

    let ch_in = au.num_inputs() as usize;
    let ch_out = au.num_outputs() as usize;
    let quiet = silence(ch_in, BLOCK as usize);
    let mut output = silence(ch_out, BLOCK as usize);

    au.reset().ok();
    let mut idle = 0.0f32;
    for _ in 0..4 {
        render(&mut au, &quiet, &mut output, BLOCK).expect("idle render");
        idle = idle.max(peak(&output));
    }
    assert!(
        all_finite(&output),
        "{}: idle output is non-finite before any note was sent",
        unit.label
    );
    assert!(
        idle < NOISEMAKER_IDLE_FLOOR,
        "{}: idle peak {idle} exceeds the measured noise floor \
         {NOISEMAKER_IDLE_FLOOR} — the instrument is sounding with no note on",
        unit.label,
    );

    // Velocity is MIDI 2.0-native u16 here; 0xC000 is ~96/127.
    au.send_midi(&[(
        Offset::ZERO,
        midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xC000),
    )]);
    let mut sounded = 0.0f32;
    for _ in 0..16 {
        render(&mut au, &quiet, &mut output, BLOCK).expect("render after note_on");
        assert!(
            all_finite(&output),
            "{}: non-finite output after note_on",
            unit.label
        );
        sounded = sounded.max(peak(&output));
    }

    // Calibrated to the measured 0.28133097 with generous headroom: the claim is
    // that the note SOUNDED, not that the synth's gain is a contract. A quarter
    // of the measured peak is far above the 1.12e-7 idle floor, so nothing but a
    // real note can satisfy it.
    assert!(
        sounded > NOISEMAKER_NOTE_PEAK / 4.0,
        "{}: peaked at {sounded} within 16 blocks of a note-on, measured \
         {NOISEMAKER_NOTE_PEAK} on macOS 15.6. The MIDI never reached the \
         plugin — an instrument that reports input channels is being hosted as \
         though it were an effect.",
        unit.label,
    );
}

// ------------------------------------------------------------ presets / bypass

/// The factory-preset table must be enumerated at its measured width, and every
/// preset must load.
///
/// If it regresses, a plugin's presets are missing or truncated from the host's
/// browser. TDR Nova's 73 presets are more than triple AUDistortion's 22 — the
/// widest Apple table — and its last selector is 72, well outside the range any
/// Apple unit uses, so this is the only row that would catch a host that assumed
/// a small dense preset index.
///
/// TAL-NoiseMaker's **zero** presets is the counterweight, and a distinct fact
/// from the read failing: `PRESETLESS_EFFECTS` covers Apple units whose
/// `FactoryPresets` read errors, which the host absorbs as "no presets". Here the
/// read succeeds and the table is genuinely empty.
#[test]
fn wide_preset_tables_enumerate_and_load() {
    let _g = lock();

    let found = each(
        THIRD_PARTY_PRESET_COUNTS,
        "its factory preset table",
        |unit, info, expected| {
            let mut au = open(info, unit.label);
            let presets = au.factory_presets();
            assert_eq!(
                presets.len(),
                expected,
                "{}: reported {} factory presets, measured {expected}. A short \
                 count is a truncated CFArray walk; the missing presets simply \
                 vanish from the host's browser.",
                unit.label,
                presets.len(),
            );

            for p in &presets {
                assert!(
                    !p.name.is_empty(),
                    "{}: preset {} has an empty name — a plugin-supplied \
                     CFString was dropped",
                    unit.label,
                    p.number,
                );
            }

            // Load the first and the LAST: the last is the one whose selector is
            // out of the range Apple's units occupy (72 for TDR Nova), and a host
            // that mishandled a wide index would still pass on the first.
            if let (Some(first), Some(last)) = (presets.first(), presets.last()) {
                for p in [first, last] {
                    au.load_factory_preset(p.number).unwrap_or_else(|e| {
                        panic!(
                            "{}: preset {} ({:?}) failed to load: {e:?}",
                            unit.label, p.number, p.name
                        )
                    });
                    let current = au
                        .current_preset()
                        .unwrap_or_else(|e| panic!("{}: current_preset: {e:?}", unit.label));
                    assert_eq!(
                        current.number, p.number,
                        "{}: loaded preset {} but the AU reports {} selected",
                        unit.label, p.number, current.number,
                    );
                }
                assert_renders_finite(&mut au, unit.label, "after loading a factory preset");
            }

            // A selector no preset uses must be refused, not silently accepted.
            // Measured -10879 on all three.
            let err = au
                .load_factory_preset(9_999)
                .expect_err("a nonexistent preset number must be refused");
            assert!(
                matches!(err, AuError::OsStatus { .. }),
                "{}: expected the AU's own refusal for preset 9999, got {err:?}",
                unit.label,
            );
        },
    );
    eprintln!("wide_preset_tables_enumerate_and_load: exercised {found} unit(s)");
}

/// Bypass must be settable, readable back, and audible in the output.
///
/// If it regresses, the host's bypass button lies: the user hears processing they
/// have switched off, or loses audio they have not.
///
/// The audible half is what makes this load-bearing, and it required a measured
/// correction that is worth recording: **TDR Nova's default preset is
/// transparent.** Rendering a sine through it bypassed and processed gives
/// bit-identical output (max difference 0.000000 on DC, 1 kHz and 2 kHz alike),
/// because every band starts at neutral gain. A "bypass changes the audio" test
/// therefore fails against a perfectly correct host until the plugin is given
/// something to do — so [`engage_processing`] moves a gain parameter off its
/// default first. Measured afterwards: Nova's processed-vs-bypassed difference
/// becomes 0.433844.
///
/// The 0.83 figure a first pass at this test used was measuring the wrong thing —
/// bypassed output versus the *input*, which differs for Nova only because its
/// 184-sample latency shifts the sine's phase. That is not evidence of
/// processing, and pinning it would have made the test pass for a reason
/// unrelated to bypass.
///
/// TAL Reverb 4 and TAL-NoiseMaker pass the input through **bit-exactly** when
/// bypassed (measured 0.0 difference against the input), so for them bypass is
/// true passthrough.
#[test]
fn bypass_round_trips_and_changes_the_audio() {
    let _g = lock();

    let found = each_unit(THIRD_PARTY, "its bypass path", |unit, info| {
        let mut au = open(info, unit.label);
        // Without this the processed leg is identical to the bypassed one on TDR
        // Nova, whose default preset is neutral — see this test's docs.
        engage_processing(&mut au, unit.label);

        assert!(
            !au.is_bypassed()
                .unwrap_or_else(|e| panic!("{}: is_bypassed failed: {e:?}", unit.label)),
            "{}: a freshly initialized AU must not start bypassed",
            unit.label
        );

        let ch_in = au.num_inputs().max(1) as usize;
        let ch_out = au.num_outputs() as usize;
        // DC rather than silence: bypass is invisible on a silent signal, since
        // both legs are then zero and the test could not fail.
        let input: Vec<Vec<f32>> = vec![vec![0.4f32; BLOCK as usize]; ch_in];

        let mut processed = silence(ch_out, BLOCK as usize);
        au.reset().ok();
        for _ in 0..8 {
            render(&mut au, &input, &mut processed, BLOCK).expect("processed render");
        }

        au.set_bypass(true)
            .unwrap_or_else(|e| panic!("{}: set_bypass(true) failed: {e:?}", unit.label));
        assert!(
            au.is_bypassed().unwrap(),
            "{}: set_bypass(true) did not stick — the host's bypass button would \
             show engaged while the plugin keeps processing",
            unit.label
        );

        let mut bypassed = silence(ch_out, BLOCK as usize);
        au.reset().ok();
        for _ in 0..8 {
            render(&mut au, &input, &mut bypassed, BLOCK).expect("bypassed render");
        }
        assert!(
            all_finite(&bypassed),
            "{}: bypassed output is non-finite",
            unit.label
        );

        // Bypassed output must differ from processed output, or bypass did nothing
        // audible and only the property round-tripped.
        let delta = processed[0]
            .iter()
            .zip(bypassed[0].iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            delta > 1e-4,
            "{}: bypassed and processed output are identical (max difference \
             {delta}), so engaging bypass changed nothing audible. Either the \
             property write is being dropped, or `engage_processing` no longer \
             finds a parameter that makes this plugin alter the signal — \
             measured 0.433844 for TDR Nova and 0.08+ for TAL Reverb 4 once a \
             gain is off its default.",
            unit.label,
        );

        au.set_bypass(false).expect("un-bypass");
        assert!(
            !au.is_bypassed().unwrap(),
            "{}: bypass did not clear",
            unit.label
        );
    });
    eprintln!("bypass_round_trips_and_changes_the_audio: exercised {found} unit(s)");
}

/// Push every "Gain" parameter to the top of its range, so the plugin actually
/// alters the signal.
///
/// Needed because a plugin at its default preset may be **transparent**: TDR
/// Nova's bands all start at neutral gain, so processed and bypassed output are
/// bit-identical and a bypass test cannot distinguish a working host from a broken
/// one. Measured: after this, Nova's processed-vs-bypassed difference is 0.433844
/// where it was exactly 0.
///
/// Gain is chosen by name rather than by id because the three units share no id
/// scheme (Nova's are 48..=1757, NoiseMaker's 0..=87, TAL Reverb 4's are hashes),
/// and "some parameter called Gain moves the output" is the property that
/// generalizes. Falls back to the first writable parameter when nothing matches,
/// and asserts that *something* was set — a silent no-op here would quietly
/// restore the transparent-plugin problem this exists to solve.
fn engage_processing(au: &mut Au, label: &str) {
    let params = au.get_parameter_list();
    let mut touched = 0usize;

    for p in params.iter().filter(|p| p.writable && !p.meter_read_only) {
        // "Active" alone does nothing on a band whose gain is still neutral, so
        // both are set: the band is switched on AND given a non-default gain.
        let is_target = p.name.contains("Gain") || p.name.contains("Active");
        if !is_target {
            continue;
        }
        if au.set_parameter(p.id, p.range.max).is_ok() {
            touched += 1;
        }
    }

    if touched == 0
        && let Some(p) = params
            .iter()
            .find(|p| p.writable && !p.meter_read_only && p.range.max > p.range.min)
        && au.set_parameter(p.id, p.range.max).is_ok()
    {
        touched += 1;
    }

    assert!(
        touched > 0,
        "{label}: could not move any parameter off its default, so the plugin may \
         still be transparent and the bypass comparison would be vacuous"
    );
}

/// A `.aupreset` written by this host must read back with the right identity and
/// reload into the unit that wrote it.
///
/// If it regresses, saved presets are unreadable — either rejected on load or,
/// worse, accepted by the wrong plugin. `src/aupreset.rs` documents why the host
/// checks identity itself rather than trusting the AU: an AU handed a dictionary
/// bearing its own identity keys but another plugin's `data` blob **accepts it and
/// adopts nonsense values**.
///
/// The cross-plugin leg is the one Apple's corpus cannot express well, because
/// these three units span two manufacturers *and* two component types — measured:
/// feeding TAL-NoiseMaker's file (`aumu`/`ncut`/`TOGU`) to TDR Nova
/// (`aufx`/`Td5a`/`Tdrl`) is refused with `PresetIdentityMismatch`.
#[test]
fn a_written_aupreset_round_trips_and_a_foreign_one_is_refused() {
    let _g = lock();
    // `save_preset_file`/`load_preset_file` take a `MainThread`, which this
    // test process claims for the worker thread it runs on: the preset calls
    // are plist I/O, not AppKit, so the OS main thread is not needed.

    let dir = std::env::temp_dir().join(format!(
        "au_third_party_presets_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&dir).expect("create temp preset dir");

    let found = each_unit(THIRD_PARTY, "its .aupreset round-trip", |unit, info| {
        let mut au = open(info, unit.label);
        let path = dir.join(format!(
            "{}.aupreset",
            String::from_utf8_lossy(unit.sub_type)
        ));

        au.save_preset_file(&main_thread(), &path, "ThirdPartyRoundTrip")
            .unwrap_or_else(|e| panic!("{}: save_preset_file failed: {e:?}", unit.label));

        let identity = tutti_au_host::read_preset_metadata(&path)
            .unwrap_or_else(|e| panic!("{}: read_preset_metadata failed: {e:?}", unit.label));
        assert_eq!(
            identity.sub_type,
            u32::from_be_bytes(*unit.sub_type),
            "{}: the written preset records subtype {:#x}, expected {:#x} — a \
             preset browser would attribute this file to the wrong plugin",
            unit.label,
            identity.sub_type,
            u32::from_be_bytes(*unit.sub_type),
        );
        assert_eq!(
            identity.manufacturer,
            u32::from_be_bytes(*unit.manufacturer),
            "{}: the written preset records the wrong manufacturer",
            unit.label,
        );
        assert_eq!(
            identity.name.as_deref(),
            Some("ThirdPartyRoundTrip"),
            "{}: the preset name did not survive the write/read",
            unit.label,
        );

        au.load_preset_file(&main_thread(), &path)
            .unwrap_or_else(|e| panic!("{}: could not reload its own preset: {e:?}", unit.label));
        assert_renders_finite(&mut au, unit.label, "after reloading its own .aupreset");
    });

    // The cross-plugin refusal, using two files written above.
    let nova_path = dir.join(format!(
        "{}.aupreset",
        String::from_utf8_lossy(TDR_NOVA.sub_type)
    ));
    if let (Some(nm), true) = (TAL_NOISEMAKER.find(), nova_path.exists()) {
        let mut nm_au = open(&nm, TAL_NOISEMAKER.label);
        let err = nm_au
            .load_preset_file(&main_thread(), &nova_path)
            .expect_err("TAL-NoiseMaker must refuse TDR Nova's .aupreset");
        assert!(
            matches!(err, AuError::PresetIdentityMismatch(_)),
            "expected PresetIdentityMismatch for a foreign preset, got {err:?}. \
             The host must catch this itself: an AU handed a dictionary with its \
             own identity keys but another plugin's data blob accepts it and \
             adopts nonsense.",
        );
        assert_renders_finite(
            &mut nm_au,
            TAL_NOISEMAKER.label,
            "after a refused foreign preset",
        );
    }

    std::fs::remove_dir_all(&dir).ok();
    eprintln!(
        "a_written_aupreset_round_trips_and_a_foreign_one_is_refused: exercised {found} unit(s)"
    );
}

// -------------------------------------------------- refused optional selectors

/// `AudioUnitProcess` must be reported as unimplemented, not as a silent success.
///
/// If it regresses — specifically, if the host ever absorbs `unimpErr` into
/// `Ok(())` — a host using the push-render path renders **silence** on every real
/// plugin while appearing to work.
///
/// This is the headline third-party asymmetry. `WITH_PUSH_RENDER` records 7 Apple
/// effects implementing the selector, which makes it look like the normal path for
/// an effect; all three third-party units answer `unimpErr` (-4). The status is
/// the component manager's "selector not implemented", so it is asserted exactly
/// rather than as "some error".
#[test]
fn push_render_is_refused_by_every_third_party_unit() {
    let _g = lock();
    use tutti_types::ChannelLayout;

    let found = each_unit(
        THIRD_PARTY_WITHOUT_PUSH_RENDER,
        "its AudioUnitProcess refusal",
        |unit, info| {
            let mut au = open(info, unit.label);
            let mut scratch = tutti_au_host::PushScratch::new(
                &[ChannelLayout::STEREO],
                &[ChannelLayout::STEREO],
                BLOCK,
            );
            let err = au
                .process_push(&mut scratch, BLOCK)
                .expect_err("this unit is in the corpus because it refuses AudioUnitProcess");
            assert!(
                matches!(
                    err,
                    AuError::RenderFailed {
                        code: UNIMP_ERR,
                        ..
                    }
                ),
                "{}: expected unimpErr ({UNIMP_ERR}) from AudioUnitProcess, got \
                 {err:?}. If this unit gained the selector the corpus note is \
                 stale; if the host started absorbing the refusal, a push-render \
                 host is now silently rendering nothing.",
                unit.label,
            );

            // The pull path must still work — the refusal is the selector, not
            // the unit, and a host must fall back rather than give up.
            assert_renders_finite(
                &mut au,
                unit.label,
                "via the pull path after a refused push",
            );
        },
    );
    eprintln!("push_render_is_refused_by_every_third_party_unit: exercised {found} unit(s)");
}

/// Optional capability properties must propagate their refusal rather than being
/// flattened into a plausible default.
///
/// If it regresses — if the host returned `false` for a refused
/// `InPlaceProcessing`, or `0` for a refused `RenderQuality` — the host would
/// report that these plugins *forbid* in-place operation and *request* the lowest
/// render quality, when in truth they have never mentioned either. Both are
/// decisions a host acts on.
///
/// All three third-party units refuse both properties with -10879, while 6 Apple
/// units answer the first and 4 the second (`WITH_IN_PLACE`,
/// `WITH_RENDER_QUALITY`). So neither property is something a host can count on
/// from a real plugin, and the refusal path is the common case rather than the
/// exotic one.
#[test]
fn refused_optional_properties_stay_refused() {
    let _g = lock();

    let found = each_unit(
        THIRD_PARTY_WITHOUT_OPTIONAL_PROPS,
        "its refusal of the optional capability properties",
        |unit, info| {
            let au = open(info, unit.label);

            for (what, result) in [
                (
                    "InPlaceProcessing",
                    au.supports_in_place_processing().map(|v| v as u32),
                ),
                ("RenderQuality", au.render_quality()),
            ] {
                let err = result.err().unwrap_or_else(|| {
                    panic!(
                        "{}: {what} was answered, but this unit is in the corpus \
                         because it refuses the property. If the plugin gained \
                         it, THIRD_PARTY_WITHOUT_OPTIONAL_PROPS is stale.",
                        unit.label
                    )
                });
                assert!(
                    matches!(
                        err,
                        AuError::OsStatus {
                            code: support::corpus::INVALID_PROPERTY,
                            ..
                        }
                    ),
                    "{}: expected kAudioUnitErr_InvalidProperty ({}) for {what}, \
                     got {err:?}. Flattening this into a default would report a \
                     capability claim the plugin never made.",
                    unit.label,
                    support::corpus::INVALID_PROPERTY,
                );
            }
        },
    );
    eprintln!("refused_optional_properties_stay_refused: exercised {found} unit(s)");
}
