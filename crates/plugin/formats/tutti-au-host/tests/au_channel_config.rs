//! Channel-layout and block-size configuration.
//!
//! Two host capabilities a DAW cannot do without:
//!
//! - **A requested channel layout.** A stereo floor in `AuLoaded::new` or
//!   `StreamConfig::apply` puts mono out of reach — `apply` is `pub(crate)`, so
//!   there is no way around it from outside the crate, and a mono track pays for
//!   a doubled channel through every AU in its chain. Worse, a floor inside
//!   `apply` makes the failure *invisible*: the effective layout handed back is
//!   an accurate `Stereo`, indistinguishable from a genuine refusal.
//!
//! - **A block-size change.** Without `set_block_size`, changing the buffer size
//!   in a DAW's preferences means destroying and rebuilding every
//!   plugin instance.
//!
//! ## What "accepted" means here, and why the asymmetry is deliberate
//!
//! A refused *channel width* is not an error: the AU keeps its own layout and
//! `num_outputs()` reports what it actually accepted. A refused *block size* IS
//! an error, because `process` admits any `num_frames` up to the recorded block
//! size — a config holding more than the AU allocated turns that bound check
//! into a false negative. So the tests below assert a comparison against the AU
//! for the former and a hard `Err` for the latter.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_channel_config
//! ```
//!
//! Corpus units ship with macOS; a missing one **fails** rather than skipping —
//! see `support/corpus.rs`.

#![cfg(target_os = "macos")]

use std::sync::Mutex;

mod support;
use support::corpus::{
    all_finite, peak, render, silence, ACCEPTS_MONO, DELAY, DLS_SYNTH, EFFECTS, INSTRUMENTS,
    MATRIX_REVERB, REFUSES_MONO,
};

use tutti_au_host::bus::BusDirection;
use tutti_au_host::stream::{AuBusLayout, StreamConfig};
use tutti_au_host::{AuError, AuInstance};
use tutti_plugin_types::ChannelLayout;

/// As in `au_conformance.rs`: AudioToolbox tolerates concurrent use of distinct
/// units, but discovery walks a process-global registry and these tests open the
/// same units. Serialize.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// Poison recovery: the guard is only a serializer, so one panicking test must
/// not convert into N spurious failures.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// Build a config requesting `layout` on both sides.
///
/// `has_input` is passed in rather than assumed: an instrument has no input bus,
/// and claiming otherwise makes `apply` write a format to a scope that does not
/// exist.
fn config_at(layout: ChannelLayout, has_input: bool) -> StreamConfig {
    StreamConfig::new(
        RATE,
        BLOCK,
        AuBusLayout {
            inputs: layout,
            outputs: layout,
            has_input,
        },
    )
}

// ------------------------------------------------------------ channel layout

/// The headline capability: an AU opened in mono reports **one** output channel
/// and renders.
///
/// Measured on macOS 15.6 — 12 of the 15 Apple units probed accept a 1-channel
/// format and initialize at it. `ACCEPTS_MONO` names the corpus members that do.
/// Before this change every one of them reported 2, because `apply` rewrote the
/// requested width to `max(2)`.
#[test]
fn an_au_opened_in_mono_reports_one_output_channel() {
    let _g = lock();
    for unit in ACCEPTS_MONO {
        let info = unit.require();
        let has_input = unit.au_type == tutti_au_host::AuType::Effect;
        // SAFETY: `component` came from `AudioComponentFindNext` via the corpus,
        // so it is a live factory handle for the lifetime of this process.
        let mut au = unsafe {
            AuInstance::new_with_config(info.component, config_at(ChannelLayout::MONO, has_input))
        }
        .unwrap_or_else(|e| panic!("{}: instantiate in mono failed: {e:?}", unit.label));

        assert_eq!(
            au.num_outputs(),
            1,
            "{}: measured to accept a 1-channel format, so the host must report \
             1 output channel — a 2 here means the stereo floor is back",
            unit.label
        );
        au.initialize()
            .unwrap_or_else(|e| panic!("{}: initialize in mono failed: {e:?}", unit.label));

        // And it must actually render at that width, with ONE buffer.
        let input = silence(1, BLOCK as usize);
        let mut output = silence(1, BLOCK as usize);
        render(&mut au, &input, &mut output, BLOCK)
            .unwrap_or_else(|e| panic!("{}: mono render failed: {e:?}", unit.label));
        // Silence in, so the level must stay low — but assert finiteness too:
        // `peak` is built on `f32::max`, which returns the non-NaN operand, so a
        // buffer of NaN would pass a magnitude check alone.
        assert!(
            all_finite(&output),
            "{}: mono render produced non-finite samples",
            unit.label
        );
        assert!(
            peak(&output) < 1e-3,
            "{}: silence in should not produce {} out",
            unit.label,
            peak(&output)
        );
    }
}

/// A width the AU **refuses** must be reported as the width it kept — never as
/// the width that was asked for.
///
/// This is the honesty property the stereo floor destroyed. Measured on macOS
/// 15.6: AUMatrixReverb and DLSMusicDevice both refuse a mono output format with
/// `-10868` (`kAudioUnitErr_FormatNotSupported`) and keep 2 channels. A host that
/// trusted the request would size its buffers for 1 and hand the AU a buffer list
/// one channel short of what it writes.
///
/// Deliberately NOT asserting `Err`: a refused channel count is recoverable, and
/// `apply` reports the effective layout precisely so the caller can notice. What
/// must never happen is a silent agreement.
#[test]
fn a_refused_layout_reports_the_width_the_au_kept() {
    let _g = lock();
    for unit in REFUSES_MONO {
        let info = unit.require();
        let has_input = unit.au_type == tutti_au_host::AuType::Effect;
        let mut au = unsafe {
            AuInstance::new_with_config(info.component, config_at(ChannelLayout::MONO, has_input))
        }
        .unwrap_or_else(|e| panic!("{}: instantiate failed: {e:?}", unit.label));

        assert_eq!(
            au.num_outputs(),
            2,
            "{}: measured to REFUSE a mono output format (-10868), so the host \
             must report the 2 channels the AU kept, not the 1 requested",
            unit.label
        );

        // The independent check: ask the AU itself, not the config we just wrote.
        au.initialize()
            .unwrap_or_else(|e| panic!("{}: initialize failed: {e:?}", unit.label));
        let actual = au
            .bus_layout(BusDirection::Output, 0)
            .unwrap_or_else(|e| panic!("{}: output bus 0 layout: {e:?}", unit.label));
        assert_eq!(
            actual.count(),
            au.num_outputs() as u16,
            "{}: the config disagrees with the AU's own stream format",
            unit.label
        );

        // And it renders at the width it actually chose.
        let ch = au.num_outputs() as usize;
        let input = silence(ch, BLOCK as usize);
        let mut output = silence(ch, BLOCK as usize);
        render(&mut au, &input, &mut output, BLOCK)
            .unwrap_or_else(|e| panic!("{}: render at kept width failed: {e:?}", unit.label));
        assert!(all_finite(&output), "{}: non-finite output", unit.label);
    }
}

/// A wider-than-stereo request must also be honoured, so the layout is genuinely
/// caller-driven rather than "mono or the old default".
///
/// Measured on macOS 15.6: every unit in `ACCEPTS_MONO` also accepts a 4-channel
/// format. AUMatrixReverb takes 4 on output but keeps 2 on input, which is why
/// this asserts per-side against the AU rather than assuming symmetry.
#[test]
fn a_quad_request_is_honoured_where_the_au_takes_it() {
    let _g = lock();
    for unit in ACCEPTS_MONO {
        let info = unit.require();
        let has_input = unit.au_type == tutti_au_host::AuType::Effect;
        let mut au = unsafe {
            AuInstance::new_with_config(info.component, config_at(ChannelLayout::QUAD, has_input))
        }
        .unwrap_or_else(|e| panic!("{}: instantiate in quad failed: {e:?}", unit.label));
        assert_eq!(
            au.num_outputs(),
            4,
            "{}: measured to accept a 4-channel format",
            unit.label
        );
        au.initialize()
            .unwrap_or_else(|e| panic!("{}: initialize in quad failed: {e:?}", unit.label));

        let input = silence(4, BLOCK as usize);
        let mut output = silence(4, BLOCK as usize);
        render(&mut au, &input, &mut output, BLOCK)
            .unwrap_or_else(|e| panic!("{}: quad render failed: {e:?}", unit.label));
        assert!(all_finite(&output), "{}: non-finite output", unit.label);
    }
}

/// `new` must keep its stereo default, because the `tutti-plugin-server` loader
/// calls it and a silent narrowing there would change every existing host's
/// channel count.
///
/// The floor moved from `apply` into `new`; this pins that it is still applied at
/// the one entry point that has to invent a width.
#[test]
fn the_layout_less_constructor_still_defaults_to_stereo() {
    let _g = lock();
    for unit in EFFECTS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        assert_eq!(
            au.num_outputs(),
            2,
            "{}: AuInstance::new must still default to stereo",
            unit.label
        );
    }
    // Instruments too: DLSMusicDevice probes 2 out natively and AUSampler is
    // floored up to 2, so both report stereo through the default constructor.
    for unit in INSTRUMENTS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        assert_eq!(
            au.num_outputs(),
            2,
            "{}: AuInstance::new must still default to stereo",
            unit.label
        );
        assert_eq!(
            au.num_inputs(),
            0,
            "{}: an instrument has no input bus",
            unit.label
        );
    }
}

/// `has_input` must agree with the AU's own input **element count** across every
/// installed unit.
///
/// ## What this does and does not prove
///
/// `probe()` derives `has_input` from `bus_count(Input) > 0` rather than from
/// whether the input stream-format read succeeded. Those two differ only for an
/// AU that *has* an input element but declines to report its format — for which
/// the old inference skipped installing the render callback on a unit that needs
/// one (the bug that made every instrument fail `initialize` with `-10877`).
///
/// **Measured: no unit installed on this machine distinguishes the two.** Swapping
/// `probe()` back to the format-read-derived semantics leaves this test green,
/// because every unit here that has input elements also reports an input format.
/// So this is a *consistency* assertion over the whole installed set, not a
/// discriminating one, and it is recorded as such rather than dressed up: it
/// pins that the flag matches the element count for all 52 units present, which
/// is what makes the callback-install decision correct today, and it will catch
/// the divergence on a machine that has a unit exhibiting it.
///
/// The discriminating case needs an AU that refuses `StreamFormat` on a real
/// input element. None ships with macOS; building one is `tests/au_misbehaving`
/// territory and is deliberately not attempted here.
///
/// Both legs are walked either way: measured on macOS 15.6, 45 units take the
/// with-input path and 7 the `in == 0` path.
#[test]
fn has_input_agrees_with_the_input_element_count() {
    let _g = lock();
    let mut with_input = 0usize;
    let mut without_input = 0usize;

    for au_type in [
        tutti_au_host::AuType::Effect,
        tutti_au_host::AuType::Instrument,
        tutti_au_host::AuType::Mixer,
    ] {
        for info in tutti_au_host::component::enumerate_components_of_type(au_type) {
            // SAFETY: `component` came from `AudioComponentFindNext`.
            let Ok(au) = (unsafe { AuInstance::new(info.component, RATE, BLOCK) }) else {
                // A unit that refuses to instantiate says nothing about
                // `has_input`; skipping it does not weaken the invariant, which
                // is asserted over the tally below.
                continue;
            };
            let element_count = au.bus_count(BusDirection::Input);
            // `num_inputs()` returns 0 exactly when `has_input` is false, so it
            // is the observable face of the flag. Note this shares `probe()`'s
            // source, which is why the module doc above is explicit that this
            // assertion is consistency rather than discrimination.
            let host_says_has_input = au.num_inputs() > 0;
            assert_eq!(
                host_says_has_input,
                element_count > 0,
                "{}: has_input must equal (input element count > 0); the AU \
                 reports {element_count} input elements",
                info.name
            );

            // The consequence that actually matters, asserted independently of
            // the flag: an AU with input elements must initialize (the render
            // callback install must have been attempted and accepted), and one
            // without must also initialize (the install must have been skipped).
            // This is the -10877 failure mode, and it does NOT route through
            // `num_inputs`.
            let mut au = au;
            if let Err(e) = au.initialize() {
                // Some units legitimately refuse (absent hardware/entitlement);
                // that is not this invariant's business. Only assert when the
                // refusal is the property error the bad gate produced.
                assert!(
                    !matches!(
                        e,
                        AuError::OsStatus {
                            code: tutti_au_host::types::K_AUDIO_UNIT_ERR_INVALID_ELEMENT,
                            ..
                        }
                    ),
                    "{}: initialize failed with kAudioUnitErr_InvalidElement \
                     ({element_count} input elements) — that is the render \
                     callback being installed on a scope the AU does not have",
                    info.name
                );
            }
            if element_count > 0 {
                with_input += 1;
            } else {
                without_input += 1;
            }
        }
    }

    // Both legs must actually have been walked, or the assertion above proved
    // nothing. Lower bounds, not the exact measured 45/7, so a machine with a
    // different plugin set still exercises the property.
    assert!(
        with_input >= 10,
        "expected many units with an input bus, saw {with_input}"
    );
    assert!(
        without_input >= 2,
        "expected at least the two instruments with no input bus, saw {without_input}"
    );
}

// ---------------------------------------------------------------- block size

/// `set_block_size` must change the reported size, and the AU must still render
/// — at the new size, and at a frame count only the new size admits.
#[test]
fn set_block_size_changes_the_size_and_the_au_still_renders() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    assert_eq!(au.block_size(), BLOCK);

    // Grow. 2048 is a real DAW buffer setting, and 4x the starting size, so a
    // stale scratch would be caught by the render below.
    au.set_block_size(2048).expect("grow the block size");
    assert_eq!(au.block_size(), 2048);
    assert!(
        au.is_initialized(),
        "set_block_size must restore the Ready state it found"
    );

    // A frame count the OLD size would have refused must now be admitted, and
    // the scratch must be big enough for it — that is the resize actually
    // happening rather than the config merely recording a bigger number.
    let input = silence(2, 2048);
    let mut output = silence(2, 2048);
    render(&mut au, &input, &mut output, 2048).expect("render a full 2048-frame block");
    assert!(all_finite(&output), "non-finite output after growing");

    // Shrink, and confirm the bound tightens again.
    au.set_block_size(256).expect("shrink the block size");
    assert_eq!(au.block_size(), 256);
    let input = silence(2, 256);
    let mut output = silence(2, 256);
    render(&mut au, &input, &mut output, 256).expect("render a 256-frame block");
    assert!(all_finite(&output), "non-finite output after shrinking");
}

/// After a shrink, an oversized block must still be refused — the guard has to
/// track the new size rather than the largest ever configured.
///
/// This is the direction that matters: if `process` kept admitting 2048 after a
/// shrink to 256, the AU would write past buffers it re-allocated for 256.
#[test]
fn an_oversized_block_is_still_refused_after_a_change() {
    let _g = lock();
    let mut au = DELAY.open(RATE, 2048);

    au.set_block_size(256).expect("shrink");
    assert_eq!(au.block_size(), 256);

    let input = silence(2, 2048);
    let mut output = silence(2, 2048);
    let err = render(&mut au, &input, &mut output, 2048)
        .expect_err("a 2048-frame render must be refused at a 256-frame block size");
    assert!(
        matches!(err, AuError::InvalidBuffer(_)),
        "expected InvalidBuffer for an oversized block, got {err:?}"
    );

    // The instance must still be usable at a legal size afterwards.
    let input = silence(2, 256);
    let mut output = silence(2, 256);
    render(&mut au, &input, &mut output, 256).expect("render at the legal size after a refusal");
}

/// A zero block size is refused outright, and must leave the instance untouched.
///
/// Zero would fail every `process` call's bound check and size the scratch to
/// empty buffers, so it is rejected before AudioToolbox ever sees it.
#[test]
fn a_zero_block_size_is_refused_and_changes_nothing() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let err = au
        .set_block_size(0)
        .expect_err("a zero block size must be an error");
    assert!(
        matches!(err, AuError::InvalidBuffer(_)),
        "expected InvalidBuffer for a zero block size, got {err:?}"
    );
    assert_eq!(
        au.block_size(),
        BLOCK,
        "a refused size must not be recorded"
    );
    assert!(
        au.is_initialized(),
        "a refused size must not tear the AU down"
    );

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("still renders after a refused change");
}

/// Setting the size the AU is already at is a no-op that preserves the Ready
/// state — a DAW re-applying its preferences must not silently uninitialize
/// every plugin.
#[test]
fn setting_the_same_block_size_preserves_the_ready_state() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    au.set_block_size(BLOCK).expect("same size is a no-op");
    assert_eq!(au.block_size(), BLOCK);
    assert!(au.is_initialized());

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("renders after a no-op change");
}

/// A block-size change must work from the `Loaded` state too, and must not
/// spontaneously initialize the AU.
///
/// `MaximumFramesPerSlice` is only writable while uninitialized, so this is the
/// path with no uninitialize/re-initialize dance around it at all.
#[test]
fn set_block_size_works_before_initialize() {
    let _g = lock();
    let mut au = DELAY.open_uninitialized(RATE, BLOCK);
    assert!(!au.is_initialized());

    au.set_block_size(1024)
        .expect("set the block size in the Loaded state");
    assert_eq!(au.block_size(), 1024);
    assert!(
        !au.is_initialized(),
        "set_block_size must not initialize an AU that was merely Loaded"
    );

    // And the size that was set while Loaded is the one that takes effect.
    au.initialize().expect("initialize after the change");
    let input = silence(2, 1024);
    let mut output = silence(2, 1024);
    render(&mut au, &input, &mut output, 1024).expect("render a 1024-frame block");
    assert!(all_finite(&output), "non-finite output");
}

/// The size must survive a sample-rate change, and vice versa: the two writes
/// share `StreamConfig::apply`, so one clobbering the other is a live risk.
#[test]
fn block_size_and_sample_rate_do_not_clobber_each_other() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    au.set_block_size(1024).expect("set block size");
    au.set_sample_rate(44_100.0).expect("set sample rate");
    assert_eq!(
        au.block_size(),
        1024,
        "a sample-rate change must not reset the block size"
    );
    assert_eq!(au.sample_rate(), 44_100.0);

    au.set_block_size(512).expect("set block size again");
    assert_eq!(
        au.sample_rate(),
        44_100.0,
        "a block-size change must not reset the sample rate"
    );
    assert_eq!(au.block_size(), 512);

    let input = silence(2, 512);
    let mut output = silence(2, 512);
    render(&mut au, &input, &mut output, 512).expect("render after both changes");
    assert!(all_finite(&output), "non-finite output");
}

/// Across the whole corpus: a block-size change either succeeds and the AU is
/// verifiably at that size, or it fails and the previous size is still recorded.
///
/// The one thing that must never happen is an `Ok` that records a size the AU is
/// not running at — `process` would then admit frame counts past what the AU
/// allocated for.
///
/// CAVEAT on what this proves locally: every Apple unit measured accepts every
/// block size offered (64 … 8192), so the *rejection* branch is not driven on
/// this machine. `AuError::BlockSizeRejected` is what would carry it, and this
/// test is the standing guarantee that the two agree for whatever an AU does —
/// including on a machine with a pickier one installed.
#[test]
fn a_block_size_is_either_applied_or_rolled_back() {
    let _g = lock();
    for unit in EFFECTS
        .iter()
        .chain(INSTRUMENTS)
        .chain([MATRIX_REVERB, DLS_SYNTH].iter())
    {
        let mut au = unit.open(RATE, BLOCK);
        for size in [64u32, 256, 1024, 4096, 8192] {
            let before = au.block_size();
            match au.set_block_size(size) {
                Ok(()) => {
                    assert_eq!(
                        au.block_size(),
                        size,
                        "{}: Ok must mean the size was recorded",
                        unit.label
                    );
                    let ch = au.num_outputs() as usize;
                    let input = silence(ch, size as usize);
                    let mut output = silence(ch, size as usize);
                    render(&mut au, &input, &mut output, size).unwrap_or_else(|e| {
                        panic!(
                            "{}: render at accepted size {size} failed: {e:?}",
                            unit.label
                        )
                    });
                    assert!(
                        all_finite(&output),
                        "{}: non-finite output at size {size}",
                        unit.label
                    );
                }
                Err(e) => {
                    assert_eq!(
                        au.block_size(),
                        before,
                        "{}: a rejected size ({e:?}) must leave the previous one \
                         recorded, not the rejected one",
                        unit.label
                    );
                }
            }
        }
    }
}
