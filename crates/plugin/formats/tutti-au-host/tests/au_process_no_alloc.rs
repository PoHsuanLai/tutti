//! RT-safety regression: the host's audio-thread operations must not allocate
//! in steady state.
//!
//! Mirrors `tutti-clap-host/tests/clap_process_no_alloc.rs` and
//! `tutti-vst3-host/tests/vst3_process_no_alloc.rs`. macOS-only; uses Apple's
//! stock units (always installed), so the suite runs anywhere the AU framework
//! is available — no third-party plugin required. `#[ignore]`'d to match the
//! other host-level rt tests:
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_process_no_alloc -- --ignored
//! ```
//!
//! ## What is covered, and why each one is here
//!
//! A single effect rendering silence exercises exactly one path, which is not
//! enough. The operations below are the ones a DAW actually performs from its
//! render thread, and each was *measured* rather than assumed:
//!
//! - **instrument render driven by MIDI**, because `AuInstance::send_midi`
//!   decodes every event through `MidiEvent::message()` (a UMP `normalize()`
//!   plus a `MidiMessage` construction) on what a host calls from the RT path.
//!   That decode is the suspicious part, and it is why this file does not stop
//!   at effects.
//! - **render after a preset load**, **bypass toggling**, **per-element
//!   parameter writes**, and **render after a sample-rate change** — host APIs
//!   that reach the AU through a property or parameter write rather than through
//!   `AudioUnitRender`, so `AudioUnitRender` alone does not cover them.
//!
//! Every one measured allocation-free on macOS 15.6 (Apple silicon), with a
//! scratch probe run while calibrating this file. So these are **regression
//! guards**, not bug reproductions: they exist to fail the day someone adds a
//! `Vec`, a `format!`, or a `CFString` to one of these paths.
//!
//! ## What this suite cannot prove
//!
//! Only that *these* call sequences do not allocate on *this* thread. The
//! repo's `CLAUDE.md` is explicit that a no-alloc test cannot pin a **race**:
//! an RT deallocation caused by a concurrent publisher is a scheduling hazard,
//! and sampling schedules cannot exhaust one. Nothing here is evidence about
//! cross-thread publication.
//!
//! ## Why a violation aborts rather than fails
//!
//! `AllocDisabler` aborts the process on violation; it is not a catchable
//! panic. A regression therefore shows up as the test binary dying with
//! `memory allocation of N bytes failed` on stderr, not as a tidy assertion
//! diff. That abort was observed deliberately (a `Vec::with_capacity` under the
//! guard) while calibrating this file — which is also what confirms the
//! allocator hook is armed at all, the property every assertion here rests on.

#![cfg(target_os = "macos")]

use std::sync::Mutex;

use assert_no_alloc::AllocDisabler;
use tutti_au_host::bus::BusDirection;
use tutti_au_host::instance::AuInstance;
use tutti_au_host::parameters::{self, ParamAddress};
use tutti_au_host::MidiEvent;

mod support;
use support::corpus;
use tutti_midi_types::{CCNumber, MidiChannel, MidiGroup};

// The `assert_no_alloc` checks below are inert unless `AllocDisabler` is the
// active global allocator for THIS test binary. The `#[cfg(test)]` decl in
// `src/lib.rs` does not apply to integration tests, so declare it here —
// matching `tutti-vst3-host` / `tutti-clap-host`.
#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Serializes AU instantiation across these tests.
///
/// Poison is recovered rather than propagated: an ordinary assertion failure in
/// one test would otherwise convert every later test into a spurious
/// `PoisonError` and hide which one actually broke.
static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    PLUGIN_LOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Block size every test in this file renders at.
const BLOCK: u32 = 64;

/// Warm-up blocks: settles first-call lazy allocation inside the AU and the
/// host adapter before the guarded region opens.
const WARMUP: usize = 32;

/// Guarded blocks. Larger than [`WARMUP`] so an allocation that fires only
/// every N blocks — a ring that grows periodically — still has room to hit.
const GUARDED: usize = 256;

/// Stereo scratch, allocated once per test *before* the guarded region.
///
/// The buffers must already exist when `assert_no_alloc` is entered: a helper
/// that allocated a `Vec` per render would trip on its own harness allocation
/// and say nothing about the host. That is why this is a struct owning arrays
/// rather than a closure that builds buffers per call.
struct Bufs {
    in_l: [f32; BLOCK as usize],
    in_r: [f32; BLOCK as usize],
    out_l: [f32; BLOCK as usize],
    out_r: [f32; BLOCK as usize],
}

impl Bufs {
    fn new() -> Self {
        Self {
            in_l: [0.0; BLOCK as usize],
            in_r: [0.0; BLOCK as usize],
            out_l: [0.0; BLOCK as usize],
            out_r: [0.0; BLOCK as usize],
        }
    }

    /// Render one block, asserting it actually succeeded.
    ///
    /// The `expect` is load-bearing, not defensive. This helper's ancestor
    /// discarded the result with `let _`, which made the whole no-alloc
    /// assertion vacuous: a `process` that failed on **every** call allocates
    /// nothing, so the test passed just as readily when no audio was being
    /// produced at all. Any drive helper added to this file must assert success
    /// for the same reason.
    ///
    /// `expect` is not itself an allocation risk inside the guarded section: it
    /// formats only on the failure path, and a failure fails the test anyway.
    fn render(&mut self, au: &mut AuInstance) {
        let ins: &[&[f32]] = &[&self.in_l, &self.in_r];
        let outs: &mut [&mut [f32]] = &mut [&mut self.out_l[..], &mut self.out_r[..]];
        au.process(ins, outs, BLOCK).expect("steady-state render");
    }
}

/// [`Bufs`] at f64, for the `process_f64` path.
///
/// A separate struct rather than a generic one: the arrays must be owned at a
/// concrete width to exist before the guard opens, which is the whole point of
/// [`Bufs`].
struct Bufs64 {
    in_l: [f64; BLOCK as usize],
    in_r: [f64; BLOCK as usize],
    out_l: [f64; BLOCK as usize],
    out_r: [f64; BLOCK as usize],
}

impl Bufs64 {
    fn new() -> Self {
        Self {
            in_l: [0.0; BLOCK as usize],
            in_r: [0.0; BLOCK as usize],
            out_l: [0.0; BLOCK as usize],
            out_r: [0.0; BLOCK as usize],
        }
    }

    /// Render one f64 block, asserting success for the same reason
    /// [`Bufs::render`] does.
    fn render(&mut self, au: &mut AuInstance) {
        let ins: &[&[f64]] = &[&self.in_l, &self.in_r];
        let outs: &mut [&mut [f64]] = &mut [&mut self.out_l[..], &mut self.out_r[..]];
        au.process_f64(ins, outs, BLOCK)
            .expect("steady-state f64 render");
    }
}

/// Steady-state effect render must not allocate.
///
/// The original coverage, kept as the baseline. If `process` grows a per-block
/// allocation — a resized scratch, a `format!` on a hot path — a DAW's audio
/// thread starts taking the allocator lock every 1.3ms at 48kHz, which is heard
/// as intermittent dropouts under load rather than seen as a crash.
#[test]
#[ignore]
fn effect_render_does_not_allocate() {
    let _lock = lock();
    let mut au = corpus::DELAY.open(48_000.0, BLOCK);
    let mut b = Bufs::new();

    for _ in 0..WARMUP {
        b.render(&mut au);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..GUARDED {
            b.render(&mut au);
        }
    });
}

/// Steady-state **f64** effect render must not allocate — the regression #266
/// reported.
///
/// AUv2 renders only in f32, so an f64 host pays a narrowing round trip on every
/// block. The conversion is unavoidable; the *allocation* was not. Until this
/// was fixed, the server loader built that conversion scratch inline — two
/// `Vec<Vec<f32>>` buffer sets plus two `Vec` pointer tables, four heap
/// allocations per block on the audio thread.
///
/// This test is the reason the bug survived: every other case in this file
/// drives `process`, and none of them touch f64. The sibling fixes in #262 were
/// gated behind a plugin feature bit, but nothing gates this one — sample width
/// is the host pipeline's choice, so an AU on a `Float64` pipeline paid it on
/// every block unconditionally.
///
/// Guards the host-side conversion specifically. The four `Vec`s that motivated
/// the issue lived one layer up, in `tutti-plugin-server`'s AU loader; moving
/// the conversion here is what let it reuse the render scratch that already
/// existed, so this is where the property is now enforceable.
#[test]
#[ignore]
fn f64_effect_render_does_not_allocate() {
    let _lock = lock();
    let mut au = corpus::DELAY.open(48_000.0, BLOCK);
    let mut b = Bufs64::new();

    for _ in 0..WARMUP {
        b.render(&mut au);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..GUARDED {
            b.render(&mut au);
        }
    });
}

/// An instrument rendering under MIDI must not allocate.
///
/// Instruments are the case the effect test cannot reach: they pull no input
/// bus, and their output is driven entirely by the events the host feeds them.
/// A host that allocates here glitches exactly while the user is playing — the
/// worst possible moment, and one that never reproduces on an idle timeline.
#[test]
#[ignore]
fn instrument_render_with_midi_does_not_allocate() {
    let _lock = lock();
    let mut au = corpus::DLS_SYNTH.open(48_000.0, BLOCK);
    let mut b = Bufs::new();

    au.send_midi(&[MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0xC000,
    )]);
    for _ in 0..WARMUP {
        b.render(&mut au);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..GUARDED {
            b.render(&mut au);
        }
    });
}

/// Decoding MIDI events for delivery must not allocate.
///
/// This is the case that motivated widening the file. `send_midi` runs every
/// event through `MidiEvent::message()`, which calls `normalize()` and builds a
/// `MidiMessage` — a decode the host performs per event, per block, on the RT
/// path. It measured allocation-free, so this guards the property rather than
/// reporting a bug: the day that decode grows a heap-backed variant (a SysEx
/// payload `Vec`, say), every note a user plays allocates on the audio thread.
///
/// All four families that reach `MusicDeviceMIDIEvent` are driven, plus one
/// that does not: `per_note_pitch_bend` has no legacy 3-byte form and takes the
/// `_ => continue` arm. The skipped family is included deliberately — a decode
/// that allocated *before* discovering it had nothing to send would be invisible
/// to a test that only drove the delivered families.
#[test]
#[ignore]
fn send_midi_decode_does_not_allocate() {
    let _lock = lock();
    let mut au = corpus::DLS_SYNTH.open(48_000.0, BLOCK);
    let mut b = Bufs::new();

    au.send_midi(&[MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0xC000,
    )]);
    for _ in 0..WARMUP {
        b.render(&mut au);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..GUARDED {
            let note = 60 + (i % 12) as u8;
            au.send_midi(&[
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, note, 0xC000),
                MidiEvent::cc(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    CCNumber::VOLUME,
                    0x4000_0000,
                ),
                MidiEvent::pitch_bend(MidiGroup::FIRST, MidiChannel::FIRST, 0x4000_0000),
                // No legacy channel-voice form: exercises the skip arm.
                MidiEvent::per_note_pitch_bend(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    note,
                    0x4000_0000,
                ),
                MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, note, 0),
            ]);
            b.render(&mut au);
        }
    });
}

/// Rendering after a factory preset load must not allocate.
///
/// A preset load replaces the AU's whole parameter set at once, which is the
/// moment an AU is most likely to resize its internal buffers. The host must
/// not add allocation of its own on the blocks that follow: automating a preset
/// change mid-playback is ordinary DAW usage, and a per-block allocation
/// introduced by it would be blamed on the plugin.
///
/// AUDistortion is the subject because it carries the widest factory-preset
/// table in the corpus (22, pinned in `support/corpus.rs`) and is strongly
/// non-linear, so a load genuinely moves its state.
#[test]
#[ignore]
fn render_after_preset_load_does_not_allocate() {
    let _lock = lock();
    let mut au = corpus::DISTORTION.open(48_000.0, BLOCK);
    let mut b = Bufs::new();

    // Enumerated before the guarded region: the walk copies CFStrings into
    // owned `String`s and is emphatically not an RT operation.
    let numbers: Vec<i32> = au.factory_presets().iter().map(|p| p.number).collect();
    assert!(
        !numbers.is_empty(),
        "AUDistortion reported no factory presets; corpus.rs pins it at 22, so \
         an empty table means the CFArray walk regressed rather than that this \
         test needs relaxing"
    );

    au.load_factory_preset(numbers[0])
        .expect("load first factory preset");
    for _ in 0..WARMUP {
        b.render(&mut au);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..GUARDED {
            b.render(&mut au);
        }
    });
}

/// Toggling bypass between blocks must not allocate.
///
/// Bypass is a `kAudioUnitProperty_BypassEffect` write — an
/// `AudioUnitSetProperty` the host issues while the stream is running, which is
/// how every DAW implements a bypass button clicked during playback. If that
/// write allocates, the click that bypasses a plugin is also the click that
/// drops a buffer.
#[test]
#[ignore]
fn bypass_toggle_during_render_does_not_allocate() {
    let _lock = lock();
    let mut au = corpus::DISTORTION.open(48_000.0, BLOCK);
    let mut b = Bufs::new();

    au.set_bypass(false).expect("AUDistortion supports bypass");
    for _ in 0..WARMUP {
        b.render(&mut au);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..GUARDED {
            au.set_bypass(i % 2 == 0).expect("toggle bypass");
            b.render(&mut au);
        }
    });
}

/// Per-element parameter writes must not allocate.
///
/// `parameters::set_at` is what a mixer strip drives: one parameter id, a
/// different element per bus, written every time a fader moves. A host that
/// allocates per write turns a fader drag — hundreds of writes a second — into
/// hundreds of allocator acquisitions a second on the audio thread.
///
/// The **error path is driven too**, against element 9999, which the AU rejects
/// with `kAudioUnitErr_InvalidElement`. That arm is included because it is the
/// one most likely to grow a `format!`: error construction is where allocation
/// hides, and a suite that only drove the success path would never see it.
#[test]
#[ignore]
fn per_element_parameter_writes_do_not_allocate() {
    let _lock = lock();
    let mut au = corpus::MULTI_CHANNEL_MIXER.open(48_000.0, BLOCK);
    let mut b = Bufs::new();

    let unit = au.raw_unit();
    let bus0 = ParamAddress::on_bus(BusDirection::Input, 0);
    let strip = parameters::list_at(unit, bus0);
    let pid = strip
        .first()
        .expect(
            "AUMultiChannelMixer published no input-element parameters; \
             corpus.rs records a 7-parameter strip per input element",
        )
        .id;
    // Far past the 8 input elements the unit actually has, so `set_at` must
    // report the AU's own rejection rather than aliasing onto element 0.
    let missing = ParamAddress::on_bus(BusDirection::Input, 9_999);

    for _ in 0..WARMUP {
        b.render(&mut au);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..GUARDED {
            let addr = ParamAddress::on_bus(BusDirection::Input, (i % 4) as u32);
            parameters::set_at(unit, addr, pid, 0.5).expect("write live element");
            // Asserted as an error rather than ignored: if this ever starts
            // succeeding, the write is landing on an element the host never
            // addressed.
            assert!(
                parameters::set_at(unit, missing, pid, 0.5).is_err(),
                "write to element 9999 must be rejected, not silently aliased"
            );
            b.render(&mut au);
        }
    });
}

/// Rendering after a sample-rate change must not allocate.
///
/// A rate change tears the AU down and rebuilds it — `uninitialize`, reapply
/// the stream format, `initialize` — and reallocates the host's render scratch
/// at the new size. That is legitimate, and happens outside the guard here.
/// What must not happen is the *steady state afterwards* being permanently
/// worse than before: a scratch left mis-sized against the new rate would
/// reallocate on every subsequent block. Switching the audio device's rate
/// mid-session is the real trigger, and it is the kind of regression that only
/// shows up on the machine that switched.
#[test]
#[ignore]
fn render_after_sample_rate_change_does_not_allocate() {
    let _lock = lock();
    let mut au = corpus::DELAY.open(48_000.0, BLOCK);
    let mut b = Bufs::new();

    for _ in 0..WARMUP {
        b.render(&mut au);
    }
    au.set_sample_rate(44_100.0)
        .expect("AUDelay accepts 44.1kHz");
    // Re-warm at the new rate: the rebuild's own allocation is expected, and is
    // deliberately left outside the guard.
    for _ in 0..WARMUP {
        b.render(&mut au);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..GUARDED {
            b.render(&mut au);
        }
    });
}
