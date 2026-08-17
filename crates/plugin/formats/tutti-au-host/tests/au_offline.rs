//! Bounce-time properties and the push-render path, against the real corpus.
//!
//! Four AUv2 facilities that only an *exporting* host exercises, and which
//! nothing on the base branch touched: three had zero call sites in the crate.
//! What this suite proves:
//!
//! - **Offline render** round-trips on the units that implement it, is refused as
//!   an error (never flattened to `false`) on the units that do not, and does not
//!   break rendering when set.
//! - **In-place processing** is *read* faithfully, including the distinction
//!   between an AU that says `0` and one that never answered — a distinction that
//!   matters because **no unit on this system says `0`**, so a host that flattened
//!   the refusal would publish a census that is pure fiction.
//! - **Render quality** round-trips over its whole documented range where
//!   supported, is an error where not, and — the part that took measuring — is
//!   range-checked *host-side* because three of the four units that implement it
//!   accept 999 with `noErr` and read 999 straight back.
//! - **`AudioUnitProcess`** renders correct audio, is bit-identical to
//!   `AudioUnitRender` on the same input, refuses an oversized block before the AU
//!   ever sees it, and does not allocate in steady state.
//! - **`AudioUnitProcessMultiple`** — measured, and the measurement is the
//!   finding. See `process_multiple_is_unimplemented_across_the_corpus`.
//!
//! ## The headline: there is no working AU sidechain on this machine
//!
//! `AudioUnitProcessMultiple` is the only AUv2 call that can deliver a second
//! input bus in one render, which is what a sidechain is. Of every Apple unit in
//! the corpus plus both third-party units installed, exactly one — AUReverb2 —
//! implements the selector, and it accepts exactly **one** input list, refusing a
//! second with `kAudioUnitErr_InvalidElement`. AUMultiChannelMixer, which has 8
//! real input elements, answers `unimpErr` for 1, 2 and 8 lists alike, so the
//! absence is the selector and not the topology.
//!
//! The tests below assert that state of affairs rather than skipping it. If a
//! future macOS or a newly installed plugin implements the selector, the
//! corresponding test **fails** and says so — which is the only way a measured
//! absence stays honest.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_offline
//! cargo test -p tutti-au-host --test au_offline -- --include-ignored
//! ```
//!
//! The no-alloc tests are `#[ignore]`d to match `au_process_no_alloc.rs`. A
//! missing corpus unit **fails** rather than skipping — see `support/corpus.rs`.

#![cfg(target_os = "macos")]

use std::sync::Mutex;

mod support;
use support::corpus::{
    all_finite, peak, render, silence, sine, DELAY, ENFORCES_RENDER_QUALITY_RANGE,
    IMPLEMENTS_PROCESS_MULTIPLE, PARAM_ERR, TOO_MANY_FRAMES, UNIMP_ERR, WITHOUT_IN_PLACE,
    WITHOUT_OFFLINE_RENDER, WITHOUT_PUSH_RENDER, WITHOUT_RENDER_QUALITY, WITH_IN_PLACE,
    WITH_OFFLINE_RENDER, WITH_PUSH_RENDER, WITH_RENDER_QUALITY,
};

use tutti_au_host::offline::{self, PushScratch, RENDER_QUALITY_MAX};
use tutti_au_host::types::K_AUDIO_UNIT_ERR_INVALID_PROPERTY;
use tutti_au_host::AuError;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_plugin_types::ChannelLayout;

// Serializes AU instantiation, as `au_conformance.rs`'s `AU_LOCK` does and for
// the same reason: component discovery walks a process-global registry, and
// several tests here open the same unit.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// Poison is recovered rather than propagated: one panicking test would
/// otherwise turn every later test into a spurious `PoisonError` and hide which
/// one actually broke. The guard only serializes — there is no shared state to be
/// left inconsistent.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// A property refusal, spelled as the exact status rather than a bare `is_err`.
///
/// The distinction matters throughout this file: `is_err()` is satisfied by *any*
/// failure, so an assertion built on it passes just as readily when the unit
/// failed to instantiate, when the wrong scope was addressed, or when the host
/// grew a bug that turns every property read into an error. Only
/// `kAudioUnitErr_InvalidProperty` means "this AU does not have that property",
/// which is the fact each `WITHOUT_*` list records.
fn assert_property_absent<T: std::fmt::Debug>(
    result: tutti_au_host::Result<T>,
    label: &str,
    what: &str,
) {
    match result {
        Err(AuError::OsStatus {
            code: K_AUDIO_UNIT_ERR_INVALID_PROPERTY,
            ..
        }) => {}
        other => panic!(
            "{label} is recorded in corpus.rs as not implementing {what}, so the \
             host must report kAudioUnitErr_InvalidProperty \
             ({K_AUDIO_UNIT_ERR_INVALID_PROPERTY}) — got {other:?}"
        ),
    }
}

// ------------------------------------------------------------- offline render

/// The offline flag must round-trip on every unit that implements it, in both
/// directions and in both typestates.
///
/// Both directions, because a setter that ignored its argument and always wrote
/// `1` would satisfy a one-way check. Both typestates, because a bouncing host
/// sets the flag *before* `initialize` (an AU sizing an oversampling buffer from
/// it can only act at initialize time) and clears it after the bounce, while the
/// unit is live.
#[test]
fn the_offline_flag_round_trips_where_implemented() {
    let _g = lock();
    for unit in WITH_OFFLINE_RENDER {
        let mut au = unit.open_uninitialized(RATE, BLOCK);

        // Fresh instances default to real-time, which Apple's header states
        // outright ("The value defaults to false"). Asserted rather than assumed:
        // a host that wrote the flag at construction would break every live
        // session, and nothing else would notice.
        assert!(
            !au.is_offline_render()
                .unwrap_or_else(|e| panic!("{}: read offline flag: {e:?}", unit.label)),
            "{}: a fresh instance must default to real-time rendering",
            unit.label
        );

        for state in ["pre-init", "post-init"] {
            for wanted in [true, false, true] {
                au.set_offline_render(wanted).unwrap_or_else(|e| {
                    panic!("{} ({state}): set offline={wanted}: {e:?}", unit.label)
                });
                assert_eq!(
                    au.is_offline_render().unwrap(),
                    wanted,
                    "{} ({state}): offline flag did not round-trip",
                    unit.label
                );
            }
            if state == "pre-init" {
                au.initialize()
                    .unwrap_or_else(|e| panic!("{}: initialize: {e:?}", unit.label));
            }
        }
    }
}

/// A unit with no offline-render property must report the AU's own refusal, not a
/// fabricated `false`.
///
/// This is the whole justification for the API returning `Result<bool>` rather
/// than `bool`, and the shape is unusual enough to be worth pinning: the property
/// whose entire purpose is "this render is a bounce" is not implemented by a
/// single Apple *effect*. A host that flattened the refusal would be unable to
/// tell the user that an export cannot be made to match the audition, because
/// every effect would report "real-time mode" — a state the host would then
/// believe it could change.
#[test]
fn a_unit_without_the_offline_property_reports_the_refusal() {
    let _g = lock();
    for unit in WITHOUT_OFFLINE_RENDER {
        let mut au = unit.open(RATE, BLOCK);
        assert_property_absent(au.is_offline_render(), unit.label, "OfflineRender");
        assert_property_absent(au.set_offline_render(true), unit.label, "OfflineRender");
    }
}

/// An AU told it is rendering offline must still render.
///
/// The flag is advisory and changes only what the AU is *permitted* to do, so a
/// unit that stopped producing audio under it would be a hard bug — and one a
/// host would find only at export time, having already spent the render.
///
/// The output is asserted **finite as well as non-trivial**. `peak()` folds with
/// `f32::max`, which returns the non-NaN operand, so an all-NaN buffer has a peak
/// of exactly 0.0: a "peak is small" assertion alone cannot tell silence from
/// garbage, and a "peak is large" one cannot tell audio from infinity.
///
/// Measured on macOS 15.6: neither instrument audibly changes under the flag.
/// AUSampler peaks at 0.2466 and DLSMusicDevice at 0.0953 over 16 blocks of a
/// held middle C, identical with the flag set and clear. The assertion is
/// therefore "still renders, still finite, still non-silent", not "sounds
/// different" — claiming the latter would be asserting a behaviour these units do
/// not have.
#[test]
fn an_offline_flagged_unit_still_renders() {
    let _g = lock();
    for unit in WITH_OFFLINE_RENDER {
        let mut peaks = Vec::new();
        for offline in [false, true] {
            let mut au = unit.open_uninitialized(RATE, BLOCK);
            au.set_offline_render(offline).unwrap();
            au.initialize().unwrap();
            assert_eq!(
                au.is_offline_render().unwrap(),
                offline,
                "{}: the flag must survive initialize",
                unit.label
            );

            // Instruments have no input bus, so they are driven by MIDI.
            au.send_midi(&[tutti_au_host::MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                0xC000,
            )]);
            let input = silence(2, BLOCK as usize);
            let mut output = silence(2, BLOCK as usize);
            let mut max = 0.0f32;
            for _ in 0..16 {
                render(&mut au, &input, &mut output, BLOCK).unwrap_or_else(|e| {
                    panic!("{}: render with offline={offline}: {e:?}", unit.label)
                });
                assert!(
                    all_finite(&output),
                    "{}: offline={offline} produced a non-finite sample; peak() folds \
                     with f32::max and would report 0.0 for an all-NaN block, so \
                     finiteness has to be checked separately",
                    unit.label
                );
                max = max.max(peak(&output));
            }
            peaks.push(max);
        }
        for (offline, p) in [false, true].iter().zip(&peaks) {
            assert!(
                *p > 0.001,
                "{}: offline={offline} rendered silence (peak {p}); the flag is \
                 advisory and must not stop the AU producing audio",
                unit.label
            );
        }
    }
}

// -------------------------------------------------------- in-place processing

/// Every unit that advertises in-place processing must report the value
/// `corpus.rs` measured.
///
/// The *value* is asserted, not merely that the read succeeded, because the
/// property is a one-bit capability claim and the bit is its entire content. It
/// also documents the census a host would build: six Apple effects permit
/// aliasing, so a host that implemented the optimisation would save one buffer
/// copy per block on each of them.
#[test]
fn in_place_capability_is_read_faithfully() {
    let _g = lock();
    let mut advertised = Vec::new();
    for (unit, expected) in WITH_IN_PLACE {
        let au = unit.open(RATE, BLOCK);
        let got = au
            .supports_in_place_processing()
            .unwrap_or_else(|e| panic!("{}: read InPlaceProcessing: {e:?}", unit.label));
        assert_eq!(
            got, *expected,
            "{}: corpus.rs records InPlaceProcessing = {expected}",
            unit.label
        );
        advertised.push((unit.label, got));
    }
    // Reported, not merely asserted: the point of the sweep is the census, and a
    // reader of the test output should be able to see which units it covered.
    eprintln!("units advertising in-place processing: {advertised:?}");
    assert_eq!(
        advertised.len(),
        WITH_IN_PLACE.len(),
        "every unit in WITH_IN_PLACE must have been probed"
    );
}

/// No unit on this system reports in-place processing as *unsupported* — they
/// either say `true` or refuse the property.
///
/// This is the fact that makes flattening the refusal into `false` a lie rather
/// than a harmless default, and it is asserted directly so the reasoning in
/// `offline::supports_in_place`'s docs is pinned rather than merely written down.
/// If some future unit genuinely reports `0`, this test fails and the docs need
/// rewriting — which is the correct outcome, since the host's error handling was
/// justified by this being empty.
#[test]
fn nothing_reports_in_place_as_forbidden() {
    let _g = lock();
    let mut says_no = Vec::new();
    for (unit, _) in WITH_IN_PLACE {
        let au = unit.open(RATE, BLOCK);
        if !au.supports_in_place_processing().unwrap() {
            says_no.push(unit.label);
        }
    }
    for unit in WITHOUT_IN_PLACE {
        let au = unit.open(RATE, BLOCK);
        // A refusal, specifically — not merely "not true". The two are the
        // distinction this whole test exists to draw.
        assert_property_absent(
            au.supports_in_place_processing(),
            unit.label,
            "InPlaceProcessing",
        );
    }
    assert!(
        says_no.is_empty(),
        "no unit was measured to report InPlaceProcessing = 0; if {says_no:?} now \
         does, `offline::supports_in_place`'s justification for propagating the \
         refusal rather than flattening it to `false` needs revisiting"
    );
}

// ------------------------------------------------------------ render quality

/// Render quality must round-trip across its whole documented range on every
/// unit that implements it, and the measured default must hold.
///
/// The range is walked rather than sampled at one value: a setter that wrote a
/// constant, or a getter that returned the last value the *host* wrote rather
/// than the AU's, would satisfy a single round-trip. The endpoints and the
/// documented maximum are included explicitly because those are where an
/// off-by-one lives.
#[test]
fn render_quality_round_trips_where_supported() {
    let _g = lock();
    for (unit, default) in WITH_RENDER_QUALITY {
        let mut au = unit.open(RATE, BLOCK);
        assert_eq!(
            au.render_quality()
                .unwrap_or_else(|e| panic!("{}: read RenderQuality: {e:?}", unit.label)),
            *default,
            "{}: corpus.rs records a default of {default}",
            unit.label
        );

        for q in [0, 1, 63, 64, 126, RENDER_QUALITY_MAX] {
            au.set_render_quality(q)
                .unwrap_or_else(|e| panic!("{}: set quality {q}: {e:?}", unit.label));
            assert_eq!(
                au.render_quality().unwrap(),
                q,
                "{}: quality {q} did not round-trip",
                unit.label
            );
        }
    }
}

/// A unit with no render-quality property must report the refusal, in both
/// directions.
#[test]
fn render_quality_is_refused_where_unsupported() {
    let _g = lock();
    for unit in WITHOUT_RENDER_QUALITY {
        let mut au = unit.open(RATE, BLOCK);
        assert_property_absent(au.render_quality(), unit.label, "RenderQuality");
        assert_property_absent(au.set_render_quality(64), unit.label, "RenderQuality");
    }
}

/// The host must refuse an out-of-range render quality itself, because the AUs
/// mostly do not.
///
/// This is the test that justifies the host-side bound, and it needs both halves
/// to do so:
///
/// * **AUDistortion**, the one unit that enforces the range, is driven through
///   the *raw* property write to show what enforcement looks like — `paramErr`,
///   and the previous value kept. If the host merely forwarded the write, this is
///   the behaviour a caller would get.
/// * **AUMatrixReverb** is driven the same way to show what it looks like when
///   the AU does *not* enforce: `noErr`, and `999` read straight back. A host that
///   trusted the status would display "quality: 999" out of a 0–127 control
///   indefinitely.
///
/// The host's own refusal is then asserted to be an `InvalidBuffer`, arriving
/// *before* the AU is asked, so the caller learns its number was nonsense instead
/// of quietly keeping it.
#[test]
fn an_out_of_range_render_quality_is_refused_by_the_host() {
    let _g = lock();

    // The host's guard: refuses without consulting the AU, on a unit that would
    // have enforced anyway and on one that would not.
    for (unit, _) in WITH_RENDER_QUALITY {
        let mut au = unit.open(RATE, BLOCK);
        let before = au.render_quality().unwrap();
        for bad in [RENDER_QUALITY_MAX + 1, 200, 999, u32::MAX] {
            match au.set_render_quality(bad) {
                Err(AuError::InvalidBuffer(msg)) => assert!(
                    msg.contains(&bad.to_string()),
                    "{}: the rejection should name the offending value; got {msg:?}",
                    unit.label
                ),
                other => panic!(
                    "{}: quality {bad} is above the documented maximum \
                     {RENDER_QUALITY_MAX} and must be refused host-side, because \
                     three of the four units that implement the property accept it \
                     with noErr — got {other:?}",
                    unit.label
                ),
            }
        }
        assert_eq!(
            au.render_quality().unwrap(),
            before,
            "{}: a refused write must not have reached the AU",
            unit.label
        );
    }

    // What the AUs themselves do, to show the guard is not redundant. Driven
    // through the raw property write rather than the host method, precisely
    // because the host method is what is being justified.
    let enforcing = ENFORCES_RENDER_QUALITY_RANGE.open(RATE, BLOCK);
    let st = unsafe { raw_set_render_quality(enforcing.raw_unit(), 999) };
    assert_eq!(
        st, PARAM_ERR,
        "{} is recorded as the one unit that enforces the range; it should answer \
         paramErr ({PARAM_ERR}) for 999",
        ENFORCES_RENDER_QUALITY_RANGE.label
    );

    // AUMatrixReverb: the non-enforcing case, which is the majority.
    let lax = support::corpus::MATRIX_REVERB.open(RATE, BLOCK);
    let st = unsafe { raw_set_render_quality(lax.raw_unit(), 999) };
    let read_back = lax.render_quality().unwrap();
    assert_eq!(
        (st, read_back),
        (0, 999),
        "AUMatrixReverb is recorded as accepting an out-of-range quality with noErr \
         and reading it straight back — that is what makes the host-side check \
         load-bearing rather than belt-and-braces, and why a write-then-verify \
         scheme would not substitute for it"
    );
}

/// Write `kAudioUnitProperty_RenderQuality` bypassing the host's range check, to
/// observe what the AU itself does with an out-of-range value.
///
/// # Safety
/// `unit` must be a live `AudioUnit`. The property's value type is `UInt32`, which
/// is what is written.
unsafe fn raw_set_render_quality(unit: coreaudio_sys::AudioUnit, quality: u32) -> i32 {
    coreaudio_sys::AudioUnitSetProperty(
        unit,
        coreaudio_sys::kAudioUnitProperty_RenderQuality,
        coreaudio_sys::kAudioUnitScope_Global,
        0,
        &quality as *const u32 as *const std::os::raw::c_void,
        std::mem::size_of::<u32>() as u32,
    )
}

// ----------------------------------------------------------- push render path

/// Build a stereo-in / stereo-out single-bus scratch at `BLOCK`.
fn stereo_scratch() -> PushScratch {
    PushScratch::new(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO], BLOCK)
}

/// The push path must produce real audio on every unit that implements the
/// selector.
///
/// Driven with a sine rather than silence, deliberately. An in-place render can be
/// wrong in ways silence cannot reveal — an AU reading its own output as input, a
/// stale block emitted from the previous call, a `bind_list` that pointed at the
/// wrong channel — and every one of those still yields zeroes when fed zeroes.
///
/// Both a lower bound and finiteness are asserted, for the reason
/// `an_offline_flagged_unit_still_renders` spells out: `peak()` folds with
/// `f32::max`, so an all-NaN block reports 0.0 and would pass a "no clipping"
/// check on its own.
#[test]
fn the_push_path_renders_audio() {
    let _g = lock();
    for unit in WITH_PUSH_RENDER {
        let mut au = unit.open(RATE, BLOCK);
        let mut scratch = stereo_scratch();
        let mut out = silence(2, BLOCK as usize);
        let mut max = 0.0f32;

        for blk in 0..8usize {
            let input = sine(2, BLOCK as usize, blk * BLOCK as usize, 0.5, RATE as f32);
            let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
            assert!(
                scratch.stage_input(0, &ins, BLOCK),
                "input bus 0 must exist on a scratch built with one input bus"
            );
            au.process_push(&mut scratch, BLOCK)
                .unwrap_or_else(|e| panic!("{}: process_push block {blk}: {e:?}", unit.label));

            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|v| v.as_mut_slice()).collect();
            assert!(scratch.emit_output(0, &mut outs, BLOCK));
            assert!(
                all_finite(&out),
                "{}: block {blk} produced a non-finite sample",
                unit.label
            );
            max = max.max(peak(&out));
        }

        // 0.5 in; every unit here either passes level through or attenuates, so a
        // floor well below the input catches "rendered nothing" without pinning
        // any unit's gain. Measured peaks ranged 0.25 (AUMultibandCompressor) to
        // 0.63 (AUDistortion) at this input.
        assert!(
            max > 0.05,
            "{}: the push path produced effectively nothing (peak {max}) from a \
             0.5-amplitude sine; measured peaks on this corpus are 0.25..0.63",
            unit.label
        );
        // And it must not be wildly hotter than the input either — an AU fed its
        // own output would run away.
        assert!(
            max < 4.0,
            "{}: peak {max} from a 0.5 input suggests the AU is being fed its own \
             output rather than the staged input",
            unit.label
        );
    }
}

/// The push and pull paths must produce the *same* audio from the same input.
///
/// This is the strongest available check that the push path is wired correctly:
/// `AudioUnitRender` is the path the crate has always used and every other test
/// exercises, so agreement between the two means the push path's buffer binding,
/// timestamps and frame counts are all right. A path that merely "produced
/// plausible audio" could still be off by a block, a channel, or a sample.
///
/// Measured bit-exact on AUDelay: peaks `0.35350975`, `0.35353398`, `0.35354853`,
/// … identical across 8 blocks from both paths. So the comparison is written as
/// exact equality rather than with a tolerance — a tolerance here would hide
/// exactly the drift the test is for. Two separate instances are used, each
/// starting from a fresh delay line, so their internal state stays in step.
#[test]
fn the_push_path_matches_the_pull_path_sample_for_sample() {
    let _g = lock();
    let mut push_au = DELAY.open(RATE, BLOCK);
    let mut pull_au = DELAY.open(RATE, BLOCK);
    let mut scratch = stereo_scratch();

    for blk in 0..8usize {
        let input = sine(2, BLOCK as usize, blk * BLOCK as usize, 0.5, RATE as f32);
        let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();

        scratch.stage_input(0, &ins, BLOCK);
        push_au
            .process_push(&mut scratch, BLOCK)
            .expect("push render");
        let mut push_out = silence(2, BLOCK as usize);
        let mut push_slices: Vec<&mut [f32]> =
            push_out.iter_mut().map(|v| v.as_mut_slice()).collect();
        scratch.emit_output(0, &mut push_slices, BLOCK);
        drop(push_slices);

        let mut pull_out = silence(2, BLOCK as usize);
        render(&mut pull_au, &input, &mut pull_out, BLOCK).expect("pull render");

        assert_eq!(
            push_out, pull_out,
            "block {blk}: AudioUnitProcess and AudioUnitRender must produce \
             identical audio from identical input — they were measured bit-exact, \
             so any difference is a wiring bug in the push path, not tolerance"
        );
        assert!(all_finite(&push_out), "block {blk}: non-finite output");
    }
}

/// An oversized block must be refused by the *host*, before the AU is asked.
///
/// The AU also refuses — measured `kAudioUnitErr_TooManyFramesToProcess`
/// (-10874) for a 4×-block render on AUDelay — but only after being handed an
/// `AudioBufferList` whose `mDataByteSize` claims storage the host never
/// allocated. So the guard has to fire first, and the assertion checks *which*
/// error came back rather than merely that one did: an `is_err()` here would pass
/// on the AU's refusal and prove nothing about the host.
#[test]
fn the_push_path_refuses_an_oversized_block() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    let mut scratch = stereo_scratch();
    assert_eq!(scratch.block_size(), BLOCK);

    match au.process_push(&mut scratch, BLOCK + 1) {
        Err(AuError::InvalidBuffer(msg)) => {
            assert!(
                msg.contains(&(BLOCK + 1).to_string()) && msg.contains(&BLOCK.to_string()),
                "the rejection should name both the requested and the configured \
                 frame counts; got {msg:?}"
            );
        }
        other => panic!(
            "a {}-frame render on a {BLOCK}-frame scratch must be refused host-side \
             with InvalidBuffer, not forwarded to the AU (which answers \
             {TOO_MANY_FRAMES} — a refusal that arrives only after it has been \
             handed a buffer list that overstates its own size). Got {other:?}",
            BLOCK + 1
        ),
    }

    // The exact-size render still works, so the bound is `>` and not `>=`.
    let input = sine(2, BLOCK as usize, 0, 0.5, RATE as f32);
    let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
    scratch.stage_input(0, &ins, BLOCK);
    au.process_push(&mut scratch, BLOCK)
        .expect("a render at exactly block_size must be admitted");
}

/// The push path must be refused before `initialize`, as the pull path is.
///
/// `AudioUnitProcess` renders through buffers the AU allocated at
/// `AudioUnitInitialize`, so a pre-init push is not merely useless — it is a
/// render against unallocated resources. The typestate check must therefore
/// mirror `process`'s, and the assertion pins the same `kAudioUnitErr_Uninitialized`
/// rather than a substring of the Debug output.
#[test]
fn the_push_path_is_refused_before_initialize() {
    let _g = lock();
    let mut au = DELAY.open_uninitialized(RATE, BLOCK);
    let mut scratch = stereo_scratch();
    assert!(!au.is_initialized());

    let err = au
        .process_push(&mut scratch, BLOCK)
        .expect_err("process_push in the Loaded state must be an error");
    assert!(
        matches!(
            err,
            AuError::OsStatus {
                code: tutti_au_host::types::K_AUDIO_UNIT_ERR_UNINITIALIZED,
                ..
            }
        ),
        "expected kAudioUnitErr_Uninitialized, got {err:?}"
    );

    let err = au
        .process_push_multiple(&mut scratch, BLOCK)
        .expect_err("process_push_multiple in the Loaded state must be an error");
    assert!(
        matches!(
            err,
            AuError::OsStatus {
                code: tutti_au_host::types::K_AUDIO_UNIT_ERR_UNINITIALIZED,
                ..
            }
        ),
        "expected kAudioUnitErr_Uninitialized, got {err:?}"
    );
}

/// A unit that does not implement `AudioUnitProcess` must surface `unimpErr`, not
/// silence.
///
/// The status is asserted exactly, because `unimpErr` is a specific and
/// actionable fact — "this AU has no push path, use `AudioUnitRender`" — whereas
/// a generic error is indistinguishable from a render that failed for a real
/// reason. A host that absorbed this into a zeroed output buffer would put a
/// silent plugin in the middle of a chain with nothing to explain it.
#[test]
fn units_without_a_push_path_report_unimplemented() {
    let _g = lock();
    for unit in WITHOUT_PUSH_RENDER {
        let mut au = unit.open(RATE, BLOCK);
        // Instruments and mixers report their own widths; the scratch only needs
        // to be well-formed enough for the call to reach the AU.
        let mut scratch =
            PushScratch::new(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO], BLOCK);
        let err = au
            .process_push(&mut scratch, BLOCK)
            .expect_err("a unit that does not implement the selector must error");
        match err {
            AuError::RenderFailed {
                function: "AudioUnitProcess",
                code: UNIMP_ERR,
                ..
            } => {}
            other => panic!(
                "{} is recorded in corpus.rs as not implementing AudioUnitProcess, \
                 so the host must surface unimpErr ({UNIMP_ERR}) from \
                 AudioUnitProcess — got {other:?}. If this unit now implements the \
                 selector, move it to WITH_PUSH_RENDER rather than relaxing this.",
                unit.label
            ),
        }
    }
}

// ------------------------------------------------ AudioUnitProcessMultiple

/// `AudioUnitProcessMultiple` is not implemented by anything in the corpus except
/// AUReverb2 — and therefore **there is no working AU sidechain on this machine**.
///
/// Asserted rather than skipped, because a measured absence that is not pinned
/// silently becomes a measured presence the day the platform changes, and a host
/// that had quietly stopped exercising the path would not find out. Every unit
/// listed answers `unimpErr`; if one starts implementing the selector this fails
/// and names it, which is the correct outcome — a sidechain would then be
/// buildable and the module docs would need rewriting.
///
/// The AUMultiChannelMixer row is the load-bearing one: it has **8** real input
/// elements (`corpus.rs` pins the count, and `au_multibus.rs` asserts it), and it
/// still answers `unimpErr` for 1, 2 and 8 input lists alike. So the absence is
/// the *selector* and not a topology mismatch the host could work around by
/// configuring more buses.
#[test]
fn process_multiple_is_unimplemented_across_the_corpus() {
    let _g = lock();
    let mut implemented = Vec::new();

    for unit in WITHOUT_PUSH_RENDER.iter().chain(WITH_PUSH_RENDER.iter()) {
        if unit.sub_type == IMPLEMENTS_PROCESS_MULTIPLE.sub_type {
            continue;
        }
        let mut au = unit.open(RATE, BLOCK);
        // One input list — the least the selector could possibly accept. If it
        // refuses even this, it does not implement it at all.
        let mut scratch =
            PushScratch::new(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO], BLOCK);
        match au.process_push_multiple(&mut scratch, BLOCK) {
            Err(AuError::RenderFailed {
                function: "AudioUnitProcessMultiple",
                code: UNIMP_ERR,
                ..
            }) => {}
            Ok(_) => implemented.push(unit.label),
            other => panic!(
                "{}: expected unimpErr ({UNIMP_ERR}) from AudioUnitProcessMultiple; \
                 got {other:?}",
                unit.label
            ),
        }
    }

    assert!(
        implemented.is_empty(),
        "{implemented:?} now implement AudioUnitProcessMultiple. That is good news — \
         an AU sidechain may be buildable — but `offline`'s module docs and \
         `IMPLEMENTS_PROCESS_MULTIPLE` say it is unreachable, so both need updating \
         rather than this assertion relaxing."
    );
}

/// AUMultiChannelMixer refuses `AudioUnitProcessMultiple` even for as many input
/// lists as it has input elements.
///
/// Separated from the sweep above because it is the specific claim that kills the
/// "configure more buses and it will work" theory. The mixer publishes 8 input
/// elements; 1, 2 and 8 lists are all refused with the same `unimpErr`.
#[test]
fn a_real_multi_element_unit_still_refuses_process_multiple() {
    let _g = lock();
    let mixer = support::corpus::MULTI_CHANNEL_MIXER;
    let mut au = mixer.open(RATE, BLOCK);
    let elements = au.bus_count(tutti_au_host::BusDirection::Input);
    assert!(
        elements >= 8,
        "{} is recorded with 8 input elements; got {elements}",
        mixer.label
    );

    for lists in [1usize, 2, 8] {
        let inputs = vec![ChannelLayout::STEREO; lists];
        let mut scratch = PushScratch::new(&inputs, &[ChannelLayout::STEREO], BLOCK);
        assert_eq!(scratch.input_bus_count(), lists);
        match au.process_push_multiple(&mut scratch, BLOCK) {
            Err(AuError::RenderFailed {
                function: "AudioUnitProcessMultiple",
                code: UNIMP_ERR,
                ..
            }) => {}
            other => panic!(
                "{} has {elements} input elements and was measured to refuse \
                 AudioUnitProcessMultiple with unimpErr for {lists} input list(s) — \
                 got {other:?}. If it now accepts them, the absence of an AU \
                 sidechain path is no longer a platform fact.",
                mixer.label
            ),
        }
    }
}

/// AUReverb2 — the one unit that implements the selector — renders correctly
/// through it for one input list, and refuses two.
///
/// The refusal of two is *correct*, not a bug: AUReverb2 has one input element,
/// and `kAudioUnitErr_InvalidElement` is the right answer to a host offering a
/// bus that does not exist. Asserting it is what shows the host is not silently
/// dropping the extra list — a dropped sidechain is a compressor that never
/// ducks, indistinguishable from one whose threshold is too high.
///
/// The `mDataByteSize` readback is the second half. Sample values alone cannot
/// distinguish "wrote 512 frames" from "wrote 64 frames, the rest of which were
/// near zero"; the size the AU reports back can. Measured 256 bytes for a
/// 64-frame stereo render, i.e. the full `frames * 4` per channel.
#[test]
fn the_one_unit_that_implements_process_multiple_renders_through_it() {
    let _g = lock();
    let unit = IMPLEMENTS_PROCESS_MULTIPLE;
    let mut au = unit.open(RATE, BLOCK);

    let mut scratch = PushScratch::new(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO], BLOCK);
    let mut out = silence(2, BLOCK as usize);
    let mut max = 0.0f32;

    for blk in 0..8usize {
        let input = sine(2, BLOCK as usize, blk * BLOCK as usize, 0.5, RATE as f32);
        let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
        scratch.stage_input(0, &ins, BLOCK);
        au.process_push_multiple(&mut scratch, BLOCK)
            .unwrap_or_else(|e| {
                panic!(
                    "{} is recorded as implementing AudioUnitProcessMultiple for one \
                 input list; block {blk} failed: {e:?}",
                    unit.label
                )
            });

        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|v| v.as_mut_slice()).collect();
        scratch.emit_output(0, &mut outs, BLOCK);
        assert!(
            all_finite(&out),
            "block {blk}: non-finite output; peak() would report 0.0 for an all-NaN \
             block, so finiteness is checked separately"
        );
        max = max.max(peak(&out));

        // Every output buffer must report the full frame count. A short write
        // would be invisible in the samples.
        let sizes = unsafe { offline::output_byte_sizes(&scratch) };
        let expected = BLOCK * std::mem::size_of::<f32>() as u32;
        assert_eq!(
            sizes,
            vec![expected; 2],
            "block {blk}: each of the 2 output channels must report {expected} bytes \
             ({BLOCK} frames x 4); a short write is not detectable from the samples"
        );
    }

    // Measured ~0.497 out of a 0.5 input over 8 blocks — the reverb is near
    // unity at this setting. The bounds are wide enough not to pin its gain and
    // tight enough to catch a dead render or a runaway one.
    assert!(
        (0.05..4.0).contains(&max),
        "{}: peak {max} from a 0.5-amplitude sine; measured ~0.497",
        unit.label
    );

    // Two input lists on a one-input-element unit: the AU's own InvalidElement.
    let mut wide = PushScratch::new(
        &[ChannelLayout::STEREO, ChannelLayout::STEREO],
        &[ChannelLayout::STEREO],
        BLOCK,
    );
    let input = sine(2, BLOCK as usize, 0, 0.5, RATE as f32);
    let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
    wide.stage_input(0, &ins, BLOCK);
    wide.stage_input(1, &ins, BLOCK);
    let err = au
        .process_push_multiple(&mut wide, BLOCK)
        .expect_err("a second input list on a one-element unit must be refused");
    match err {
        AuError::RenderFailed {
            function: "AudioUnitProcessMultiple",
            code: tutti_au_host::types::K_AUDIO_UNIT_ERR_INVALID_ELEMENT,
            ..
        } => {}
        other => panic!(
            "{} has one input element, so a second input buffer list must come back \
             as kAudioUnitErr_InvalidElement rather than being silently dropped — a \
             dropped sidechain is a compressor that never ducks. Got {other:?}",
            unit.label
        ),
    }
}

// ------------------------------------------------------------ scratch shape

/// A scratch with no output bus cannot render, and says so rather than reporting
/// a successful no-op.
///
/// An AU has to write somewhere; a zero-length output array is not a render. The
/// check exists because `PushScratch::new(&[], &[], n)` is a perfectly ordinary
/// thing for a host that miscounted its buses to build, and the failure would
/// otherwise be a silent stream of successful empty blocks.
#[test]
fn a_scratch_with_no_output_bus_is_refused() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    let mut no_out = PushScratch::new(&[ChannelLayout::STEREO], &[], BLOCK);
    assert_eq!(no_out.output_bus_count(), 0);

    assert!(
        matches!(
            au.process_push(&mut no_out, BLOCK),
            Err(AuError::InvalidBuffer(_))
        ),
        "AudioUnitProcess with no output bus must be refused"
    );
    assert!(
        matches!(
            au.process_push_multiple(&mut no_out, BLOCK),
            Err(AuError::InvalidBuffer(_))
        ),
        "AudioUnitProcessMultiple with no output bus must be refused"
    );
}

/// Staging into a bus the scratch does not have must report the miss.
///
/// `stage_input` returning `false` rather than panicking is what lets a host
/// detect a bus-count mismatch; silently discarding the audio would leave a
/// sidechain fed with nothing and no way to tell.
#[test]
fn staging_into_a_missing_bus_is_reported() {
    let _g = lock();
    let mut scratch = stereo_scratch();
    let input = sine(2, BLOCK as usize, 0, 0.5, RATE as f32);
    let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();

    assert!(scratch.stage_input(0, &ins, BLOCK), "bus 0 exists");
    assert!(
        !scratch.stage_input(1, &ins, BLOCK),
        "a one-input-bus scratch must report bus 1 as absent, not silently discard \
         the audio staged into it"
    );

    let mut out = silence(2, BLOCK as usize);
    let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|v| v.as_mut_slice()).collect();
    assert!(
        scratch.emit_output(0, &mut outs, BLOCK),
        "output bus 0 exists"
    );
    assert!(
        !scratch.emit_output(1, &mut outs, BLOCK),
        "a one-output-bus scratch must report bus 1 as absent"
    );
}

/// Unfed channels and short sources are zeroed, not left holding the previous
/// block.
///
/// Stale audio in an unfed sidechain channel makes a compressor duck against a
/// signal that is no longer there, and the artefact outlives the block that
/// caused it. This drives a loud block, then a block that supplies only channel 0
/// and only half its frames, and checks the untouched regions came back silent
/// through a real render.
#[test]
fn unfed_channels_and_short_sources_are_zeroed() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    // Bypass so the delay's own tail cannot be mistaken for stale scratch: what
    // is being tested is the host's staging, not the AU's memory.
    au.set_bypass(true).expect("AUDelay supports bypass");

    let mut scratch = stereo_scratch();
    let loud = vec![vec![1.0f32; BLOCK as usize]; 2];
    let ins: Vec<&[f32]> = loud.iter().map(|v| v.as_slice()).collect();
    scratch.stage_input(0, &ins, BLOCK);
    au.process_push(&mut scratch, BLOCK).expect("loud block");

    let mut out = silence(2, BLOCK as usize);
    let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|v| v.as_mut_slice()).collect();
    scratch.emit_output(0, &mut outs, BLOCK);
    drop(outs);
    assert!(
        peak(&out) > 0.5,
        "the loud block must actually have rendered loud (peak {}), or the next \
         assertion proves nothing",
        peak(&out)
    );

    // Now supply channel 0 only, and only half of it. Channel 1 and the tail of
    // channel 0 must come back silent rather than carrying the 1.0s above.
    let half = vec![0.0f32; BLOCK as usize / 2];
    let partial: Vec<&[f32]> = vec![half.as_slice()];
    scratch.stage_input(0, &partial, BLOCK);
    au.process_push(&mut scratch, BLOCK).expect("partial block");

    let mut out2 = vec![vec![f32::NAN; BLOCK as usize]; 2];
    let mut outs2: Vec<&mut [f32]> = out2.iter_mut().map(|v| v.as_mut_slice()).collect();
    scratch.emit_output(0, &mut outs2, BLOCK);
    drop(outs2);

    assert!(
        all_finite(&out2),
        "the render overwrote every sample, so nothing should still be the NaN the \
         destination was pre-filled with; a surviving NaN means emit_output left \
         part of the buffer untouched"
    );
    let p = peak(&out2);
    assert!(
        p < 0.01,
        "an all-but-silent input must not produce a peak of {p}; the 1.0s from the \
         previous block are being reused rather than the unfed channels zeroed"
    );
}

// ------------------------------------------------------------------ no-alloc

// The `assert_no_alloc` checks are inert unless `AllocDisabler` is this binary's
// global allocator — the `#[cfg(test)]` declaration in `src/lib.rs` does not
// apply to integration tests. Declared here for the same reason
// `au_process_no_alloc.rs` declares its own.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// Block size the no-alloc tests render at — small, so a per-block allocation has
/// the shortest possible period in which to hide.
const RT_BLOCK: u32 = 64;
/// Warm-up blocks, settling first-call lazy allocation inside the AU and the host
/// before the guarded region opens.
const WARMUP: usize = 32;
/// Guarded blocks. Larger than [`WARMUP`] so an allocation that fires only every
/// N blocks still has room to hit.
const GUARDED: usize = 256;

/// Buffers owned across the guarded region.
///
/// They must already exist when `assert_no_alloc` is entered: a helper that
/// allocated a `Vec` per render would trip on its own harness allocation and say
/// nothing about the host. Same shape, and same reason, as
/// `au_process_no_alloc.rs`'s `Bufs`.
struct RtBufs {
    in_l: [f32; RT_BLOCK as usize],
    in_r: [f32; RT_BLOCK as usize],
    out_l: [f32; RT_BLOCK as usize],
    out_r: [f32; RT_BLOCK as usize],
}

impl RtBufs {
    fn new() -> Self {
        let mut me = Self {
            in_l: [0.0; RT_BLOCK as usize],
            in_r: [0.0; RT_BLOCK as usize],
            out_l: [0.0; RT_BLOCK as usize],
            out_r: [0.0; RT_BLOCK as usize],
        };
        // Non-silent, so the guarded renders exercise the copy paths rather than
        // whatever fast path an AU might take for a zero block.
        for (i, (l, r)) in me.in_l.iter_mut().zip(me.in_r.iter_mut()).enumerate() {
            let v = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE as f32).sin() * 0.5;
            *l = v;
            *r = v * 0.5;
        }
        me
    }

    /// Stage, render, emit — one full push block, asserting it succeeded.
    ///
    /// The `expect` is load-bearing, not defensive, and the lesson is recorded in
    /// `au_process_no_alloc.rs`: a `process` that fails on **every** call
    /// allocates nothing, so a helper that discarded the result would make the
    /// whole no-alloc assertion vacuous — it would pass just as readily with no
    /// audio being produced at all. It formats only on the failure path, which
    /// fails the test anyway.
    fn push(&mut self, au: &mut tutti_au_host::AuInstance, scratch: &mut PushScratch) {
        let ins: &[&[f32]] = &[&self.in_l, &self.in_r];
        assert!(scratch.stage_input(0, ins, RT_BLOCK), "input bus 0 exists");
        au.process_push(scratch, RT_BLOCK)
            .expect("steady-state push render");
        let outs: &mut [&mut [f32]] = &mut [&mut self.out_l[..], &mut self.out_r[..]];
        assert!(
            scratch.emit_output(0, outs, RT_BLOCK),
            "output bus 0 exists"
        );
    }

    /// As [`Self::push`], through `AudioUnitProcessMultiple`.
    fn push_multiple(&mut self, au: &mut tutti_au_host::AuInstance, scratch: &mut PushScratch) {
        let ins: &[&[f32]] = &[&self.in_l, &self.in_r];
        assert!(scratch.stage_input(0, ins, RT_BLOCK), "input bus 0 exists");
        au.process_push_multiple(scratch, RT_BLOCK)
            .expect("steady-state push-multiple render");
        let outs: &mut [&mut [f32]] = &mut [&mut self.out_l[..], &mut self.out_r[..]];
        assert!(
            scratch.emit_output(0, outs, RT_BLOCK),
            "output bus 0 exists"
        );
    }
}

/// The push render path must not allocate in steady state.
///
/// `PushScratch` grows every one of its `Vec`s in `new` — the per-bus slabs, the
/// channel storage, and the `AudioBufferList` pointer arrays — so that the render
/// only ever writes through storage that already exists. This is what proves it,
/// and the property is not expressible in the types: `bind_list` rewriting the
/// `mData` field of an existing `AudioBuffer` and a `Vec` reallocating look the
/// same from the outside.
///
/// A violation aborts the process rather than failing the test — `AllocDisabler`
/// is not a catchable panic — so a regression appears as the binary dying with
/// `memory allocation of N bytes failed`, not as an assertion diff.
#[test]
#[ignore]
fn the_push_path_does_not_allocate() {
    let _g = lock();
    let mut au = DELAY.open(RATE, RT_BLOCK);
    let mut scratch =
        PushScratch::new(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO], RT_BLOCK);
    let mut b = RtBufs::new();

    for _ in 0..WARMUP {
        b.push(&mut au, &mut scratch);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..GUARDED {
            b.push(&mut au, &mut scratch);
        }
    });
}

/// `AudioUnitProcessMultiple` must not allocate either, on the one unit that
/// implements it.
///
/// Driven separately from the single-list path because it takes a different route
/// through `PushScratch`: it binds *both* the input and output lists and hands
/// over two pointer arrays, none of which the single-list path touches. Those
/// arrays are the most likely place for a `Vec` to be built per block — the
/// obvious implementation collects them at the call site — so this is the test
/// that pins them as fields.
#[test]
#[ignore]
fn process_multiple_does_not_allocate() {
    let _g = lock();
    let mut au = IMPLEMENTS_PROCESS_MULTIPLE.open(RATE, RT_BLOCK);
    let mut scratch =
        PushScratch::new(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO], RT_BLOCK);
    let mut b = RtBufs::new();

    for _ in 0..WARMUP {
        b.push_multiple(&mut au, &mut scratch);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..GUARDED {
            b.push_multiple(&mut au, &mut scratch);
        }
    });
}

/// Rendering with the offline flag set must not allocate.
///
/// The flag is a property write that happens once, outside the guard — what is
/// checked is that the *steady state afterwards* is no worse. An offline bounce
/// is exactly the situation in which a host renders millions of blocks back to
/// back, so a per-block allocation introduced by the flag would be paid once per
/// block for the whole export.
///
/// AUSampler is the subject because it is one of only two units that implement
/// the property at all, and it renders under MIDI rather than from an input bus —
/// so this also covers the pull path with the flag set, which the push tests
/// above cannot (AUSampler answers `unimpErr` to `AudioUnitProcess`).
#[test]
#[ignore]
fn offline_flagged_render_does_not_allocate() {
    let _g = lock();
    let mut au = support::corpus::SAMPLER.open_uninitialized(RATE, RT_BLOCK);
    au.set_offline_render(true)
        .expect("AUSampler implements OfflineRender");
    au.initialize()
        .expect("initialize with the offline flag set");
    assert!(au.is_offline_render().unwrap());

    au.send_midi(&[tutti_au_host::MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0xC000,
    )]);
    let mut b = RtBufs::new();
    let drive = |au: &mut tutti_au_host::AuInstance, b: &mut RtBufs| {
        let ins: &[&[f32]] = &[&b.in_l, &b.in_r];
        let outs: &mut [&mut [f32]] = &mut [&mut b.out_l[..], &mut b.out_r[..]];
        au.process(ins, outs, RT_BLOCK).expect("offline render");
    };

    for _ in 0..WARMUP {
        drive(&mut au, &mut b);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..GUARDED {
            drive(&mut au, &mut b);
        }
    });
}
