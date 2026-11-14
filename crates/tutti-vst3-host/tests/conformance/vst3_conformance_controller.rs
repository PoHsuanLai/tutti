//! VST3 host conformance: Controller-side surfaces (`IEditController` and its extensions rather than the audio path).
//!
//! The harness (HostCheck's C ABI, the plugin lock, discovery, `drive_block`) is
//! `support/conformance.rs`, which also carries the suite's overview.

#![cfg(feature = "conformance")]

use crate::conformance_support;

use tutti_fixtures::main_thread;
use tutti_graph::Offset;
use tutti_types::Samples;
use tutti_ump::{CCNumber, MidiChannel};
use tutti_vst3_host::{AudioBuffer, InputEvents, PrefetchSupport, TransportInfo};

/// A block longer than any offset these tests place an event at.
const TEST_BLOCK: usize = 1 << 16;

/// The offset `k` frames into a test block.
fn at(k: usize) -> Offset {
    Offset::of(Samples(k), Samples(TEST_BLOCK))
}

use conformance_support::*;
use tutti_fixtures::midi;

// ── Controller-side surfaces ─────────────────────────────────────────────────
//
// The tests below drive `IEditController` and its extensions rather than the
// audio path. The twin's controller records the thread it was initialized on
// and aborts a main-thread call made from any other, so "the main thread" is
// whichever thread loaded the plugin, and doing load and every controller call
// on one thread, as a `#[test]` naturally does, satisfies it.

/// `IPrefetchableSupport` is read from the processor, and its answer reaches the
/// caller as the exact state the plugin reports.
///
/// The twin reports `kIsYetPrefetchable`; `audio-probe` implements nothing, so a
/// host that returned a constant fails on one of the two.
#[test]
fn prefetchable_support_is_queryable() {
    let main = main_thread();
    assert_eq!(
        require_twin().prefetchable_support(&main),
        Some(PrefetchSupport::Yes),
        "the twin reports kIsYetPrefetchable"
    );
    assert_eq!(
        require_probe().prefetchable_support(&main),
        None,
        "audio-probe does not implement IPrefetchableSupport"
    );
}

/// MIDI-learn arming must forward captured CCs to the plugin on the main thread.
///
/// `IMidiLearn::onLiveMIDIControllerInput` is main-thread-only, but the CCs it
/// receives are captured on the audio thread, so the host has to buffer and
/// forward. The twin aborts a call from any thread but the one it was loaded
/// on, so a host that forwards straight from `process` kills the test process,
/// and it moves `aux` to the controller number over 127, so the forward is
/// observable: it must be the last captured CC (PAN, 10), and nothing moves it
/// before the poll.
#[test]
fn midi_learn_forwards_from_the_main_thread() {
    let path = tutti_fixtures::vst3_cpp_module();
    let mut inst = Active::<f32>::load(&path, 48_000.0, 512)
        .unwrap_or_else(|e| panic!("the twin failed to load: {e:?}"));
    const AUX: u32 = 1;
    let before = inst.get_parameter(AUX);
    assert_eq!(before, 0.25, "aux starts at its default");

    inst.loaded_mut().arm_midi_learn(&main_thread(), true);
    assert!(inst.loaded().is_midi_learn_armed());

    // Feed CCs through the *audio* path; the host must capture them there and
    // forward on the next main-thread poll, not call the plugin inline.
    // MIDI 2.0 CC values are 32-bit; 0x8000_0000 is mid-scale.
    let cc = [
        (
            at(0),
            midi::cc(MidiChannel::FIRST, CCNumber::VOLUME, 0x8000_0000),
        ),
        (
            at(64),
            midi::cc(MidiChannel::FIRST, CCNumber::PAN, 0x4000_0000),
        ),
    ];
    let ins: Vec<Vec<f32>> = (0..2).map(|_| vec![0.0; 512]).collect();
    let mut outs: Vec<Vec<f32>> = (0..2).map(|_| vec![0.0; 512]).collect();
    let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
    let mut buffer = AudioBuffer {
        inputs: &in_refs,
        outputs: &mut out_refs,
        num_samples: 512,
        sample_rate: 48_000.0,
    };
    inst.process(
        &mut buffer,
        &InputEvents {
            midi: &cc,
            ..Default::default()
        },
        None,
        &TransportInfo::default(),
    );
    assert_eq!(
        inst.get_parameter(AUX),
        before,
        "the plugin was called from the audio path, before any main-thread poll"
    );

    // The forward happens here, on this (main) thread.
    let _ = inst
        .loaded_mut()
        .poll_plugin_notifications(&main_thread(), |_| {});
    assert_eq!(
        inst.get_parameter(AUX),
        f64::from(CCNumber::PAN.get()) / 127.0,
        "the poll forwards the captured controllers in order, so the last one (PAN) wins"
    );
    inst.loaded_mut().arm_midi_learn(&main_thread(), false);
    assert!(!inst.loaded().is_midi_learn_armed());
}

/// Plugin state must survive a `getState` → `setState` → `getState` round trip.
///
/// This is the persistence path: a project save/reload runs exactly this, against every plugin the suite can load.
#[test]
fn plugin_state_round_trips() {
    require_harness();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        let mut inst = match Active::<f32>::load(&path, 48_000.0, 512) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("  {name}: skipped (load failed {e:?})");
                continue;
            }
        };

        let Ok(first) = inst.get_state() else {
            // A plugin with no state at all is legal; nothing to round-trip.
            continue;
        };
        if first.is_empty() {
            continue;
        }
        exercised += 1;

        if let Err(e) = inst.set_state(&first) {
            failures.push(format!("{name}: set_state rejected its own state: {e:?}"));
            continue;
        }
        match inst.get_state() {
            Ok(second) => {
                if first != second {
                    failures.push(format!(
                        "{name}: state changed across round trip ({} bytes -> {} bytes)",
                        first.len(),
                        second.len()
                    ));
                }
            }
            Err(e) => failures.push(format!("{name}: second getState failed: {e:?}")),
        }
    }

    // Asserted, not printed. Every bundle `build.rs` produces exposes
    // component state, so zero here does not mean "no plugin has state" — it
    // means no plugin loaded, and the `failures.is_empty()` check below would
    // then pass having round-tripped nothing.
    assert!(
        exercised > 0,
        "no sample plugin exposed state, so no round trip was performed — with \
         the in-repo corpus that means nothing loaded rather than that nothing \
         has state"
    );
    {
        eprintln!("state round-trip: {exercised} plugins exercised");
    }
    assert!(
        failures.is_empty(),
        "plugin state did not round-trip:\n{}",
        failures.join("\n")
    );
}
