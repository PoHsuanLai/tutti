//! Render notifications and scheduled parameters, against real Audio Units.
//!
//! What this suite proves about the host:
//!
//! - the pre/post notify pair **fires**, once each, pre before post, per render
//! - the flags and frame count each half reports match what was actually rendered
//! - dropping the handle **stops** the notify — observed by counting deliveries,
//!   not by inspecting a pointer — and does not crash, which is the
//!   use-after-free case the removal ordering exists to prevent
//! - a scheduled *immediate* parameter change takes effect
//! - a scheduled *ramp* does what the installed plugins actually do with it,
//!   which is not what `kAudioUnitParameterFlag_CanRamp` advertises
//!
//! ## The two rules this suite is written against
//!
//! **Observe the count, not the nullness.** `the_notify_stops_after_the_handle_is_dropped`
//! asserts on a delivery counter that the callback itself increments. An
//! over-release passes a whole suite that only checks a pointer for non-null —
//! a resource-balance claim has to watch the resource.
//!
//! **A small value must also be a finite one.** `peak()` folds with `f32::max`,
//! which returns the non-NaN operand, so an all-NaN buffer has a peak of `0.0`
//! and sails through any "is it quiet?" assertion. Every smallness assertion
//! here is paired with [`all_finite`].
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_render_notify
//! ```
//!
//! No env vars, no SDK, no display. A missing **Apple** unit fails rather than
//! skipping — see `support/corpus.rs`. The one third-party unit this suite can
//! use is reached through `corpus::optional_third_party` and its absence is
//! tolerated, because a third-party plugin is a genuine optional install; the
//! test that uses it still asserts an unconditional property either way.

#![cfg(target_os = "macos")]

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

mod support;
use support::corpus::{
    all_finite, envelope, open_info, optional_third_party, peak, render, silence, DELAY, DYNAMICS,
    SPATIAL_MIXER, SPATIAL_MIXER_RAMP_PARAM, TAL_REVERB_4,
};

use tutti_au_host::component::AuType;
use tutti_au_host::render_notify::{ParamEvent, RenderPhase, ScheduleAddress};
use tutti_au_host::types::{
    K_AUDIO_UNIT_RENDER_ACTION_POST_RENDER, K_AUDIO_UNIT_RENDER_ACTION_PRE_RENDER,
};

/// Serializes component discovery and instantiation, as every suite in this
/// crate does. AudioToolbox tolerates concurrent use of *distinct* units, but
/// discovery walks a process-global registry and several tests here open the
/// same unit. Mirrors `AU_LOCK` in `au_conformance.rs`.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// `AU_LOCK` is poisoned by any panicking test, and a poisoned lock would turn
/// one real failure into N spurious ones. The guard is only a serializer — there
/// is no shared state to be left inconsistent — so recover.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// What a notify records, as scalars an audio-thread callback may safely touch.
///
/// Every field is an atomic and nothing here allocates, which is the contract
/// `RenderNotify::new` documents. In particular the phase order is accumulated
/// into a single packed integer rather than pushed onto a `Vec`: a `Vec::push`
/// in a render notify can reallocate on the audio thread, which is exactly what
/// the no-allocation rule forbids. One decimal digit per call — `12` reads as
/// "pre then post".
#[derive(Default)]
struct Observed {
    pre: AtomicU32,
    post: AtomicU32,
    /// Packed phase order, one digit per delivery (1 = pre, 2 = post).
    order: AtomicU32,
    /// Frames and flags as reported by each half, so they can be compared
    /// against what was actually rendered.
    pre_frames: AtomicU32,
    post_frames: AtomicU32,
    pre_flags: AtomicU32,
    post_flags: AtomicU32,
    bus: AtomicU32,
    /// Whether post-render ever saw a non-null buffer list — the property that
    /// makes metering possible at all.
    post_saw_buffers: AtomicU32,
}

impl Observed {
    fn record(&self, n: &tutti_au_host::render_notify::RenderNotification) {
        let digit = match n.phase {
            RenderPhase::Pre => {
                self.pre.fetch_add(1, Ordering::SeqCst);
                self.pre_frames.store(n.frames, Ordering::SeqCst);
                self.pre_flags.store(n.flags, Ordering::SeqCst);
                1
            }
            RenderPhase::Post => {
                self.post.fetch_add(1, Ordering::SeqCst);
                self.post_frames.store(n.frames, Ordering::SeqCst);
                self.post_flags.store(n.flags, Ordering::SeqCst);
                if !n.io_data.is_null() {
                    self.post_saw_buffers.store(1, Ordering::SeqCst);
                }
                2
            }
        };
        self.bus.store(n.bus, Ordering::SeqCst);
        let _ = self
            .order
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(v.saturating_mul(10).saturating_add(digit))
            });
    }
}

// ------------------------------------------------------------ the pre/post pair

/// The notify must fire twice per render — pre then post, once each — and both
/// halves must report the frame count that was actually rendered.
///
/// This is the whole foundation: a host that gets one call, or two pre-renders,
/// or a post-render before a pre-render, cannot meter or schedule correctly.
/// Measured on macOS 15.6 against AUDelay: `pre=1 post=1`, order `12`, both
/// halves reporting 512 frames for a 512-frame render.
#[test]
fn pre_and_post_render_both_fire_once_each_in_that_order() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let obs = Arc::new(Observed::default());
    let sink = Arc::clone(&obs);
    let notify = au
        .add_render_notify(move |n| sink.record(&n))
        .expect("AUDelay must accept AudioUnitAddRenderNotify");

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("render");

    assert_eq!(
        obs.pre.load(Ordering::SeqCst),
        1,
        "exactly one pre-render notification per AudioUnitRender"
    );
    assert_eq!(
        obs.post.load(Ordering::SeqCst),
        1,
        "exactly one post-render notification per AudioUnitRender"
    );
    assert_eq!(
        obs.order.load(Ordering::SeqCst),
        12,
        "pre must be delivered before post (1 = pre, 2 = post, one digit per \
         delivery); a host that schedules parameters on the pre half depends on \
         this ordering"
    );
    assert_eq!(
        obs.pre_frames.load(Ordering::SeqCst),
        BLOCK,
        "the pre half must report the frame count being rendered"
    );
    assert_eq!(
        obs.post_frames.load(Ordering::SeqCst),
        BLOCK,
        "the post half must report the same frame count"
    );

    drop(notify);
}

/// Each half must carry **its own** phase bit and not the other's.
///
/// Worth asserting separately from the ordering above because `RenderPhase` is
/// decoded from a flags word by testing bits: an `else if` chain that tested the
/// post bit first would report every call as `Post` and the counts would still
/// come to 1 and 1 in some other arrangement. This pins the flags themselves.
///
/// Note the flags are *not* asserted equal to the phase bit alone — the AU is
/// free to set `OutputIsSilence` alongside, and AUDelay does on a silent input.
#[test]
fn each_half_carries_its_own_phase_flag() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let obs = Arc::new(Observed::default());
    let sink = Arc::clone(&obs);
    let notify = au.add_render_notify(move |n| sink.record(&n)).unwrap();

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("render");

    let pre = obs.pre_flags.load(Ordering::SeqCst);
    let post = obs.post_flags.load(Ordering::SeqCst);
    assert_ne!(
        pre & K_AUDIO_UNIT_RENDER_ACTION_PRE_RENDER,
        0,
        "the pre half's flags must have the PreRender bit set"
    );
    assert_eq!(
        pre & K_AUDIO_UNIT_RENDER_ACTION_POST_RENDER,
        0,
        "the pre half must not also claim PostRender"
    );
    assert_ne!(
        post & K_AUDIO_UNIT_RENDER_ACTION_POST_RENDER,
        0,
        "the post half's flags must have the PostRender bit set"
    );
    assert_eq!(
        post & K_AUDIO_UNIT_RENDER_ACTION_PRE_RENDER,
        0,
        "the post half must not also claim PreRender"
    );
    assert_eq!(
        obs.bus.load(Ordering::SeqCst),
        0,
        "this host renders bus 0, so that is what the notify must report"
    );

    drop(notify);
}

/// Post-render must hand over a non-null buffer list.
///
/// This is the property that makes the notify useful for metering: on post-render
/// `ioData` holds what the AU actually produced. Apple documents the buffer list
/// as nullable in general ("Can be null in the notification that input is
/// available"), so this asserts the *measured* behaviour for the post half of a
/// real effect render rather than assuming it.
#[test]
fn post_render_exposes_the_buffers_the_au_produced() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let obs = Arc::new(Observed::default());
    let sink = Arc::clone(&obs);
    let notify = au.add_render_notify(move |n| sink.record(&n)).unwrap();

    // A non-silent input, so the AU has something to produce and the buffer list
    // is not merely a formality.
    let input: Vec<Vec<f32>> = (0..2).map(|_| vec![0.25f32; BLOCK as usize]).collect();
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("render");

    assert_eq!(
        obs.post_saw_buffers.load(Ordering::SeqCst),
        1,
        "post-render must expose a non-null AudioBufferList — it is the only \
         place a host can meter the plugin's real output"
    );

    drop(notify);
}

/// The notify count must scale with renders, one pair per block.
///
/// Guards the case where the AU (or this host) installs the notify more than
/// once, or where a stale registration accumulates across blocks: 8 renders must
/// be 8 pre and 8 post, not 8 and 9, and not 16 of either.
#[test]
fn every_render_delivers_exactly_one_pair() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let obs = Arc::new(Observed::default());
    let sink = Arc::clone(&obs);
    let notify = au.add_render_notify(move |n| sink.record(&n)).unwrap();

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    const BLOCKS: u32 = 8;
    for _ in 0..BLOCKS {
        render(&mut au, &input, &mut output, BLOCK).expect("render");
    }

    assert_eq!(obs.pre.load(Ordering::SeqCst), BLOCKS);
    assert_eq!(obs.post.load(Ordering::SeqCst), BLOCKS);

    drop(notify);
}

// ---------------------------------------------------------------- removal

/// Dropping the handle must stop the notify, and must not crash.
///
/// This is the use-after-free case. While the notify is installed the AU holds a
/// raw pointer into the handle's boxed state and dereferences it on every
/// render; if `Drop` freed the state before calling
/// `AudioUnitRemoveRenderNotify`, the renders after the drop would call a freed
/// closure. That does not reliably crash — it reads whatever the allocator left
/// behind — so **the count is the evidence**, not survival.
///
/// Per the first rule in this module's docs: this observes a delivery counter
/// the callback increments, rather than checking a pointer for null. An
/// over-release is invisible to a nullness check — the pointer is null either
/// way — so only the count separates "stopped" from "never started".
#[test]
fn the_notify_stops_after_the_handle_is_dropped() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let obs = Arc::new(Observed::default());
    let sink = Arc::clone(&obs);
    let notify = au.add_render_notify(move |n| sink.record(&n)).unwrap();

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    for _ in 0..4 {
        render(&mut au, &input, &mut output, BLOCK).expect("render");
    }
    let before_pre = obs.pre.load(Ordering::SeqCst);
    let before_post = obs.post.load(Ordering::SeqCst);
    assert_eq!(before_pre, 4, "sanity: the notify was live before the drop");
    assert_eq!(before_post, 4);

    drop(notify);

    // Renders after the removal must still succeed — removing a notify must not
    // disturb the AU — and must deliver nothing.
    for _ in 0..8 {
        render(&mut au, &input, &mut output, BLOCK)
            .expect("removing a render notify must not break rendering");
    }

    assert_eq!(
        obs.pre.load(Ordering::SeqCst),
        before_pre,
        "no pre-render notification may be delivered after the handle is dropped \
         — a count that kept rising means the removal did not take, and the AU is \
         calling into freed memory"
    );
    assert_eq!(
        obs.post.load(Ordering::SeqCst),
        before_post,
        "no post-render notification may be delivered after the handle is dropped"
    );

    // The audio must still be sane after all that. Paired finite check: `peak`
    // folds with `f32::max`, which returns the non-NaN operand, so an all-NaN
    // buffer would report a peak of 0.0 and pass a smallness assertion alone.
    assert!(
        all_finite(&output),
        "output must remain finite after the notify is removed"
    );
    assert!(
        peak(&output) < 1.0e-6,
        "a silent input must still render silence after removal; peak was {}",
        peak(&output)
    );
}

/// Installing and dropping many notifies in sequence must stay balanced.
///
/// AudioToolbox keys registrations by the `(proc, ref_con)` tuple, and this host
/// derives `ref_con` from each handle's own allocation. If a removal ever missed
/// — wrong tuple, or a `Drop` that did not run — the stale registration would
/// still fire, so the final count after the last drop is the balance check.
///
/// Also exercises the allocator's freedom to reuse an address: a fresh handle
/// may well land on the address a dropped one occupied, which is precisely the
/// situation where a missed removal aliases a live registration onto dead state.
#[test]
fn repeated_install_and_drop_leaves_nothing_registered() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);

    let obs = Arc::new(Observed::default());
    for round in 1..=6u32 {
        let sink = Arc::clone(&obs);
        let notify = au.add_render_notify(move |n| sink.record(&n)).unwrap();
        render(&mut au, &input, &mut output, BLOCK).expect("render");
        assert_eq!(
            obs.pre.load(Ordering::SeqCst),
            round,
            "round {round}: exactly one live registration must be delivering"
        );
        drop(notify);
    }

    let settled = obs.pre.load(Ordering::SeqCst);
    for _ in 0..8 {
        render(&mut au, &input, &mut output, BLOCK).expect("render");
    }
    assert_eq!(
        obs.pre.load(Ordering::SeqCst),
        settled,
        "after every handle is dropped, no registration may remain"
    );
}

/// Two notifies on one unit must both fire, and dropping one must not remove the
/// other.
///
/// A DAW does install more than one: a meter tap and an automation writer coexist
/// on the same plugin. Because AudioToolbox keys by `(proc, ref_con)` and this
/// crate uses a single `extern "C"` proc for every handle, the `ref_con` is the
/// *only* thing distinguishing them — so a removal that passed the wrong pointer
/// would silently take out the wrong tap. That is what this pins.
#[test]
fn two_notifies_coexist_and_are_removed_independently() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let a = Arc::new(Observed::default());
    let b = Arc::new(Observed::default());
    let sink_a = Arc::clone(&a);
    let sink_b = Arc::clone(&b);
    let notify_a = au.add_render_notify(move |n| sink_a.record(&n)).unwrap();
    let notify_b = au.add_render_notify(move |n| sink_b.record(&n)).unwrap();

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("render");

    assert_eq!(
        a.pre.load(Ordering::SeqCst),
        1,
        "the first notify must fire"
    );
    assert_eq!(b.pre.load(Ordering::SeqCst), 1, "the second must fire too");

    // Drop only the first. The second must keep going.
    drop(notify_a);
    render(&mut au, &input, &mut output, BLOCK).expect("render");

    assert_eq!(
        a.pre.load(Ordering::SeqCst),
        1,
        "the dropped notify must stop"
    );
    assert_eq!(
        b.pre.load(Ordering::SeqCst),
        2,
        "dropping one notify must not remove the other — they are distinguished \
         only by ref_con, so a removal with the wrong pointer would take out this \
         one instead"
    );

    drop(notify_b);
    render(&mut au, &input, &mut output, BLOCK).expect("render");
    assert_eq!(b.pre.load(Ordering::SeqCst), 2, "the second notify stopped");
}

/// A panicking callback must not unwind into AudioToolbox.
///
/// Unwinding across `extern "C"` is undefined behaviour. The notify catches the
/// panic, prints to stderr and returns an OSStatus; what this asserts is that the
/// process **survives** and the AU keeps rendering. A stderr message is expected
/// output for this test, not a failure.
///
/// The render itself must still succeed: the notify is a tap, not a filter, so an
/// error status from it does not fail the render.
#[test]
fn a_panicking_callback_is_contained() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let calls = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let notify = au
        .add_render_notify(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            panic!("deliberate panic from a render notify callback");
        })
        .unwrap();

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    // If the panic unwound into AudioToolbox this would abort the process rather
    // than return.
    render(&mut au, &input, &mut output, BLOCK)
        .expect("a panicking notify must not fail the render — it is a tap");

    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the callback must have been reached for the panic to be under test"
    );
    assert!(all_finite(&output), "output must remain finite");

    drop(notify);

    // And the unit must still be usable afterwards.
    render(&mut au, &input, &mut output, BLOCK).expect("the AU survives a caught panic");
}

// ------------------------------------------------------- scheduled parameters

/// A scheduled *immediate* parameter change must take effect.
///
/// Scheduled from inside the pre-render notify, which is the only sanctioned
/// place: the events apply to the render call in flight and to no other.
/// Measured on macOS 15.6 against AUDynamicsProcessor — the readback lands
/// exactly on the scheduled value.
#[test]
fn a_scheduled_immediate_change_takes_effect() {
    let _g = lock();
    let mut au = DYNAMICS.open(RATE, BLOCK);

    let params = au.get_parameter_list();
    let p = params
        .first()
        .cloned()
        .expect("AUDynamicsProcessor publishes parameters");
    let target = p.range.min + (p.range.max - p.range.min) * 0.75;

    // Start somewhere else, so "took effect" is a change rather than a
    // coincidence.
    au.set_parameter(p.id, p.range.min).expect("prime");
    let before = au.get_parameter(p.id).expect("read back the primed value");

    let unit = au.render_unit();
    let status = Arc::new(AtomicI32::new(i32::MIN));
    let st = Arc::clone(&status);
    let notify = au
        .add_render_notify(move |n| {
            if n.phase != RenderPhase::Pre {
                return;
            }
            // SAFETY: `unit` is the unit currently rendering, so it is live.
            let r = unsafe {
                tutti_au_host::render_notify::schedule(
                    unit.get(),
                    p.id,
                    ScheduleAddress::GLOBAL,
                    &[ParamEvent::Immediate {
                        buffer_offset: 0,
                        value: target,
                    }],
                )
            };
            st.store(if r.is_ok() { 0 } else { -1 }, Ordering::SeqCst);
        })
        .unwrap();

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("render");
    drop(notify);

    assert_eq!(
        status.load(Ordering::SeqCst),
        0,
        "AudioUnitScheduleParameters must be accepted from the pre-render notify"
    );
    let after = au.get_parameter(p.id).expect("read back");
    assert!(
        (after - target).abs() < 0.01,
        "a scheduled immediate change must take effect: primed at {before}, \
         scheduled {target}, read back {after}"
    );
    assert!(all_finite(&output), "output must remain finite");
}

/// Scheduling many events in one call must be accepted.
///
/// The C API takes an array precisely so a host can hand over a block's worth of
/// automation in one FFI transition rather than N on the audio thread, and
/// [`schedule`] chunks rather than allocating. This drives more events than the
/// internal chunk size (16) so the chunking loop is exercised, not just its
/// first iteration.
#[test]
fn a_batch_of_scheduled_events_is_accepted() {
    let _g = lock();
    let mut au = DYNAMICS.open(RATE, BLOCK);

    let params = au.get_parameter_list();
    let p = params.first().cloned().expect("parameters");

    // 20 events > the 16-event chunk, so this crosses a chunk boundary.
    const N: u32 = 20;
    let events: Vec<ParamEvent> = (0..N)
        .map(|i| ParamEvent::Immediate {
            buffer_offset: i * (BLOCK / N),
            value: p.range.min + (p.range.max - p.range.min) * (i as f32 / N as f32),
        })
        .collect();

    let unit = au.render_unit();
    let status = Arc::new(AtomicI32::new(i32::MIN));
    let st = Arc::clone(&status);
    let notify = au
        .add_render_notify(move |n| {
            if n.phase != RenderPhase::Pre {
                return;
            }
            // SAFETY: the unit is currently rendering.
            let r = unsafe {
                tutti_au_host::render_notify::schedule(
                    unit.get(),
                    p.id,
                    ScheduleAddress::GLOBAL,
                    &events,
                )
            };
            st.store(if r.is_ok() { 0 } else { -1 }, Ordering::SeqCst);
        })
        .unwrap();

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("render");
    drop(notify);

    assert_eq!(
        status.load(Ordering::SeqCst),
        0,
        "a batch spanning more than one chunk must be accepted"
    );
    assert!(all_finite(&output), "output must remain finite");
}

/// `kAudioUnitParameterFlag_CanRamp` is a **claim**, not a guarantee — and
/// AUSpatialMixer is the proof.
///
/// It is the only Apple unit on macOS 15.6 that advertises the flag (10 of its 12
/// parameters), and it does not honour a ramp. This renders the same block twice
/// from freshly-opened units: once with a ramp scheduled across the parameter's
/// whole range, once with the parameter merely pinned at the ramp's start value.
/// A unit that ramps produces a rising envelope in the first case and a flat one
/// in the second; AUSpatialMixer produces **identical** envelopes.
///
/// Measured: `max|ramp − step|` is exactly `0.000000000` over 5 runs, while the
/// parameter readback *does* land on the ramp's end value — so the endpoint is
/// applied and the interpolation discarded. That is a step at the block
/// boundary, i.e. the zipper artifact ramping exists to remove.
///
/// This test exists so no host trusts `can_ramp` as a promise about audio. If a
/// future macOS makes AUSpatialMixer actually ramp, this fails and says so
/// rather than the claim quietly becoming true.
#[test]
fn the_can_ramp_flag_is_a_claim_not_a_guarantee() {
    let _g = lock();
    let (param_id, lo, hi) = SPATIAL_MIXER_RAMP_PARAM;

    // The flag really is advertised — otherwise this test is asserting nothing.
    {
        let au = SPATIAL_MIXER.open(RATE, BLOCK);
        let params = au.get_parameter_list();
        let p = params
            .iter()
            .find(|p| p.id == param_id)
            .expect("AUSpatialMixer must publish parameter 9 (global reverb gain)");
        assert!(
            p.can_ramp,
            "AUSpatialMixer parameter {param_id} is the corpus's CanRamp \
             advertiser; if it stopped advertising, this test's subject is gone"
        );
        assert_eq!(
            (p.range.min, p.range.max),
            (lo, hi),
            "the measured range is pinned so a changed parameter is caught"
        );
    }

    let ramped = render_with_schedule(
        &SPATIAL_MIXER,
        param_id,
        ParamEvent::Ramped {
            start_buffer_offset: 0,
            duration_frames: BLOCK,
            start_value: lo,
            end_value: hi,
        },
    );
    let stepped = render_with_schedule(
        &SPATIAL_MIXER,
        param_id,
        ParamEvent::Immediate {
            buffer_offset: 0,
            value: lo,
        },
    );

    let diff = ramped
        .iter()
        .zip(stepped.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);

    assert!(
        ramped.iter().all(|v| v.is_finite()) && stepped.iter().all(|v| v.is_finite()),
        "both envelopes must be finite — a NaN envelope would make the \
         difference below meaningless (f32::max returns the non-NaN operand)"
    );
    assert!(
        diff < 1.0e-6,
        "MEASURED BEHAVIOUR CHANGE: AUSpatialMixer advertises CanRamp on \
         parameter {param_id} but was measured NOT to honour it (envelopes \
         identical to 0.000000000 over 5 runs). It now differs by {diff}, which \
         means either the AU started ramping or this host started scheduling \
         differently. Both are worth knowing — re-measure before relaxing this.\n\
         ramp = {ramped:?}\nstep = {stepped:?}"
    );
}

/// A ramp **is** honoured where a plugin implements it — measured against
/// TAL Reverb 4.
///
/// The positive counterpart to the test above, and the only proof in this suite
/// that this host's ramp scheduling actually produces intra-block movement rather
/// than merely being accepted. Ramping `Dry` 0.0 → 1.0 across a 512-frame block
/// against a 0.5 DC input yields a strictly monotonic 8-segment envelope
/// `0.014483 → 0.104773`; pinning the parameter at the ramp's start gives a flat
/// `0.0`. Bit-identical across 10 repeats.
///
/// # Why absence is tolerated here
///
/// TAL Reverb 4 is a third-party plugin — a genuine optional install, unlike the
/// Apple units the corpus hard-requires because they ship with macOS. So this is
/// reached through `corpus::optional_third_party`.
///
/// That is **not** the silent skip `support/corpus.rs` warns about, because the
/// unconditional half still runs: the Apple negative result above is a separate
/// test that always executes, and when this unit is absent the notice below is
/// loud. What would be unacceptable is a suite where *every* ramp assertion
/// vanished with the plugin, leaving "ramping works" unexamined.
#[test]
fn a_ramp_is_honoured_where_a_plugin_implements_it() {
    let _g = lock();
    let (sub, mfr, param_id) = TAL_REVERB_4;

    let Some(info) = optional_third_party(sub, mfr, AuType::Effect) else {
        eprintln!(
            "NOTICE: TAL Reverb 4 ({}/{}) is not installed, so the POSITIVE ramp \
             leg is not exercised on this machine. The negative leg \
             (the_can_ramp_flag_is_a_claim_not_a_guarantee, against Apple's \
             AUSpatialMixer) still runs unconditionally. Install TAL Reverb 4 to \
             cover intra-block ramping.",
            String::from_utf8_lossy(sub),
            String::from_utf8_lossy(mfr),
        );
        return;
    };

    // Ramp 0 -> 1 across the block, versus pinned at 0.
    let ramped = render_info_with_schedule(
        &info,
        param_id,
        ParamEvent::Ramped {
            start_buffer_offset: 0,
            duration_frames: BLOCK,
            start_value: 0.0,
            end_value: 1.0,
        },
    );
    let stepped = render_info_with_schedule(
        &info,
        param_id,
        ParamEvent::Immediate {
            buffer_offset: 0,
            value: 0.0,
        },
    );

    assert!(
        ramped.iter().all(|v| v.is_finite()),
        "the ramp envelope must be finite; got {ramped:?}"
    );

    // The ramp must MOVE within the block. This is the whole claim.
    assert!(
        ramped.windows(2).all(|w| w[1] > w[0]),
        "a honoured ramp must rise monotonically across the block — measured \
         0.014483 -> 0.104773 in 8 segments on macOS 15.6. Got {ramped:?}"
    );

    // And it must differ from the step-at-start control, or the movement above
    // could be the plugin's own signal evolution rather than the ramp.
    let diff = ramped
        .iter()
        .zip(stepped.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        diff > 0.01,
        "the ramp must differ from pinning the parameter at its start value, or \
         the movement is the plugin's own and not the ramp's. Measured max \
         difference 0.104773; got {diff}.\nramp = {ramped:?}\nstep = {stepped:?}"
    );

    // Calibrated to the measured figures with generous headroom: the first
    // segment measured 0.014483 and the last 0.104773, reproducibly. The bounds
    // are wide because the point is the SHAPE (asserted above), not the exact
    // gain — a different build of the plugin may scale differently while still
    // ramping.
    assert!(
        ramped[0] < 0.05,
        "the ramp must start near its start value; measured 0.014483, got {}",
        ramped[0]
    );
    assert!(
        ramped[7] > ramped[0] * 2.0,
        "the ramp must travel materially across the block; measured 0.014483 -> \
         0.104773 (7.2x), got {} -> {}",
        ramped[0],
        ramped[7]
    );
}

/// The host must schedule a ramp that spans blocks by **re-scheduling it each
/// render**, and the result must stay finite and bounded.
///
/// Apple's header requires this: "the ramp is scheduled each audio unit render
/// for the duration of the ramp. Each schedule of the the new audio unit render
/// specifies the progress of the ramp." A host that schedules once gets one block
/// of movement and then a step — the artifact it was avoiding.
///
/// `start_buffer_offset` goes **negative** on every re-schedule after the first,
/// which is how a host says "this ramp began in an earlier block". That the field
/// is signed is load-bearing, and `render_notify`'s unit tests pin that it
/// survives the lowering; this test drives it against a real AU to confirm the AU
/// accepts it.
#[test]
fn a_multi_block_ramp_is_rescheduled_each_render() {
    let _g = lock();
    let mut au = DYNAMICS.open(RATE, BLOCK);
    let params = au.get_parameter_list();
    let p = params.first().cloned().expect("parameters");

    const BLOCKS: u32 = 4;
    let total = BLOCK * BLOCKS;
    let unit = au.render_unit();
    let elapsed = Arc::new(AtomicU32::new(0));
    let failures = Arc::new(AtomicU32::new(0));
    let e = Arc::clone(&elapsed);
    let f = Arc::clone(&failures);

    let notify = au
        .add_render_notify(move |n| {
            if n.phase != RenderPhase::Pre {
                return;
            }
            let done = e.load(Ordering::SeqCst);
            // Negative once the ramp began in an earlier block — the signed
            // field carrying the ramp's progress.
            let offset = -(done as i32);
            // SAFETY: the unit is currently rendering.
            let r = unsafe {
                tutti_au_host::render_notify::schedule(
                    unit.get(),
                    p.id,
                    ScheduleAddress::GLOBAL,
                    &[ParamEvent::Ramped {
                        start_buffer_offset: offset,
                        duration_frames: total,
                        start_value: p.range.min,
                        end_value: p.range.max,
                    }],
                )
            };
            if r.is_err() {
                f.fetch_add(1, Ordering::SeqCst);
            }
            e.store(done + n.frames, Ordering::SeqCst);
        })
        .unwrap();

    let input: Vec<Vec<f32>> = (0..2).map(|_| vec![0.25f32; BLOCK as usize]).collect();
    let mut output = silence(2, BLOCK as usize);
    for _ in 0..BLOCKS {
        render(&mut au, &input, &mut output, BLOCK).expect("render");
        assert!(
            all_finite(&output),
            "a ramped render must stay finite; a NaN here would also defeat the \
             peak check below, since f32::max returns the non-NaN operand"
        );
        assert!(
            peak(&output) < 8.0,
            "a ramped parameter must not make the output diverge; peak {}",
            peak(&output)
        );
    }
    drop(notify);

    assert_eq!(
        failures.load(Ordering::SeqCst),
        0,
        "every per-block re-schedule must be accepted, including the ones with a \
         negative start_buffer_offset"
    );
    assert_eq!(
        elapsed.load(Ordering::SeqCst),
        total,
        "the ramp must have been re-scheduled across all {BLOCKS} blocks"
    );
}

/// `AudioUnitScheduleParameters` does **not** validate the parameter id.
///
/// Measured on macOS 15.6: scheduling against id `999999` on AUDelay returns
/// `noErr`, as does a `Ramped` event on a parameter whose `CanRamp` flag is
/// clear. Pinned because it is a trap: a host cannot use the status to discover
/// whether an event will do anything, so anything built on "the AU rejected it,
/// therefore the id is bad" is built on sand.
///
/// If a future macOS starts validating, this fails and the host's docs — which
/// currently promise the opposite — need updating.
#[test]
fn scheduling_does_not_validate_the_parameter_id() {
    let _g = lock();
    let au = DELAY.open(RATE, BLOCK);

    let bogus = 999_999u32;
    assert!(
        !au.get_parameter_list().iter().any(|p| p.id == bogus),
        "sanity: {bogus} must not be a real AUDelay parameter"
    );

    let result = au.schedule_parameters(
        bogus,
        ScheduleAddress::GLOBAL,
        &[ParamEvent::Immediate {
            buffer_offset: 0,
            value: 0.5,
        }],
    );
    assert!(
        result.is_ok(),
        "MEASURED BEHAVIOUR CHANGE: AudioUnitScheduleParameters was measured to \
         accept a nonexistent parameter id ({bogus}) with noErr on macOS 15.6. It \
         now returns {result:?}. That is arguably better, but `schedule`'s docs \
         promise the opposite — update them."
    );

    // The same for a ramp on a parameter that does not advertise CanRamp.
    let params = au.get_parameter_list();
    let p = params.first().expect("AUDelay publishes parameters");
    assert!(
        !p.can_ramp,
        "AUDelay's first parameter is the corpus's non-rampable subject"
    );
    let ramp_result = au.schedule_parameters(
        p.id,
        ScheduleAddress::GLOBAL,
        &[ParamEvent::Ramped {
            start_buffer_offset: 0,
            duration_frames: BLOCK,
            start_value: p.range.min,
            end_value: p.range.max,
        }],
    );
    assert!(
        ramp_result.is_ok(),
        "a ramp on a non-rampable parameter was measured to return noErr; got \
         {ramp_result:?}"
    );
}

/// An empty event slice must be a no-op that never reaches AudioToolbox.
///
/// A host draining an empty automation queue does this every block on the audio
/// thread. Asserted through the public `AuInstance` path as well as the module's
/// own unit test, because this is the shape a caller actually uses.
#[test]
fn scheduling_an_empty_batch_is_a_no_op() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    au.schedule_parameters(0, ScheduleAddress::GLOBAL, &[])
        .expect("an empty schedule must succeed without an FFI call");

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("render");
    assert!(all_finite(&output));
}

// ---------------------------------------------------------------- helpers

/// Segments the envelope is measured in. 8 is enough to see a monotonic rise
/// while each segment (64 frames at BLOCK=512) still holds enough samples for a
/// stable peak.
const SEGMENTS: usize = 8;

/// Open `unit` fresh, schedule one event from the pre-render notify, render one
/// block of DC, and return the output envelope.
///
/// A fresh instance and a `reset` per call so the AU's own history cannot differ
/// between the ramp and step legs — without that, the comparison would be
/// confounded by whatever the previous render left in the plugin's delay lines.
fn render_with_schedule(unit: &support::corpus::AuRef, param: u32, event: ParamEvent) -> Vec<f32> {
    let mut au = unit.open(RATE, BLOCK);
    schedule_one_block(&mut au, param, event)
}

/// As [`render_with_schedule`], for a unit reached by [`optional_third_party`].
fn render_info_with_schedule(
    info: &tutti_au_host::AuComponentInfo,
    param: u32,
    event: ParamEvent,
) -> Vec<f32> {
    let mut au = open_info(info, RATE, BLOCK);
    schedule_one_block(&mut au, param, event)
}

/// Shared body: install a notify that schedules `event` on every pre-render,
/// render one block of 0.5 DC, and return the channel-0 envelope.
fn schedule_one_block(
    au: &mut tutti_au_host::AuInstance,
    param: u32,
    event: ParamEvent,
) -> Vec<f32> {
    let unit = au.render_unit();
    let notify = au
        .add_render_notify(move |n| {
            if n.phase != RenderPhase::Pre {
                return;
            }
            // SAFETY: the unit is currently rendering, so it is live.
            let _ = unsafe {
                tutti_au_host::render_notify::schedule(
                    unit.get(),
                    param,
                    ScheduleAddress::GLOBAL,
                    &[event],
                )
            };
        })
        .expect("the corpus units accept AudioUnitAddRenderNotify");

    // Flush any state the instantiation left, so both legs start identically.
    au.reset().expect("reset");

    // DC rather than silence: a gain-ish parameter's movement is only visible if
    // there is signal for it to act on.
    let input: Vec<Vec<f32>> = (0..2).map(|_| vec![0.5f32; BLOCK as usize]).collect();
    let mut output = silence(2, BLOCK as usize);
    render(au, &input, &mut output, BLOCK).expect("render");
    drop(notify);

    envelope(&output[0], SEGMENTS)
}
