//! Gaps a mutation audit of this crate's suites found, and the assertions that
//! close them.
//!
//! Every test here was produced the same way: a plausible bug was introduced
//! into `src/`, the whole suite was run, and **nothing failed**. The tests below
//! are the ones that now do. Each doc comment names the mutation it kills, so a
//! future refactor that reintroduces it fails here rather than shipping.
//!
//! ## What is deliberately *not* here
//!
//! Six survivors were measured to be unfalsifiable on this machine, and a test
//! that cannot fail is worse than no test. Four are unfalsifiable because the
//! installed AUs never exhibit the distinguishing behaviour; two are
//! unfalsifiable *in kind*, being races that no sampling schedule can exhaust —
//! the same reasoning the repo's `RtPublish` rule applies to no-alloc tests.
//!
//! Absent behaviour in the corpus:
//!
//! * **`has_input` inferred from the stream-format read** instead of the input
//!   element count. Probed across every instantiable component: **54 units, 0
//!   disagreements** between the two inferences. Both answers are identical
//!   here, so any test would be asserting a tautology. The element count is
//!   still the right source — it is the AU's direct answer to "is there an input
//!   bus", while a format read conflates "no bus" with "declines to say" — but
//!   that argument is a design one, not a testable one on this corpus.
//!
//! * **A non-fatal sample-rate read-back.** The `?` in `StreamConfig::apply`
//!   only fires for an AU that keeps a rate other than the one requested.
//!   Probed at 8 k, 22.05 k, 44.1 k, 96 k, 192 k, 1 MHz and **7 Hz** across 12
//!   units: every unit echoed every rate back verbatim. The guard is unreachable
//!   without a probe AU that lies about its rate.
//!
//! * **Ignoring `OutputIsSilence`.** Honouring the flag only changes the output
//!   on a block where the AU raises it *and* the host's scratch holds stale
//!   non-silence. Probed across 9 Apple effects, 200 blocks of silence each
//!   after a loud block: **not one unit ever raised the flag**, and every unit's
//!   tail decayed to exactly 0.0 regardless. (`au_render_notify.rs` carries a
//!   comment asserting AUDelay sets it on a silent input; that was not
//!   reproducible here.) Falsifying this needs a probe AU that sets the flag
//!   while writing garbage — a `Misbehaviour` variant, not a corpus unit.
//!
//! * **`RenderScratch::new`'s `in_ch.max(out_ch)` narrowed to `in_ch`.** This is
//!   recorded because the over-allocation is *documented* as the guard against
//!   an instrument out-of-bounds, and it is not what actually guards that.
//!   `render_input`
//!   bounds every write by the AU's own `mDataByteSize` and resolves a channel
//!   past the end of `scratch.inputs` through `.get(ch)` with
//!   `None => dst.fill(0.0)`, so a short `inputs` vec is memory-safe — the guard
//!   in the callback supersedes the over-allocation. What remains is a
//!   silence-vs-signal difference on a channel an AU pulls beyond its declared
//!   input width, and no installed unit does that: every corpus effect is 2-in,
//!   the instruments are 0-in with `has_input == false`, and AUSpatialMixer
//!   (1-in / 2-out, the one unit where the two counts differ) already renders
//!   silence for channel 1 with the over-allocation in place — measured. So the
//!   line is dead weight rather than a fix, and its comment overstates it.
//!
//! Races, which are unfalsifiable in kind rather than for want of a plugin:
//!
//! * **`TransportState`'s locate `Release`/`Acquire` pair downgraded to
//!   `Relaxed`.** `the_locate_flag_is_consumed_exactly_once` covers the `swap`'s
//!   atomicity, but the pairing exists so an AU that observes `state_changed`
//!   also sees the *position stores that preceded it*. That is a cross-thread
//!   visibility guarantee: on arm64 the reordering it forbids is permitted by
//!   the model yet vanishingly rare in practice, and a test that samples for it
//!   would pass under the bug almost always. The guarantee belongs in the
//!   ordering annotation, not in a flaky assertion.
//!
//! * **The listener's private serial dispatch queue made concurrent.** Probed
//!   directly: 40 begin/end gesture pairs emitted back-to-back with no spacing,
//!   under `_dispatch_queue_attr_concurrent`, across 6 runs — **80/80 events
//!   delivered in perfect alternation every time, 0 violations**. AudioToolbox
//!   serializes its own delivery upstream of the queue, so the concurrent
//!   attribute does not manifest here. (Note the existing
//!   `gesture_begin_and_end_are_delivered_in_order` sleeps 50 ms between its two
//!   events to defeat coalescing, which also guarantees only one is ever in
//!   flight — so it cannot observe reordering by construction.)

#![cfg(target_os = "macos")]

mod support;
use support::corpus;

use std::sync::Mutex;

use tutti_au_host::bus::BusDirection;
use tutti_au_host::offline::PushScratch;
use tutti_au_host::AuError;
use tutti_plugin_types::ChannelLayout;

/// Serializes AU instantiation, as the other suites' `AU_LOCK`s do and for the
/// same reason: component discovery walks a process-global registry.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// Poison is recovered rather than propagated, so one panicking test does not
/// turn every later one into a spurious `PoisonError`.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// `process_push` must refuse a scratch with no **input** bus, not only one with
/// no output bus.
///
/// `AudioUnitProcess` is the single-list push call: it renders *through* one
/// buffer list, so the host has to supply an input bus for the AU to read and
/// overwrite. `offline::process_push` reaches straight for `input_slabs[0]`
/// immediately after the guard, so dropping the `input_audio.is_empty()` half of
/// it indexes an empty `Vec`.
///
/// The reachable shape, and why this is not a synthetic worry: an **instrument**
/// has zero input buses. Measured on macOS 15.6, AUSampler reports 0 in / 1 out
/// and DLSMusicDevice 0 in / 2 out. A host that sizes its `PushScratch` honestly
/// — from [`AuInstance::bus_count`], which is exactly what `PushScratch::new`'s
/// docs instruct — therefore hands `process_push` a zero-input scratch for every
/// instrument in the session. Without the guard that is a panic on the export
/// path, and an export panic loses the user's bounce.
///
/// Asserted as the typed [`AuError::InvalidBuffer`] rather than a bare `is_err`,
/// because "the host refused before touching the AU" and "the AU answered
/// `unimpErr`" are different facts and only the first is this guard's doing.
#[test]
fn push_refuses_a_scratch_with_no_input_bus() {
    let _g = lock();

    // Instruments: zero input buses, so the guard is what stands between the
    // caller and an empty-Vec index.
    for unit in [corpus::SAMPLER, corpus::DLS_SYNTH] {
        let mut au = unit.open(RATE, BLOCK);
        let in_buses = au.bus_count(BusDirection::Input);
        let out_buses = au.bus_count(BusDirection::Output);
        assert_eq!(
            in_buses, 0,
            "{}: this test needs a unit with no input bus; it now reports \
             {in_buses}, so the zero-input path it guards is no longer exercised",
            unit.label
        );
        assert!(
            out_buses > 0,
            "{}: needs at least one output bus to isolate the input half of the \
             guard",
            unit.label
        );

        // Sized from the AU's own bus counts, the way `PushScratch::new` says to.
        let ins: Vec<ChannelLayout> = (0..in_buses).map(|_| ChannelLayout::STEREO).collect();
        let outs: Vec<ChannelLayout> = (0..out_buses).map(|_| ChannelLayout::STEREO).collect();
        let mut scratch = PushScratch::new(&ins, &outs, BLOCK);

        match au.process_push(&mut scratch, BLOCK) {
            Err(AuError::InvalidBuffer(msg)) => assert!(
                msg.contains("input bus"),
                "{}: the refusal should name the missing input bus; got {msg:?}",
                unit.label
            ),
            other => panic!(
                "{}: a scratch with zero input buses must be refused with \
                 InvalidBuffer — `process_push` indexes `input_slabs[0]` right \
                 after the guard, so admitting it panics on an empty Vec. \
                 Got {other:?}",
                unit.label
            ),
        }
    }

    // The control: the same call on a unit that DOES have an input bus must get
    // through the guard, so the assertions above are not passing because
    // `process_push` refuses everything.
    let mut au = corpus::DELAY.open(RATE, BLOCK);
    let ins = vec![ChannelLayout::STEREO; au.bus_count(BusDirection::Input) as usize];
    let outs = vec![ChannelLayout::STEREO; au.bus_count(BusDirection::Output) as usize];
    assert!(
        !ins.is_empty(),
        "AUDelay must have an input bus for this control to mean anything"
    );
    let mut scratch = PushScratch::new(&ins, &outs, BLOCK);
    let r = au.process_push(&mut scratch, BLOCK);
    assert!(
        !matches!(&r, Err(AuError::InvalidBuffer(m)) if m.contains("input bus")),
        "AUDelay has an input bus, so the missing-input-bus refusal must not \
         fire for it; got {r:?}"
    );
}

/// The two `has_input` inferences agree on **every** installed component, which
/// is why no test asserts one against the other.
///
/// `StreamConfig::probe` derives `has_input` from the AU's input *element
/// count*. A refactor deriving it from whether the bus-0 stream-format read
/// succeeded is a survivor of this crate's suites — and provably cannot be
/// caught here, because the two answers coincide on all 54 instantiable units.
///
/// This test pins that *measurement* rather than the behaviour: it fails if the
/// machine ever grows an AU where the two disagree, at which point a real
/// assertion becomes possible and the survivor becomes closeable. It deliberately
/// does not assert which inference is used — that would be the tautology this
/// file's header explains.
#[test]
fn the_two_has_input_inferences_are_indistinguishable_on_this_machine() {
    use tutti_au_host::component::{enumerate_components_of_type, AuType};
    use tutti_au_host::instance::AuInstance;

    let _g = lock();
    let mut checked = 0;
    let mut disagree = Vec::new();

    for ty in [
        AuType::Effect,
        AuType::Instrument,
        AuType::Mixer,
        AuType::Generator,
        AuType::MusicEffect,
        AuType::Converter,
        AuType::Output,
        AuType::MidiProcessor,
    ] {
        for info in enumerate_components_of_type(ty) {
            // A unit that will not instantiate says nothing either way; several
            // output units refuse without hardware.
            // SAFETY: `component` came from `AudioComponentFindNext` via
            // `enumerate_components_of_type`, so it is a live factory handle.
            let Ok(au) = (unsafe { AuInstance::new(info.component, RATE, BLOCK) }) else {
                continue;
            };
            let by_element_count = au.bus_count(BusDirection::Input) > 0;
            let by_format_read = au.bus_layout(BusDirection::Input, 0).is_ok();
            checked += 1;
            if by_element_count != by_format_read {
                disagree.push(format!(
                    "{}: element_count={by_element_count} format_read={by_format_read}",
                    info.name
                ));
            }
        }
    }

    assert!(
        checked >= 40,
        "only {checked} components instantiated; this census needs the bulk of \
         the system's AUs to support the claim that the two inferences agree"
    );
    assert!(
        disagree.is_empty(),
        "the two `has_input` inferences now disagree on {} of {checked} \
         components, so the element-count-vs-format-read survivor documented in \
         this file's header has become falsifiable — write the real assertion:\n  {}",
        disagree.len(),
        disagree.join("\n  "),
    );
}
