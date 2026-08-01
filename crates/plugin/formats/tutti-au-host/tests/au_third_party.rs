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
//! * **`ElementName` is Copy-rule and leaks without a release.** Apple's units
//!   return *immortal* strings — retain counts saturated at `u64::MAX` or
//!   `0x0FFF_FFFF_FFFF_FFFF` — so an unreleased read looks free. TAL-NoiseMaker's
//!   is a real object at retain count 2, and ten unreleased reads walk it to 11.
//!   A host tested only against Apple concludes no release is needed, then leaks
//!   one CFString per read on every real plugin.
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
//! * **A JUCE Cocoa view.** TDR Nova is the only unit on this machine that
//!   exercises the crate's `relax-void-encoding` feature — JUCE declares the
//!   view-factory argument as `^{ComponentInstanceRecord=[1q]}` where Apple
//!   declares `^{OpaqueAudioComponentInstance=}`.
//!
//! ## How absence is handled, and why it is loud
//!
//! Third-party plugins are an **optional install**, so `AuRef::require`'s
//! panic-on-absence rule would fail this suite on any machine that simply does
//! not have them — a false alarm, not a caught regression. These go through
//! `ThirdPartyRef::find`, which returns an `Option`.
//!
//! That is exactly the shape `support/corpus.rs` warns about, though: a test that
//! returns early and reports `ok` is the failure mode this crate exists to refuse
//! — 31 of 32 VST3 conformance tests once reported success having run nothing.
//! So the mechanism here is **three-layered**, and the middle layer is the one
//! that makes it safe:
//!
//! 1. **A loud notice on every skip.** [`skip_notice`] prints `SKIPPED (not
//!    installed)` naming the unit and the codes, following
//!    `au_render_notify.rs`'s precedent.
//! 2. **One unconditional census test.** [`the_third_party_corpus_is_accounted_for`]
//!    always runs, always asserts, and *reports which units were found*. It is
//!    what makes a wholly-absent corpus visible instead of silent: the suite can
//!    skip every behavioural test, but it cannot skip this one, and its output
//!    names the machine's actual inventory.
//! 3. **Presence is asserted per unit, not per test body.** [`each`] resolves a
//!    table of units, counts how many it found, and **panics if a table that
//!    should have matched something matched nothing**. That is the guard against
//!    the failure a previous attempt at this corpus actually hit: the component
//!    codes were guessed, matched nothing, and every test skipped green. A
//!    partially-resolving table now fails loudly, while a completely absent one
//!    skips — because those are different facts.
//!
//! The rule every test here owes: **assert something unconditional, or be the
//! census test.** No body may consist only of an optional leg.
//!
//! ## Each test was proven load-bearing
//!
//! A test that cannot fail is worse than no test, so every assertion below was
//! verified by mutating `src/` and watching it fail. The mutation each one
//! catches, in order:
//!
//! | test | mutation it catches |
//! |---|---|
//! | `the_element_name_copy_rule_…` | `mem::forget` the owning `CfString` in `element_name` → count went 2→12 |
//! | `an_infinite_tail_survives_…` | clamp a non-finite tail to `0.0` at the property read |
//! | `wide_parameter_lists_are_read_whole` | drop the last entry of `parameters::list` (74 vs 75) |
//! | `reported_latency_matches_…` | hardcode 48 kHz in the latency conversion |
//! | `push_render_is_refused_…` | absorb the push-render `unimpErr` into `Ok` |
//! | `state_round_trips_…` | make `load_state` a silent no-op |
//! | `an_instrument_that_reports_inputs_…` | make `send_midi` drop every event |
//! | `bypass_round_trips_…` | make `set_bypass(true)` a no-op |
//! | `plugin_named_buses_…` | clamp an out-of-range element index to the last valid bus |
//! | `a_written_aupreset_…` | skip the `.aupreset` identity check |
//! | `every_unit_renders_finite_…` | remove the host's oversized-block guard |
//!
//! The `element_name` mutation is the one worth dwelling on: with the leak
//! present, the **entire Apple-based `au_channel_layout` suite still passed
//! 18/18** while this file failed. That is the blind spot this suite exists to
//! close, demonstrated rather than asserted.
//!
//! The oversized-block mutation is worth recording too, because it shows why the
//! assertion names `AuError::InvalidBuffer` specifically rather than "some
//! error": with the host guard removed the render still fails, but with the
//! *AU's* `kAudioUnitErr_TooManyFramesToProcess` (-10874) — which arrives only
//! after the host has already built a buffer list whose `mDataByteSize`
//! overstates storage it allocated for `BLOCK` frames. A test that accepted any
//! error would pass on the buggy host.
//!
//! ## Running
//!
//! ```bash
//! cargo test --manifest-path crates/bevy-tutti/Cargo.toml -p tutti-au-host \
//!   --test au_third_party
//! ```
//!
//! The editor test lives in `support/gui_lifecycle.rs` instead, because AppKit
//! requires the process main thread and cargo's harness has no way to provide it
//! — see `au_gui_lifecycle_main.rs`.

#![cfg(target_os = "macos")]

use std::sync::Mutex;

mod support;
use support::corpus::{
    all_finite, peak, render, silence, sine, ThirdPartyRef, INFINITE_TAIL_UNIT,
    MORTAL_ELEMENT_NAME, NOISEMAKER_IDLE_FLOOR, NOISEMAKER_NOTE_PEAK, TAL_NOISEMAKER, TDR_NOVA,
    THIRD_PARTY, THIRD_PARTY_LATENCY, THIRD_PARTY_NAMED_ELEMENTS, THIRD_PARTY_PARAM_COUNTS,
    THIRD_PARTY_PRESET_COUNTS, THIRD_PARTY_VERSION, THIRD_PARTY_WITHOUT_OPTIONAL_PROPS,
    THIRD_PARTY_WITHOUT_PUSH_RENDER, UNIMP_ERR,
};

use tutti_au_host::component::AuComponentInfo;
use tutti_au_host::instance::AuInstance;
use tutti_au_host::{AuError, BusDirection, MidiEvent};

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

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// Announce a skipped unit, loudly and with its codes.
///
/// The codes are printed rather than only the label because the failure this
/// guards against is a **wrong code**, not a missing plugin: a previous attempt at
/// this corpus guessed the subtypes, matched nothing, and every test skipped
/// green. Seeing `Td5a/Tdrl` in the output is what lets a reader check it against
/// `auval -a`.
fn skip_notice(unit: &ThirdPartyRef, what: &str) {
    eprintln!(
        "NOTICE: {} ({}/{}) is SKIPPED (not installed), so {what} is not \
         exercised on this machine. Verify the codes against `auval -a` before \
         assuming absence — a wrong subtype also produces this message.",
        unit.label,
        String::from_utf8_lossy(unit.sub_type),
        String::from_utf8_lossy(unit.manufacturer),
    );
}

/// Resolve every unit in `table`, calling `f` for the ones installed.
///
/// Returns how many were found, and **panics when a table resolved partially to
/// zero** — i.e. when some units in the corpus are installed but none of *these*
/// are. See the module docs: a fully-absent corpus is a legitimate skip, but a
/// table that matches nothing while its siblings match is a wrong code, and that
/// is the case that must fail rather than skip.
fn each<T: Copy>(
    table: &[(ThirdPartyRef, T)],
    what: &str,
    mut f: impl FnMut(&ThirdPartyRef, &AuComponentInfo, T),
) -> usize {
    let mut found = 0;
    for (unit, expected) in table {
        match unit.find() {
            Some(info) => {
                found += 1;
                f(unit, &info, *expected);
            }
            None => skip_notice(unit, what),
        }
    }
    assert_partial_resolution(found, table.len(), what);
    found
}

/// As [`each`], for a table with no expectation column.
fn each_unit(
    table: &[ThirdPartyRef],
    what: &str,
    mut f: impl FnMut(&ThirdPartyRef, &AuComponentInfo),
) -> usize {
    let mut found = 0;
    for unit in table {
        match unit.find() {
            Some(info) => {
                found += 1;
                f(unit, &info);
            }
            None => skip_notice(unit, what),
        }
    }
    assert_partial_resolution(found, table.len(), what);
    found
}

/// The guard described in the module docs, layer 3.
///
/// If *any* third-party unit is installed on this machine but this particular
/// table matched none of them, the table's codes are wrong — the exact failure a
/// previous attempt hit — and that must be a hard failure rather than a skip.
fn assert_partial_resolution(found: usize, table_len: usize, what: &str) {
    if found > 0 || table_len == 0 {
        return;
    }
    let installed = THIRD_PARTY.iter().filter(|u| u.find().is_some()).count();
    assert_eq!(
        installed,
        0,
        "{installed} of {} third-party units are installed, but the table for \
         {what} resolved NONE of its {table_len} entries. That is a wrong \
         component code, not a missing plugin — verify every subtype and \
         manufacturer against `auval -a`. A silent skip here is what let a \
         previous version of this corpus match nothing at all.",
        THIRD_PARTY.len(),
    );
}

/// Instantiate `info` initialized, as `corpus::open_info` does for the ramp test.
fn open(info: &AuComponentInfo, label: &str) -> AuInstance {
    // SAFETY: `component` came from `AudioComponentFindNext` via
    // `enumerate_components_of_type`, so it is a live factory handle for the
    // lifetime of this process.
    let mut au = unsafe { AuInstance::new(info.component, RATE, BLOCK) }
        .unwrap_or_else(|e| panic!("{label}: instantiate failed: {e:?}"));
    au.initialize()
        .unwrap_or_else(|e| panic!("{label}: initialize failed: {e:?}"));
    au
}

// ------------------------------------------------------------------- census

/// The unconditional test: what the corpus resolved to on this machine.
///
/// **This is the test that makes every skip above safe.** If it did not exist, a
/// machine without these plugins would run this file to completion, report `ok`
/// for every test, and prove nothing — the exact shape that let 31 of 32 VST3
/// conformance tests pass while running nothing. This one always executes and
/// always asserts, so the suite's inventory is always visible in its output.
///
/// It asserts two things that hold with or without the plugins: the corpus table
/// is non-empty and internally consistent (no duplicate codes, which would mean
/// two entries silently testing the same unit), and every entry that *is*
/// installed instantiates and reports the type its code claims. A `Td5a` that
/// enumerated under `Instrument` would be a corpus bug, not a plugin one.
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
            None => skip_notice(unit, "the whole behavioural suite for it"),
        }
    }

    eprintln!(
        "third-party corpus: {}/{} installed — found [{}]",
        found.len(),
        THIRD_PARTY.len(),
        found.join(", "),
    );
    if found.is_empty() {
        eprintln!(
            "NOTICE: NO third-party AU is installed. Every behavioural test in \
             au_third_party.rs skipped. The Apple-only corpus cannot see the \
             ElementName leak, the infinite tail, or the JUCE view path — see \
             this file's module docs. Install TDR Nova, TAL-NoiseMaker and TAL \
             Reverb 4 to cover them."
        );
    }
}

// -------------------------------------------------------------- the leak

/// A read of `kAudioUnitProperty_ElementName` must **release** the string the AU
/// hands over, and this test observes the retain count rather than the name.
///
/// If it regresses, every host UI that draws bus names leaks one CFString per
/// read — and a panel that redraws bus labels on mouse-move leaks unboundedly
/// until the process dies.
///
/// Only TAL-NoiseMaker can catch this. Apple's units return immortal strings
/// (measured: TDR Nova's "Input" and DLSMusicDevice's "stereo mix" both report
/// `u64::MAX`-class counts), so on Apple's corpus a leaking host and a correct one
/// are indistinguishable. This unit's `out[0]` "Output Master" is a real object at
/// retain count 2.
///
/// The assertion is a **delta across host reads, not an absolute**, for the reason
/// `support/gui_lifecycle.rs` gives for the view retain check: the absolute count
/// is not ours to predict — the plugin's own object graph holds references. What
/// is ours is that N host reads add nothing. Measured: ten unreleased reads walk
/// the count `2,3,4,…,11`; ten host reads hold it flat.
#[test]
fn the_element_name_copy_rule_is_observed_by_retain_count() {
    let _g = lock();
    let (unit, is_output, element, base) = MORTAL_ELEMENT_NAME;

    let Some(info) = unit.find() else {
        skip_notice(&unit, "the ElementName retain-count check");
        // The unconditional leg: the constant must still describe a *mortal*
        // string, or this test would be pointless even where the unit exists.
        // `base` saturating at the immortal sentinel would mean the corpus has
        // recorded an Apple-like string and the leak is no longer observable.
        assert!(
            base < 1_000,
            "MORTAL_ELEMENT_NAME's recorded retain count ({base}) is in the \
             immortal range, so it could not observe a leak even if the unit were \
             installed — the corpus entry is wrong"
        );
        return;
    };

    let au = open(&info, unit.label);
    let direction = if is_output {
        BusDirection::Output
    } else {
        BusDirection::Input
    };

    // Confirm the string is genuinely mortal on this machine before trusting the
    // measurement below. An immortal count cannot move, so the leak check would
    // pass against a leaking host — this is the assertion that keeps the test
    // load-bearing rather than merely green.
    // SAFETY: `raw_unit` is live for the lifetime of `au`.
    let first = unsafe { raw_name_retain_count(au.raw_unit(), is_output, element) }
        .expect("the corpus says this element publishes a name");
    assert!(
        first < 1_000,
        "{}: {} element {element} reports retain count {first}, which is the \
         immortal range — this element can no longer observe the ElementName \
         leak and MORTAL_ELEMENT_NAME needs a different subject. Measured {base} \
         on macOS 15.6.",
        unit.label,
        if is_output { "output" } else { "input" },
    );

    // Ten reads through the host. Each takes the AU's +1 and must give it back.
    const READS: usize = 10;
    let before = unsafe { raw_name_retain_count(au.raw_unit(), is_output, element) }.unwrap();
    for _ in 0..READS {
        let name = au
            .element_name(direction, element)
            .unwrap_or_else(|e| panic!("{}: element_name failed: {e:?}", unit.label));
        assert!(
            !name.is_empty(),
            "{}: element name came back empty, so the read did not actually \
             happen and the retain check below is vacuous",
            unit.label
        );
    }
    let after = unsafe { raw_name_retain_count(au.raw_unit(), is_output, element) }.unwrap();

    assert_eq!(
        after, before,
        "{}: {READS} host reads of the element name moved its retain count \
         {before} → {after}. `kAudioUnitProperty_ElementName` is Copy-rule: the \
         host owns the +1 and must release it. Leaking it means every bus-name \
         redraw leaks a CFString — measured 2→11 across 10 unreleased reads on \
         this exact element.",
        unit.label,
    );
}

/// The AU's own `ElementName` retain count, read without disturbing it.
///
/// Reads the property directly rather than through `AuInstance::element_name`,
/// because the host's version releases — and the count *after* that release is
/// exactly what this needs to sample. The one reference this read itself acquires
/// is given back before returning, so the observation is non-perturbing.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
unsafe fn raw_name_retain_count(
    unit: coreaudio_sys::AudioUnit,
    is_output: bool,
    element: u32,
) -> Option<usize> {
    let mut raw: *const std::os::raw::c_void = std::ptr::null();
    let mut size = std::mem::size_of::<*const std::os::raw::c_void>() as u32;
    // 30 = kAudioUnitProperty_ElementName; scope 2 = output, 1 = input.
    let status = unsafe {
        coreaudio_sys::AudioUnitGetProperty(
            unit,
            30,
            if is_output { 2 } else { 1 },
            element,
            &mut raw as *mut *const std::os::raw::c_void as *mut std::os::raw::c_void,
            &mut size,
        )
    };
    if status != 0 || raw.is_null() {
        return None;
    }
    let count = unsafe { objc_nullary(raw as *mut _, c"retainCount") };
    // Give back the +1 this read took, so sampling does not itself leak.
    unsafe { objc_nullary(raw as *mut _, c"release") };
    Some(count)
}

/// Send a nullary Objective-C selector and return the raw word it produced.
///
/// Declared here rather than reached through `objc2`'s `msg_send!` for the reason
/// `support/gui_lifecycle.rs` gives: these are bare nullary sends with no argument
/// marshalling, and the encoding verification `msg_send!` layers on is exactly
/// what a debugging read does not want.
///
/// # Safety
/// `obj` must be null or a live Objective-C object that responds to `sel`.
unsafe fn objc_nullary(obj: *mut std::os::raw::c_void, sel: &std::ffi::CStr) -> usize {
    if obj.is_null() {
        return 0;
    }
    unsafe extern "C" {
        fn objc_msgSend(
            receiver: *mut std::os::raw::c_void,
            sel: *const std::os::raw::c_void,
        ) -> usize;
        fn sel_registerName(name: *const std::os::raw::c_char) -> *const std::os::raw::c_void;
    }
    unsafe { objc_msgSend(obj, sel_registerName(sel.as_ptr())) }
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

    // The unconditional leg: the conversion's behaviour is a property of the unit
    // vocabulary, not of any plugin, so it is asserted whether or not TAL Reverb 4
    // is installed. This is the half a host author has to know about.
    let inf = tutti_types::Seconds(f32::INFINITY);
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
    let Some(info) = unit.find() else {
        skip_notice(&unit, "the infinite-tail read");
        return;
    };

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

/// `save_state`/`load_state` must restore the parameter values that were saved.
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
            .save_state()
            .unwrap_or_else(|e| panic!("{}: save_state failed: {e:?}", unit.label));
        assert!(
            !blob.is_empty(),
            "{}: save_state returned an empty blob, so nothing was saved",
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
             would pass even if load_state did nothing at all",
            unit.label
        );

        au.load_state(&blob)
            .unwrap_or_else(|e| panic!("{}: load_state failed: {e:?}", unit.label));

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
        assert_renders_finite(&mut au, unit.label, "after load_state");
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

    let (Some(nova), Some(nm)) = (TDR_NOVA.find(), TAL_NOISEMAKER.find()) else {
        // Needs both, so report whichever is missing.
        for unit in [TDR_NOVA, TAL_NOISEMAKER] {
            if unit.find().is_none() {
                skip_notice(&unit, "the cross-plugin state rejection check");
            }
        }
        return;
    };

    let nm_blob = open(&nm, TAL_NOISEMAKER.label)
        .save_state()
        .expect("NoiseMaker save_state");
    let mut nova_au = open(&nova, TDR_NOVA.label);

    let err = nova_au
        .load_state(&nm_blob)
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
            au.set_sample_rate(44_100.0)
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
        let input = sine(2, oversized as usize, 0, 0.5, RATE as f32);
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
fn assert_renders_finite(au: &mut AuInstance, label: &str, when: &str) {
    let ch_in = au.num_inputs().max(1) as usize;
    let ch_out = au.num_outputs() as usize;
    let input = sine(ch_in, BLOCK as usize, 0, 0.5, RATE as f32);
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
    let mut skipped = std::collections::BTreeSet::new();
    for (unit, is_output, element, expected) in THIRD_PARTY_NAMED_ELEMENTS {
        let Some(info) = unit.find() else {
            skipped.insert(unit.label);
            continue;
        };
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

    for label in &skipped {
        eprintln!("NOTICE: {label} is SKIPPED (not installed) for the named-bus check.");
    }
    assert_partial_resolution(checked, THIRD_PARTY_NAMED_ELEMENTS.len(), "named buses");
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
    let Some(info) = unit.find() else {
        skip_notice(&unit, "the instrument-with-inputs check");
        return;
    };

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
    au.send_midi(&[MidiEvent::note_on(0, 0, 60, 0xC000)]);
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
fn engage_processing(au: &mut AuInstance, label: &str) {
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

    if touched == 0 {
        if let Some(p) = params
            .iter()
            .find(|p| p.writable && !p.meter_read_only && p.range.max > p.range.min)
        {
            if au.set_parameter(p.id, p.range.max).is_ok() {
                touched += 1;
            }
        }
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
    // `save_preset_file`/`load_preset_file` call `assert_main_thread`, which is a
    // no-op until some target marks a main thread. No harness target does, so
    // these are safe to call here — the same reason `au_aupreset.rs` can.

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

        au.save_preset_file(&path, "ThirdPartyRoundTrip")
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

        au.load_preset_file(&path)
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
            .load_preset_file(&nova_path)
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

// ------------------------------------------------------------- JUCE editor

/// TDR Nova's **JUCE** Cocoa view must open at the geometry the view reports, and
/// close without leaking the retain.
///
/// If it regresses, the crate's `relax-void-encoding` feature has stopped doing
/// its job and **every JUCE-based plugin's editor fails to open** — which is most
/// commercial AU plugins. JUCE declares the view factory's AudioUnit argument as
/// `^{ComponentInstanceRecord=[1q]}` while Apple declares
/// `^{OpaqueAudioComponentInstance=}`; objc2's debug-build encoding check compares
/// what the host sends against the *plugin's* declared signature, so without the
/// relaxation one of the two families always fails. **No Apple unit exercises
/// this**, which is why the feature existed in `Cargo.toml` with nothing testing
/// it against a real JUCE plugin.
///
/// The retain assertion **observes the count** rather than pointer nullness,
/// following `support/gui_lifecycle.rs`: a deliberate double-release in
/// `AuEditor::close` once passed 7/7 GUI tests because every assertion there was
/// about nullness. The delta is asserted, not the absolute, because AppKit and the
/// plugin's own object graph hold references of their own.
///
/// ## Why this is `#[ignore]` rather than in the main-thread runner
///
/// AppKit requires the process main thread, and cargo's harness gives every test
/// a worker. The `harness = false` runner `au_gui_lifecycle_main.rs` is the only
/// target that owns `main()`, but its test list is registered inside that file —
/// which this change does not own. So this test carries the same
/// `pthread_main_np` guard `au_gui_lifecycle.rs` uses: it self-skips on a worker
/// thread rather than aborting the process, and runs for real only on a target
/// that is the main thread. Measured there: 830x598, bit-stable across 3 runs.
#[test]
#[ignore = "AppKit requires the main thread; see this test's docs"]
fn a_juce_cocoa_view_opens_at_its_own_geometry() {
    let _g = lock();

    // The unconditional leg: the corpus must still describe TDR Nova as the JUCE
    // subject with a plausible geometry, whether or not it is installed. A zero
    // here would mean the recorded measurement was lost.
    let (juce_unit, want_w, want_h) = support::corpus::THIRD_PARTY_COCOA_VIEW
        .iter()
        .find(|(u, _, _)| u.sub_type == TDR_NOVA.sub_type)
        .copied()
        .expect("TDR Nova must be in THIRD_PARTY_COCOA_VIEW — it is the only JUCE subject");
    assert!(
        want_w > 0 && want_h > 0,
        "the recorded JUCE view geometry is {want_w}x{want_h}, which cannot be a \
         real view frame"
    );

    if !is_main_thread() {
        eprintln!(
            "a_juce_cocoa_view_opens_at_its_own_geometry: skipped — AppKit \
             requires the process main thread and this is a cargo worker. The \
             JUCE `relax-void-encoding` path is NOT exercised in this run."
        );
        return;
    }

    let Some(info) = juce_unit.find() else {
        skip_notice(&juce_unit, "the JUCE Cocoa view path");
        return;
    };

    let au = open(&info, juce_unit.label);
    assert!(
        tutti_au_host::AuEditor::has_editor(au.raw_unit()),
        "{}: advertises no Cocoa view, so the JUCE encoding path is untested",
        juce_unit.label
    );

    // SAFETY: on the main thread (checked above), and `au` is live and
    // initialized. `open(unit, None)` instantiates the view without parenting it.
    let mut editor =
        unsafe { tutti_au_host::AuEditor::open(au.raw_unit(), None) }.unwrap_or_else(|e| {
            panic!(
                "{}: opening a JUCE Cocoa view failed: {e:?}. This is what an \
                 objc2 encoding-check rejection looks like — verify the \
                 `relax-void-encoding` feature is still enabled.",
                juce_unit.label
            )
        });

    let size = editor.editor_size();
    assert_eq!(
        (size.width, size.height),
        (want_w, want_h),
        "{}: the JUCE view reports {}x{}, measured {want_w}x{want_h} on macOS \
         15.6. A host inventing geometry (returning its own requested size) \
         would show a mismatch here.",
        juce_unit.label,
        size.width,
        size.height,
    );

    // Observe the retain count across close, not merely the pointer.
    let view = editor.view_ptr();
    assert!(
        !view.is_null(),
        "{}: view_ptr is null while open",
        juce_unit.label
    );
    // SAFETY: `view` is the live view just returned; balanced by the release below.
    unsafe { objc_nullary(view, c"retain") };
    let before = unsafe { objc_nullary(view, c"retainCount") };
    editor.close();
    let after = unsafe { objc_nullary(view, c"retainCount") };
    assert_eq!(
        after + 1,
        before,
        "{}: the JUCE view's retain count went {before} → {after} across close, \
         but close must give up exactly the one reference open took. {}",
        juce_unit.label,
        if after + 1 < before {
            "It released too many times (over-release: UB on a view AppKit may \
             still hold)."
        } else {
            "It released too few — every plugin window leaks its whole JUCE GUI \
             object graph."
        },
    );
    // SAFETY: gives up the retain taken above; `view` is not read after.
    unsafe { objc_nullary(view, c"release") };

    assert!(
        editor.view_ptr().is_null(),
        "{}: view_ptr still points at the released NSView after close",
        juce_unit.label
    );
}

/// Whether the caller owns the process main thread.
///
/// `pthread_main_np` is the only way to ask, for the reason
/// `au_gui_lifecycle.rs` documents: `assert_main_thread` compares against a
/// thread *marked* by `mark_main_thread`, which no harness target calls.
fn is_main_thread() -> bool {
    // SAFETY: `pthread_main_np` takes no arguments and reads no memory.
    unsafe {
        unsafe extern "C" {
            fn pthread_main_np() -> std::os::raw::c_int;
        }
        pthread_main_np() == 1
    }
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
