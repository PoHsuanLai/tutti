//! Parameter notification and state reset — the host's *return* path.
//!
//! Every other suite in this crate asserts that the host can drive an AU. This
//! one asserts the two things that make a DAW's automation and transport
//! correct, both of which were entirely absent before it existed:
//!
//! - **Notification.** A listener registered on an AU actually receives
//!   `ParameterChanged` for host writes, and `BeginGesture`/`EndGesture` in
//!   order. Without the receive path a knob moved in the plugin's own editor is
//!   invisible to the host, so automation cannot be *recorded* from a plugin UI
//!   at all; without gestures, touch/latch automation cannot exist because
//!   nothing marks where a drag starts and stops.
//! - **`AudioUnitReset`.** Reverb tails, delay lines and filter memory are
//!   cleared on demand. Without it, audio from bar 60 smears across a jump to
//!   bar 1 and a loop wrap-around bleeds its end into its start.
//!
//! Two properties here are the kind that pass vacuously if written carelessly,
//! so each is defended explicitly:
//!
//! * A delivery test whose write does not change the value observes nothing and
//!   concludes "no events" — indistinguishable from a broken listener. Every
//!   test that expects an event first asserts it got one *before* drawing any
//!   conclusion from a later absence. This is not hypothetical: the probe that
//!   calibrated these numbers initially wrote `range.mid()` to AUDelay's
//!   "Dry/Wet Mix", whose midpoint *is* its default, and reported 0 events
//!   while the code was working perfectly.
//! * A "the tail got quieter" assertion passes on NaN, because `f32::max`
//!   returns the non-NaN operand and so `peak()` is NaN-blind. Every smallness
//!   assertion here is paired with `all_finite`.
//!
//! ## Measured numbers behind the thresholds
//!
//! Measured on macOS 15.6, 48 kHz / 512-frame blocks, driving 80 blocks of
//! ±0.9 square wave then rendering silence. Ten trials per unit, **bit-for-bit
//! identical every trial** (these AUs are deterministic):
//!
//! | unit | 1st silent block, no reset | 1st silent block, after reset | ratio |
//! |---|---|---|---|
//! | AUMatrixReverb | `1.277472` | `0.171456` | 0.1342 |
//! | AUReverb2 | `0.003877` | `0.000000` | 0.0 |
//!
//! AUMatrixReverb does not fall to exact zero because it re-injects into its
//! delay network at the start of the block following the reset; AUReverb2 does.
//! Both facts are asserted, so a regression in either direction is caught.
//!
//! Notification latency was measured at **~208 ms** for the first event, which
//! tracks `AuParameterListener`'s 200 ms notification interval. [`SETTLE`] is
//! set well above that with margin for scheduling.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_notification
//! ```
//!
//! No env vars, no SDK, no display. A missing unit **fails** rather than
//! skipping — see `support/corpus.rs`.

#![cfg(target_os = "macos")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod support;
use support::corpus::{all_finite, peak, render, silence, DELAY, MATRIX_REVERB, REVERB2};

use tutti_au_host::{
    emit_gesture, notify_all_parameters, AuEvent, AuParameterListener, EventAddress,
};

/// AudioToolbox tolerates concurrent use of *distinct* units, but component
/// discovery walks a process-global registry and several tests here open the
/// same unit. Serializing keeps one test's instantiate/dispose from racing
/// another's enumeration. Mirrors `AU_LOCK` in `au_conformance.rs`.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// `AU_LOCK` is poisoned by any panicking test, and a poisoned lock would
/// convert one real failure into N spurious ones. The guard is only a
/// serializer — there is no shared state to be left inconsistent — so recover.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// How long to wait for a notification before calling it absent.
///
/// The listener's notification interval is 200 ms and the first event was
/// measured arriving at ~208 ms, so this is ~14x the observed latency. Generous
/// on purpose: the cost of being too generous is a slow *passing* test, while
/// the cost of being too tight is a flaky failure on a loaded machine.
const SETTLE: Duration = Duration::from_secs(3);

/// How long to wait before concluding that *no further* events will arrive.
///
/// Used only after a listener is dropped. Must comfortably exceed one full
/// notification interval so an in-flight event has time to land and be counted;
/// otherwise "nothing arrived" would just mean "we did not wait long enough".
const QUIESCE: Duration = Duration::from_millis(900);

/// Collects events off the dispatch queue the listener delivers on.
///
/// A plain `Vec` behind a `Mutex` rather than a channel: the assertions need to
/// inspect the whole ordered history repeatedly (for the gesture ordering test),
/// not consume it once.
#[derive(Clone, Default)]
struct EventLog(Arc<Mutex<Vec<AuEvent>>>);

impl EventLog {
    fn sink(&self) -> impl Fn(AuEvent) + Send + Sync + 'static {
        let inner = Arc::clone(&self.0);
        move |ev| inner.lock().unwrap_or_else(|e| e.into_inner()).push(ev)
    }

    fn snapshot(&self) -> Vec<AuEvent> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Block until at least `n` events have arrived, or `SETTLE` elapses.
    /// Returns whether the count was reached.
    fn wait_for_at_least(&self, n: usize) -> bool {
        let start = Instant::now();
        while start.elapsed() < SETTLE {
            if self.len() >= n {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.len() >= n
    }
}

// ------------------------------------------------------------- notification

/// The core of the feature: a parameter write must reach a registered listener,
/// carrying the id and the *new value*.
///
/// The value matters as much as the delivery. AudioToolbox hands the listener
/// the value directly, and a host that had to read it back could observe a
/// different one (the parameter may have moved again in between) — so this
/// asserts the value in the event, not the value in the AU.
#[test]
fn a_listener_observes_a_parameter_written_by_the_host() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    let params = au.get_parameter_list();
    assert!(!params.is_empty(), "AUDelay publishes parameters");
    let p = params[0].clone();

    let log = EventLog::default();
    // SAFETY: `au` outlives `listener` — `listener` is dropped at the end of
    // this scope, before `au`.
    let listener = unsafe { AuParameterListener::new(au.raw_unit(), log.sink()) }
        .expect("AUEventListenerCreateWithDispatchQueue");
    listener
        .watch_parameter(p.id, EventAddress::GLOBAL)
        .expect("watch_parameter on a parameter the AU declared");

    // Write a value that genuinely DIFFERS from the current one. Writing the
    // value already in place produces no change and therefore no event, and the
    // test would then "pass" its later assertions having proven nothing. See the
    // module docs — this exact mistake produced a silent false negative.
    let current = au.get_parameter(p.id).expect("read the current value");
    let target = if (current - p.range.max).abs() > (p.range.max - p.range.min) * 0.1 {
        p.range.max
    } else {
        p.range.min
    };
    au.set_parameter(p.id, target).expect("set_parameter");

    assert!(
        log.wait_for_at_least(1),
        "no event within {SETTLE:?} for a write of {target} to parameter {} \
         (current was {current}) — the listener never fired",
        p.id
    );

    let events = log.snapshot();
    let found = events.iter().any(|ev| {
        matches!(
            ev,
            AuEvent::ParameterChanged { id, value, .. }
                if *id == p.id && (*value - target).abs() < 0.01
        )
    });
    assert!(
        found,
        "expected ParameterChanged {{ id: {}, value: ~{target} }}, got {events:?}",
        p.id
    );
}

/// Gesture begin and end must both be delivered, and **in that order**.
///
/// Order is the whole point. A host records an automation segment from begin to
/// end; receiving them transposed would close a segment before opening it, and
/// receiving only one leaves a segment open forever. `AUEventListener` documents
/// that events are "delivered serially to the listener, preserving the time
/// order", and this pins that we actually get that guarantee.
///
/// Gestures are emitted here by the host via `emit_gesture`, because nothing
/// else can make one happen: gestures originate in a plugin's own editor, which
/// a test cannot drive. That still exercises the whole receive path —
/// registration, the dispatch-queue delivery, the tag decode — which is the part
/// this crate owns.
#[test]
fn gesture_begin_and_end_are_delivered_in_order() {
    let _g = lock();
    let au = DELAY.open(RATE, BLOCK);
    let params = au.get_parameter_list();
    let p = params[0].clone();

    let log = EventLog::default();
    // SAFETY: `au` outlives `listener`.
    let listener =
        unsafe { AuParameterListener::new(au.raw_unit(), log.sink()) }.expect("create listener");
    listener
        .watch_gestures(p.id, EventAddress::GLOBAL)
        .expect("watch_gestures");

    // SAFETY: `au.raw_unit()` is live.
    unsafe {
        emit_gesture(au.raw_unit(), p.id, EventAddress::GLOBAL, true).expect("emit begin");
    }
    // Separate the two so a coalescing window cannot merge them. The listener's
    // value-change granularity is 10 ms; gestures are not value changes, but
    // spacing them removes any doubt about what an ordering failure would mean.
    std::thread::sleep(Duration::from_millis(50));
    // SAFETY: as above.
    unsafe {
        emit_gesture(au.raw_unit(), p.id, EventAddress::GLOBAL, false).expect("emit end");
    }

    assert!(
        log.wait_for_at_least(2),
        "expected both gesture events within {SETTLE:?}, saw {:?}",
        log.snapshot()
    );

    let events = log.snapshot();
    let begin_at = events
        .iter()
        .position(|ev| matches!(ev, AuEvent::BeginGesture { id, .. } if *id == p.id))
        .unwrap_or_else(|| panic!("no BeginGesture for parameter {} in {events:?}", p.id));
    let end_at = events
        .iter()
        .position(|ev| matches!(ev, AuEvent::EndGesture { id, .. } if *id == p.id))
        .unwrap_or_else(|| panic!("no EndGesture for parameter {} in {events:?}", p.id));
    assert!(
        begin_at < end_at,
        "gestures arrived out of order (begin at {begin_at}, end at {end_at}): {events:?}"
    );
}

/// Dropping the listener must stop delivery — the use-after-free guard.
///
/// `AuParameterListener` owns a heap block wrapping the host's closure, and
/// AudioToolbox holds a registration pointing at it. If `Drop` released the
/// dispatch queue or freed the block *before* `AUListenerDispose`, the next
/// parameter change on that AU would call a freed closure. That is a
/// use-after-free reachable from the plugin's own UI at any time, so it cannot
/// be left to inspection.
///
/// The test is only meaningful if delivery was working beforehand, so it asserts
/// a positive count *first* and only then that the count stops advancing. Under
/// a sanitizer the freed-closure call would trap outright; without one, the
/// frozen count is the observable.
#[test]
fn a_dropped_listener_stops_delivering() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    let params = au.get_parameter_list();
    let p = params[0].clone();

    let log = EventLog::default();
    // SAFETY: `au` outlives `listener`, which is dropped explicitly below.
    let listener =
        unsafe { AuParameterListener::new(au.raw_unit(), log.sink()) }.expect("create listener");
    listener
        .watch_parameter(p.id, EventAddress::GLOBAL)
        .expect("watch_parameter");

    // Establish that delivery WORKS before asserting that it stops. Without
    // this, a listener that never fired at all would sail through.
    let current = au.get_parameter(p.id).expect("read current");
    let target = if (current - p.range.max).abs() > (p.range.max - p.range.min) * 0.1 {
        p.range.max
    } else {
        p.range.min
    };
    au.set_parameter(p.id, target).expect("set before drop");
    assert!(
        log.wait_for_at_least(1),
        "delivery was never working, so 'it stopped' would prove nothing"
    );

    drop(listener);
    // Let anything already in flight land, so it is counted in the baseline
    // rather than mistaken for a post-drop delivery.
    std::thread::sleep(QUIESCE);
    let baseline = log.len();

    // Now hammer the parameter. Spaced past the 10 ms coalescing granularity so
    // each write is a separately notifiable change: if the registration were
    // still live, these would land.
    for i in 0..20 {
        let v = p.range.min + (p.range.max - p.range.min) * (i as f32 / 20.0);
        au.set_parameter(p.id, v).expect("set after drop");
        std::thread::sleep(Duration::from_millis(15));
    }
    std::thread::sleep(QUIESCE);

    assert_eq!(
        log.len(),
        baseline,
        "a dropped listener delivered {} further event(s) — the registration \
         outlived the handle, which means AUListenerDispose did not run before \
         the block was freed: {:?}",
        log.len() - baseline,
        log.snapshot()
    );
}

/// Restoring state must notify listeners, or every open plugin editor keeps
/// showing the values from before the load.
///
/// `kAudioUnitProperty_ClassInfo` rewrites the AU's whole parameter set
/// internally without issuing a single `AUParameterSet`, so no listener hears
/// about any of it. Apple's `ClassInfo` documentation mandates the compensating
/// `AUParameterListenerNotify(kAUParameterListener_AnyParameter)` call for
/// exactly this reason, and `AuInstance::set_state` makes it.
///
/// Measured: AUDelay's 183-byte state blob produces notifications for all **4**
/// of its parameters, first arriving at ~201 ms.
#[test]
fn load_state_notifies_listeners() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    let params = au.get_parameter_list();
    let p = params[0].clone();

    // Capture state at one value, then move the parameter somewhere else, so the
    // restore has something real to put back.
    au.set_parameter(p.id, p.range.min).expect("set to min");
    let state = au.get_state().expect("get_state");
    assert!(!state.is_empty(), "AUDelay produces a non-empty state blob");
    au.set_parameter(p.id, p.range.max).expect("set to max");

    let log = EventLog::default();
    // SAFETY: `au` outlives `listener`.
    let listener =
        unsafe { AuParameterListener::new(au.raw_unit(), log.sink()) }.expect("create listener");
    // The wildcard notify covers every parameter, so watch every parameter —
    // registration itself has no wildcard (see `notify_all_parameters`).
    for q in &params {
        listener
            .watch_parameter(q.id, EventAddress::GLOBAL)
            .expect("watch_parameter");
    }
    // Discard anything the two writes above generated; only the restore counts.
    log.clear();

    au.set_state(&state).expect("set_state");

    assert!(
        log.wait_for_at_least(1),
        "set_state produced no notification within {SETTLE:?} — open plugin \
         editors would still be showing the pre-load values"
    );

    let events = log.snapshot();
    let restored = events.iter().any(|ev| {
        matches!(
            ev,
            AuEvent::ParameterChanged { id, value, .. }
                if *id == p.id && (*value - p.range.min).abs() < 0.01
        )
    });
    assert!(
        restored,
        "expected a ParameterChanged carrying the restored value {} for \
         parameter {}, got {events:?}",
        p.range.min, p.id
    );

    // And the AU itself really is back at the saved value — the notify must
    // describe a real restore, not fire on its own.
    let after = au.get_parameter(p.id).expect("read after restore");
    assert!(
        (after - p.range.min).abs() < 0.01,
        "set_state notified but the value is {after}, not the saved {}",
        p.range.min
    );
}

/// The wildcard notify must be accepted even with no listener registered.
///
/// `set_state` calls it unconditionally, and the overwhelmingly common case is
/// a host with no editor open and therefore no listener. If AudioToolbox
/// rejected the call in that state, every project load would be doing something
/// that errors — currently ignored, but it would be masking a real refusal.
#[test]
fn notify_all_parameters_is_accepted_with_no_listener() {
    let _g = lock();
    let au = DELAY.open(RATE, BLOCK);
    // SAFETY: `au.raw_unit()` is live.
    unsafe { notify_all_parameters(au.raw_unit()) }
        .expect("the wildcard notify is legal with no listeners registered");
}

// -------------------------------------------------------------------- reset

/// Drive a unit to a loud steady state, then measure the first silent block
/// with and without an intervening `reset`.
///
/// Returns `(no_reset_peak, reset_peak)`. Two fresh instances rather than one
/// re-driven instance, so the "after reset" measurement cannot inherit any
/// history the no-reset leg left behind.
fn measure_tail(unit: &support::corpus::AuRef) -> (f32, f32) {
    let n = BLOCK as usize;
    // A ±0.9 square wave: broadband, loud, and deterministic. Deterministic
    // matters — the thresholds below are calibrated to exact measured values.
    let loud: Vec<Vec<f32>> = (0..2)
        .map(|_| {
            (0..n)
                .map(|i| if (i / 16) % 2 == 0 { 0.9 } else { -0.9 })
                .collect()
        })
        .collect();
    let quiet = silence(2, n);

    let measure = |do_reset: bool| {
        let mut au = unit.open(RATE, BLOCK);
        let mut out = silence(2, n);
        // 80 blocks ~= 0.85 s at 48 kHz, long enough for these reverbs to reach
        // a steady state.
        for _ in 0..80 {
            render(&mut au, &loud, &mut out, BLOCK).expect("render loud");
        }
        if do_reset {
            au.reset().expect("reset");
        }
        render(&mut au, &quiet, &mut out, BLOCK).expect("render silence");
        assert!(
            all_finite(&out),
            "{} produced a non-finite sample; peak() is NaN-blind so every \
             smallness assertion below would pass vacuously",
            unit.label
        );
        peak(&out)
    };

    let no_reset = measure(false);
    let after_reset = measure(true);
    (no_reset, after_reset)
}

/// `reset` must actually clear a reverb tail.
///
/// This is the audible payoff: without it, jumping the playhead smears the
/// previous position's reverb across the new one.
///
/// Thresholds come from measurement, not from taste. Measured on macOS 15.6 over
/// 10 trials per unit, **bit-for-bit identical every trial**:
///
/// - AUMatrixReverb: no-reset `1.277472`, after reset `0.171456` (ratio 0.1342)
/// - AUReverb2: no-reset `0.003877`, after reset exactly `0.000000`
///
/// The assertion is `reset <= no_reset * 0.5`, roughly 3.7x looser than the
/// worst measured ratio, so ordinary AU revisions do not break it while a reset
/// that silently did nothing (ratio ~1.0) fails immediately.
///
/// Every smallness check is paired with a finiteness check, because `peak()`
/// builds on `f32::max`, which returns the non-NaN operand — so a NaN-filled
/// buffer reports a *small* peak and would pass. `measure_tail` asserts
/// finiteness on every block it measures.
#[test]
fn reset_clears_a_reverb_tail() {
    let _g = lock();
    for unit in [MATRIX_REVERB, REVERB2] {
        let (no_reset, after_reset) = measure_tail(&unit);

        // The premise: without a reset there IS a tail to clear. If this fails
        // the unit no longer rings and the rest of the test is vacuous.
        assert!(
            no_reset > 1e-4,
            "{}: no measurable tail without reset ({no_reset:e}); the test \
             subject stopped ringing, so 'reset made it quieter' proves nothing",
            unit.label
        );
        assert!(
            after_reset.is_finite(),
            "{}: post-reset peak is not finite ({after_reset}) — peak() is \
             NaN-blind, so the comparison below would pass on garbage",
            unit.label
        );
        assert!(
            after_reset <= no_reset * 0.5,
            "{}: reset did not clear the tail — first silent block was \
             {after_reset:.6} after reset vs {no_reset:.6} without it \
             (measured 0.171456 vs 1.277472 for AUMatrixReverb, 0.0 vs \
             0.003877 for AUReverb2)",
            unit.label
        );
    }
}

/// AUReverb2 falls to *exactly* zero after a reset, and AUMatrixReverb does not.
///
/// Pinned separately from the ratio test above because the two facts are
/// different claims and the looser ratio hides both. AUMatrixReverb re-injects
/// into its delay network on the block following the reset, so its first silent
/// block is small but non-zero; AUReverb2 emits true silence. A change in either
/// direction — AUReverb2 growing a residual, or AUMatrixReverb's residual
/// ballooning — means the reset semantics moved and the ratio threshold above
/// needs re-measuring rather than relaxing.
#[test]
fn the_reset_residual_is_zero_for_reverb2_and_small_for_matrix_reverb() {
    let _g = lock();

    let (_, reverb2_after) = measure_tail(&REVERB2);
    assert_eq!(
        reverb2_after, 0.0,
        "AUReverb2 measured exactly 0.0 for the first silent block after a \
         reset over 10 trials; it now reports {reverb2_after:e}"
    );

    let (_, matrix_after) = measure_tail(&MATRIX_REVERB);
    assert!(
        matrix_after.is_finite(),
        "AUMatrixReverb post-reset peak must be finite, got {matrix_after}"
    );
    // Measured 0.171456 exactly, 10/10 trials. Bounded above with ~2x headroom
    // and below at zero-exclusive, so the "re-injects on the next block"
    // behaviour is pinned as a real, non-zero residual.
    assert!(
        matrix_after > 0.0 && matrix_after < 0.35,
        "AUMatrixReverb's post-reset residual measured 0.171456 (10/10 trials) \
         — a small but deliberately non-zero re-injection. Got {matrix_after:.6}"
    );
}

/// `reset` must not disturb parameter values.
///
/// The distinction is the method's whole contract: it clears signal *history*,
/// not the patch. A reset that also reverted parameters would silently undo the
/// user's automation on every playhead jump.
#[test]
fn reset_leaves_parameters_untouched() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    let params = au.get_parameter_list();
    assert!(!params.is_empty());

    // Move every writable parameter off its default to a distinctive value, so a
    // reset-to-defaults would be visible.
    let mut expected = Vec::new();
    for p in &params {
        if !p.writable {
            continue;
        }
        let v = p.range.min + (p.range.max - p.range.min) * 0.3;
        au.set_parameter(p.id, v).expect("set parameter");
        let readback = au.get_parameter(p.id).expect("read back");
        expected.push((p.id, readback, p.name.clone()));
    }
    assert!(
        !expected.is_empty(),
        "AUDelay must have at least one writable parameter for this to test \
         anything"
    );

    au.reset().expect("reset");

    for (id, before, name) in expected {
        let after = au.get_parameter(id).expect("read after reset");
        assert!(
            (after - before).abs() < 0.001,
            "reset changed parameter {id} ({name}) from {before} to {after} — \
             reset must clear signal history, not the patch"
        );
    }
}

/// `reset` is accepted in the `Loaded` (pre-init) state, and the AU stays
/// usable afterwards.
///
/// This pins the documented decision on `AuInstance::reset`. Measured on macOS
/// 15.6: **all nine** corpus units — AUDelay, AUMatrixReverb,
/// AUDynamicsProcessor, AUNBandEQ, AULowpass, AUReverb2, AUDistortion, AUSampler
/// and DLSMusicDevice — return `noErr` for `AudioUnitReset` before
/// `AudioUnitInitialize`, and every one initializes and renders normally after.
///
/// So `reset` deliberately does *not* gate on the typestate the way `process`
/// does: a host wiring up a channel strip should not have to know which plugins
/// are initialized yet. There is nothing to flush pre-init, so the call is
/// simply inert. If a future AU refuses, this fails rather than the documented
/// behaviour drifting silently.
#[test]
fn reset_is_accepted_before_initialize() {
    let _g = lock();
    // Deliberately spans effects and instruments: instruments have no input bus
    // and take a different path through `initialize`, so a typestate assumption
    // that held only for effects would show up here.
    for unit in [DELAY, MATRIX_REVERB, REVERB2] {
        let mut au = unit.open_uninitialized(RATE, BLOCK);
        assert!(!au.is_initialized());

        au.reset().unwrap_or_else(|e| {
            panic!(
                "{}: reset in the Loaded state failed with {e:?}. All nine \
                 corpus units accepted it when measured on macOS 15.6; if this \
                 AU now refuses, `AuInstance::reset`'s documented pre-init \
                 contract needs revisiting, not this assertion relaxing.",
                unit.label
            )
        });

        // And the instance must still be fully usable: a pre-init reset that
        // left the AU wedged would be worse than one that errored.
        au.initialize()
            .unwrap_or_else(|e| panic!("{}: initialize after a pre-init reset: {e:?}", unit.label));
        let input = silence(2, BLOCK as usize);
        let mut output = silence(2, BLOCK as usize);
        render(&mut au, &input, &mut output, BLOCK)
            .unwrap_or_else(|e| panic!("{}: render after a pre-init reset: {e:?}", unit.label));
        assert!(all_finite(&output), "{}: non-finite output", unit.label);
    }
}

/// `reset` must be idempotent and safe to call repeatedly on a live unit.
///
/// A host calls it on every transport discontinuity, which during scrubbing can
/// mean many times per second, sometimes twice for one seek. A second reset
/// finding nothing to clear must be a no-op, not an error.
#[test]
fn repeated_resets_are_harmless() {
    let _g = lock();
    let mut au = MATRIX_REVERB.open(RATE, BLOCK);
    let n = BLOCK as usize;
    let input = silence(2, n);
    let mut output = silence(2, n);

    for round in 0..5 {
        au.reset()
            .unwrap_or_else(|e| panic!("reset #{round} failed: {e:?}"));
        au.reset()
            .unwrap_or_else(|e| panic!("back-to-back reset #{round} failed: {e:?}"));
        render(&mut au, &input, &mut output, BLOCK)
            .unwrap_or_else(|e| panic!("render after reset #{round}: {e:?}"));
        assert!(
            all_finite(&output),
            "non-finite output after reset round {round}"
        );
    }
}
