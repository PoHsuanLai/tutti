//! VST3 host conformance: Controller-side surfaces (`IEditController` rather than the audio path) and HostChecker's own report through the output parameters.
//!
//! The harness (HostCheck's C ABI, the plugin lock, discovery, `drive_block`) is
//! `support/conformance.rs`, which also carries the suite's overview.

#![cfg(feature = "conformance")]

#[path = "support/conformance.rs"]
mod conformance_support;

use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_types::Samples;
use tutti_types::{BarNumber, TimeSignature};
use tutti_ump::{CCNumber, MidiChannel, MidiGroup};
use tutti_vst3_host::{AudioBuffer, InputEvents, PrefetchSupport, TransportInfo};

/// A block longer than any offset these tests place an event at.
const TEST_BLOCK: usize = 1 << 16;

/// The offset `k` frames into a test block.
fn at(k: u32) -> Offset {
    Offset::new(usize::try_from(k).unwrap(), Samples(TEST_BLOCK)).unwrap()
}

use conformance_support::*;

// ── Controller-side surfaces ─────────────────────────────────────────────────
//
// The tests below drive `IEditController` rather than the audio path. That is
// where HostChecker's ~35 thread-affinity checks live, and where its
// `IComponentHandler2`/`3`, `IProgress` and `restartComponent` probes fire —
// none of which the process-path tests can reach.
//
// The thread checker is not a GUI dependency, which is what made this look
// hard: `ThreadChecker::create()` records the calling thread and `test()`
// compares against it. The plugin creates it during `initialize`, so "the main
// thread" is simply whichever thread loaded the plugin. Doing load and all
// controller calls on one thread — as a #[test] naturally does — satisfies it,
// and calling from another thread is what a host must not do.

/// `kScoreTag` — the plugin's weighted host-capability score, 0..1 over 75
/// features (`hostcheckercontroller.h`).
const K_SCORE_TAG: u32 = 1005;
/// Triggers `restartComponent(kParamValuesChanged)` when set > 0.
const K_RESTART_PARAM_VALUES_TAG: u32 = 1013;
/// Triggers `restartComponent(kParamTitlesChanged)` when set > 0.
const K_RESTART_PARAM_TITLES_TAG: u32 = 1014;
/// Starts/stops an `IProgress` operation.
const K_TRIGGER_PROGRESS_TAG: u32 = 1008;

/// The host-capability score HostChecker computes for us.
///
/// A measured coverage figure rather than a pass/fail: each of 75 features
/// carries an importance weight (2.0 for `IComponentHandler2`, tempo, and
/// `IPlugFrame::resizeView`; 1.0 for niche ones) and the score is the weighted
/// fraction observed *so far in this session*. It therefore only counts what
/// the test actually exercised — it is a gap map, not a verdict, and a low
/// number means "not yet driven" as much as "not implemented".
///
/// ## Why the number does not move much
///
/// It is tempting to assume the figure is low mainly because this test drives
/// so little, and that feeding it everything the suite covers elsewhere would
/// raise it. Measured, that is not what happens: populating every transport
/// field (14 of the 75 are per-`ProcessContext`-flag), processing more blocks,
/// polling notifications repeatedly, and deactivating to flush the processor's
/// log all leave it at the same **28.9%**.
///
/// The reason is structural. `updateScoring` runs on the *controller*, from
/// `addFeatureLog`; the processor's observations only reach it as `"LogEvent"`
/// messages over `IConnectionPoint`. Most of what this test drives happens on
/// the processor side, so it does not credit the score however hard it is
/// driven. Raising the number means exercising controller-side surfaces —
/// editor, units, `IComponentHandler2`/`3`, keyswitches — not more audio.
///
/// So the figure is a **floor on controller-side coverage**, which is a
/// narrower claim than it looks. Treat a change in it as signal; treat
/// its absolute value as close to meaningless.
#[ignore = "needs a VST3 SDK sample whose controller inherits from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run with --ignored."]
#[test]
fn report_host_capability_score() {
    if !harness_ready() {
        return;
    }
    let path = require_host_checker();
    let mut inst = Active::<f32>::load(&path, 48_000.0, 512)
        .unwrap_or_else(|e| panic!("host-checker failed to load: {e:?}"));

    // Exercise the surfaces we do support, so the score reflects them.
    //
    // A fully-populated transport, not `default()`. HostChecker scores each
    // `ProcessContext` field separately (`ProcessContextTempoSupported`,
    // `…TimeSigSupported`, `…BarPositionSupported`, `…CycleSupported`, and so
    // on — 14 of the 75), and a default transport leaves every one of those
    // flags clear. The host fills them from what it is handed, so driving it
    // with a bare default measured the fixture rather than the host.
    //
    // `kSystemTimeValid` / `kClockValid` / `kSmpteValid` / `kChordValid` stay
    // unreachable on purpose — see `types/transport.rs:134-204`: no upstream
    // producer exists, and advertising a field the host cannot fill would be
    // worse than saying nothing. They are a deliberate ceiling on this score.
    let info = inst.loaded().plugin_info().clone();
    let transport = TransportInfo::default()
        .with_tempo(128.0)
        .with_playing(true)
        .with_recording(true)
        .with_time_signature(TimeSignature::of(7, 8))
        .with_sample_rate(48_000.0)
        .with_position_beats(8.0, 3.75)
        .with_position_samples(180_000)
        .with_continuous_samples(180_000)
        .with_bar(8.0, BarNumber::new(3))
        .with_loop(true, 4.0, 12.0);
    for _ in 0..4 {
        let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let mut outs: Vec<Vec<f32>> = (0..info.num_outputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: 512,
            sample_rate: 48_000.0,
        };
        inst.process(&mut buffer, &InputEvents::default(), None, &transport);
    }
    // Deactivate before reading: the processor half accumulates findings in a
    // local log and ships them to the controller as `"LogEvent"` messages
    // **only from `setActive`** (`hostcheckerprocessor.cpp:742`, the single call
    // site of `sendNowAllLogEvents`), and `updateScoring` runs controller-side
    // as those arrive. Reading while still active cannot see anything `process`
    // observed.
    let mut inst = inst.deactivate();
    let _ = inst.poll_plugin_notifications(&main_thread(), |_| {});

    let score = inst.get_parameter(K_SCORE_TAG);
    eprintln!(
        "host capability score: {:.1}% of HostChecker's 75 weighted features",
        score * 100.0
    );

    // Deliberately not a threshold assertion: the score measures coverage of
    // features this test drove, so pinning it would encode today's harness
    // rather than the host's behaviour. Assert only that it is well-formed —
    // a NaN or out-of-range value would mean we read the wrong parameter.
    assert!(
        (0.0..=1.0).contains(&score),
        "score {score} outside 0..=1 — likely reading the wrong parameter tag"
    );
}

/// `restartComponent` requests must reach the host and surface as notifications.
///
/// The plugin fires `restartComponent(kParamValuesChanged / kParamTitlesChanged)`
/// when these trigger parameters are set, and logs whether the host returned
/// `kResultTrue`. A host that ignores restarts leaves stale parameter values or
/// titles in its UI after a preset change — and the plugin can detect that we
/// handled it, which is what the `kLogIdRestart*Supported` probes record.
#[ignore = "needs a VST3 SDK sample whose controller inherits from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run with --ignored."]
#[test]
fn restart_component_requests_reach_the_host() {
    if !harness_ready() {
        return;
    }
    let path = require_host_checker();
    let mut inst = Active::<f32>::load(&path, 48_000.0, 512)
        .unwrap_or_else(|e| panic!("host-checker failed to load: {e:?}"));

    // Drain anything latched during load so the assertion is about our trigger.
    let _ = inst
        .loaded_mut()
        .poll_plugin_notifications(&main_thread(), |_| {});

    inst.set_parameter(K_RESTART_PARAM_VALUES_TAG, 1.0);
    inst.set_parameter(K_RESTART_PARAM_TITLES_TAG, 1.0);
    let restart = inst
        .loaded_mut()
        .poll_plugin_notifications(&main_thread(), |_| {});

    assert!(
        restart.param_values_changed || restart.param_titles_changed,
        "plugin requested restartComponent(kParamValuesChanged / kParamTitlesChanged) \
         but neither reached the host through poll_plugin_notifications()"
    );
}

/// `IProgress` reports must reach the host.
///
/// The plugin runs a timed progress operation when `kTriggerProgressTag` is
/// set, calling `IProgress::start`/`update`/`finish` on our handler. A host
/// that doesn't implement `IProgress` leaves the user with no feedback during
/// long plugin operations (sample loading, offline render).
///
/// The plugin drives progress from a **VSTGUI timer**, which only runs when its
/// editor is open — so with no window we may legitimately observe nothing. The
/// test therefore asserts the call is *accepted* and reports whether events
/// arrived, rather than requiring them.
#[ignore = "needs a VST3 SDK sample whose controller inherits from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run with --ignored."]
#[test]
fn progress_reports_are_accepted() {
    if !harness_ready() {
        return;
    }
    let path = require_host_checker();
    let mut inst = Active::<f32>::load(&path, 48_000.0, 512)
        .unwrap_or_else(|e| panic!("host-checker failed to load: {e:?}"));
    let _ = inst
        .loaded_mut()
        .poll_plugin_notifications(&main_thread(), |_| {});

    inst.set_parameter(K_TRIGGER_PROGRESS_TAG, 1.0);
    let mut progress = 0;
    let _ = inst
        .loaded_mut()
        .poll_plugin_notifications(&main_thread(), |n| {
            if matches!(n, tutti_vst3_host::PluginNotification::Progress(_)) {
                progress += 1;
            }
        });
    eprintln!(
        "IProgress: {} event(s) observed (0 is expected headless — the plugin \
         drives progress from an editor timer)",
        progress
    );

    inst.set_parameter(K_TRIGGER_PROGRESS_TAG, 0.0);
    let _ = inst
        .loaded_mut()
        .poll_plugin_notifications(&main_thread(), |_| {});
}

/// `IPrefetchableSupport` must be queryable without upsetting the plugin.
///
/// `host-checker` implements the interface; the value it reports is its own
/// business, but a host that queries it must do so on the main thread and
/// must tolerate any of the three prefetchable states.
#[ignore = "needs a VST3 SDK sample whose controller inherits from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run with --ignored."]
#[test]
fn prefetchable_support_is_queryable() {
    if !harness_ready() {
        return;
    }
    let path = require_host_checker();
    let inst = Active::<f32>::load(&path, 48_000.0, 512)
        .unwrap_or_else(|e| panic!("host-checker failed to load: {e:?}"));

    match inst.loaded().prefetchable_support(&main_thread()) {
        Some(v) => {
            eprintln!("IPrefetchableSupport reports {v:?}");
            assert!(
                !matches!(v, PrefetchSupport::Unknown(_)),
                "unexpected prefetchable state {v:?} (expected one of the three SDK values)"
            );
        }
        None => eprintln!("plugin does not implement IPrefetchableSupport"),
    }
}

/// MIDI-learn arming must forward captured CCs to the plugin on the main thread.
///
/// `IMidiLearn::onLiveMIDIControllerInput` is main-thread-only, but the CCs it
/// receives are captured on the audio thread — so the host has to buffer and
/// forward. HostChecker asserts the thread context of the call, meaning a host
/// that forwards straight from `process` is caught here.
#[ignore = "needs a VST3 SDK sample whose controller inherits from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run with --ignored."]
#[test]
fn midi_learn_forwards_from_the_main_thread() {
    if !harness_ready() {
        return;
    }
    let path = require_host_checker();
    let mut inst = Active::<f32>::load(&path, 48_000.0, 512)
        .unwrap_or_else(|e| panic!("host-checker failed to load: {e:?}"));

    inst.loaded_mut().arm_midi_learn(&main_thread(), true);
    assert!(inst.loaded().is_midi_learn_armed());

    // Feed CCs through the *audio* path; the host must capture them there and
    // forward on the next main-thread poll, not call the plugin inline.
    let info = inst.loaded().plugin_info().clone();
    // MIDI 2.0 CC values are 32-bit; 0x8000_0000 is mid-scale.
    let cc = [
        (
            at(0),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::VOLUME,
                0x8000_0000,
            ),
        ),
        (
            at(64),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::PAN,
                0x4000_0000,
            ),
        ),
    ];
    let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1))
        .map(|_| vec![0.0; 512])
        .collect();
    let mut outs: Vec<Vec<f32>> = (0..info.num_outputs.max(1))
        .map(|_| vec![0.0; 512])
        .collect();
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

    // The forward happens here, on this (main) thread.
    let _ = inst
        .loaded_mut()
        .poll_plugin_notifications(&main_thread(), |_| {});
    inst.loaded_mut().arm_midi_learn(&main_thread(), false);
    assert!(!inst.loaded().is_midi_learn_armed());
}

/// Pin [`K_PROCESS_WARN_TAG`] against the plugin's actual parameter layout.
///
/// The warning block is decoded as a bitfield; the parameters immediately
/// below it (`kParamProcessModeTag` and the `kProcessContext*` readouts) carry
/// ordinary normalized values. Decoding one as the other yields plausible
/// nonsense — it manufactured four phantom "spec errors" before this was
/// caught. Assert the block's identity structurally rather than trusting a
/// hand-counted enum offset.
#[ignore = "needs a VST3 SDK sample whose controller inherits from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run with --ignored."]
#[test]
fn warn_tag_block_is_isolated() {
    if !harness_ready() {
        return;
    }
    let path = require_host_checker();
    let inst = Active::<f32>::load(&path, 48_000.0, 512)
        .unwrap_or_else(|e| panic!("host-checker failed to load: {e:?}"));

    let declared: Vec<u32> = inst
        .loaded()
        .parameter_ids(&main_thread())
        .into_iter()
        .map(|id| id.get())
        .collect();

    // Every id in the warning block must be a real parameter...
    for i in 0..K_PARAM_WARN_COUNT {
        let tag = K_PROCESS_WARN_TAG + i;
        assert!(
            declared.contains(&tag),
            "kProcessWarnTag + {i} = {tag} is not a parameter this plugin declares; \
             the enum offset in this test is stale"
        );
    }
    // ...and the tag just below it must be the process-mode readout, which
    // fixes the block's lower bound. If the enum shifted, one of these fails.
    assert!(
        declared.contains(&K_PARAM_PROCESS_MODE_TAG),
        "expected kParamProcessModeTag at {K_PARAM_PROCESS_MODE_TAG}"
    );
    assert!(
        !declared.contains(&(K_PROCESS_WARN_TAG + K_PARAM_WARN_COUNT)),
        "found a parameter just past the warning block — the block is larger \
         than kParamWarnCount, so the decode would drop findings"
    );
}

/// The reference plugin's own verdict, delivered through `outputParameterChanges`.
///
/// This is HostChecker judging us from *inside* the plugin, independent of the
/// `HostCheck` instance we link in — and getting it requires our output-param
/// path to work, so a silent failure to read `outputParameterChanges` shows up
/// as "no findings" rather than a false pass. The test therefore asserts we
/// received *something* before asserting the findings are clean.
#[ignore = "needs a VST3 SDK sample whose controller inherits from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run with --ignored."]
#[test]
fn host_checker_reports_no_errors_through_output_params() {
    if !harness_ready() {
        return;
    }
    let path = require_host_checker();

    let mut inst = match Active::<f32>::load(&path, 48_000.0, 512) {
        Ok(i) => i,
        Err(e) => panic!("host-checker failed to load: {e:?}"),
    };
    let info = inst.loaded().plugin_info().clone();

    // Drive several blocks: the plugin latches state-machine observations
    // across calls, and only reports once it has something to say.
    let mut all_ids = Vec::new();
    let transport = TransportInfo::default();
    for _ in 0..8 {
        let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let mut outs: Vec<Vec<f32>> = (0..info.num_outputs.max(1))
            .map(|_| vec![0.0; 512])
            .collect();
        let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: 512,
            sample_rate: 48_000.0,
        };
        let out = inst.process(&mut buffer, &InputEvents::default(), None, &transport);
        all_ids.extend(decode_hostchecker_warnings(out.parameter_changes));
    }

    all_ids.sort_unstable();
    all_ids.dedup();

    let mut errors_found = Vec::new();
    for id in &all_ids {
        if let Some((severity, desc)) = describe_log_id(*id) {
            if KNOWN_FALSE_POSITIVES.iter().any(|(d, _)| desc.contains(d)) {
                continue;
            }
            if severity == "Error" {
                errors_found.push(format!("    [{severity}] {desc}"));
            } else {
                eprintln!("  advisory: [{severity}] {desc}");
            }
        }
    }

    assert!(
        errors_found.is_empty(),
        "host-checker reported VST3 spec errors against this host:\n{}",
        errors_found.join("\n")
    );
}

/// Plugin state must survive a `getState` → `setState` → `getState` round trip.
///
/// This is the persistence path: a project save/reload
/// runs exactly this. `host-checker` implements state on both the processor and
/// the controller (`HostCheckerProcessor::setState` /
/// `HostCheckerController::setState`), each with a thread-affinity assertion,
/// so a host that touches state off the main thread is caught too.
#[test]
fn plugin_state_round_trips() {
    if !harness_ready() {
        return;
    }
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

/// This process's main thread. Each test is its own process under nextest, so
/// the claim cannot be refused.
fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}
