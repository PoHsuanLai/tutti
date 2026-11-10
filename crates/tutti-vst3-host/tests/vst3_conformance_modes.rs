//! VST3 host conformance: Process modes (offline, prefetch, the live toggle) and the host-context hand-off.
//!
//! The harness (HostCheck's C ABI, the plugin lock, discovery, `drive_block`) is
//! `support/conformance.rs`, which also carries the suite's overview.

#![cfg(feature = "conformance")]

#[path = "support/conformance.rs"]
mod conformance_support;

use std::os::raw::{c_int, c_uint, c_void};
use std::sync::Mutex;
use tutti_fixtures::main_thread;

use tutti_vst3_host::{
    AudioBuffer, InputEvents, LoadOptions, ProcessMode, TransportInfo, Vst3Loaded,
    host::conformance,
};

use conformance_support::*;

// ── Process modes ────────────────────────────────────────────────────────────
//
// `ProcessSetup::processMode` and `ProcessData::processMode` must agree;
// `ProcessSetupCheck::check` reports `kLogIdInvalidProcessMode` (severity
// Error) when they do not, permitting exactly one divergence — a per-block
// toggle between `kRealtime` and `kPrefetch`. Because the checker is configured
// from the observed `ProcessSetup` (see `drive_blocks_in_mode`), every one of
// these tests validates the host's *real* setup against the host's real block,
// so a mode that never reached `setupProcessing` fails rather than passes.

/// An offline bounce must be spec-clean for every sample plugin.
///
/// The regression this pins: `processMode` was hardcoded to `kRealtime` at
/// every site, so asking for an offline render silently produced the realtime
/// result — no error, and nothing downstream could tell. Plugins with lookahead
/// limiters, high-quality resamplers, or longer offline FFT windows are
/// entitled to behave differently here, and could not.
#[test]
fn offline_mode_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    // Every plugin that could not be driven at all. Counted rather than merely
    // printed: `failures` holds only *spec violations*, so a run where nothing
    // loaded leaves it empty and the assertion below passes having checked
    // nothing. That is the same vacuity the skip guards had, one level down.
    let mut unloadable: Vec<String> = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        // Partial blocks alongside the full one: offline renders end on a
        // short block, which is where a stale `numSamples` would show up.
        let blocks = [Block::of(512), Block::of(64), Block::of(1)];
        exercised += 1;
        match drive_blocks_in_mode::<f32>(&path, 48_000.0, 512, ProcessMode::Offline, &blocks) {
            Ok(per_block) => {
                for (i, findings) in per_block.iter().enumerate() {
                    let errs = errors(findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} block {i} (offline):\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
            }
            Err(e) => {
                eprintln!("  {name}: skipped ({e})");
                unloadable.push(format!("{name}: {e}"));
            }
        }
    }

    assert!(exercised > 0, "no sample plugins to drive offline");
    assert!(
        failures.is_empty(),
        "VST3 spec violations in offline mode:\n{}",
        failures.join("\n")
    );

    // Guard the premise. `failures` counts violations, not work done, so this
    // is what separates "every plugin was clean" from "no plugin ran".
    assert!(
        unloadable.len() < sample_plugins().len().max(1),
        "no plugin could be driven, so nothing was checked: {unloadable:?}"
    );
}

/// Prefetch mode, selected at activation, must be spec-clean.
///
/// `prefetchable.vst3` is the corpus plugin that implements
/// `IPrefetchableSupport`, but the mode is the *host's* to request and every
/// plugin must tolerate it, so this sweeps them all.
#[test]
fn prefetch_mode_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    // Every plugin that could not be driven at all. Counted rather than merely
    // printed: `failures` holds only *spec violations*, so a run where nothing
    // loaded leaves it empty and the assertion below passes having checked
    // nothing. That is the same vacuity the skip guards had, one level down.
    let mut unloadable: Vec<String> = Vec::new();

    for (name, path) in sample_plugins() {
        let blocks = [Block::of(512), Block::of(64)];
        match drive_blocks_in_mode::<f32>(&path, 48_000.0, 512, ProcessMode::Prefetch, &blocks) {
            Ok(per_block) => {
                for (i, findings) in per_block.iter().enumerate() {
                    let errs = errors(findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} block {i} (prefetch):\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
            }
            Err(e) => {
                eprintln!("  {name}: skipped ({e})");
                unloadable.push(format!("{name}: {e}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations in prefetch mode:\n{}",
        failures.join("\n")
    );

    // Guard the premise. `failures` counts violations, not work done, so this
    // is what separates "every plugin was clean" from "no plugin ran".
    assert!(
        unloadable.len() < sample_plugins().len().max(1),
        "no plugin could be driven, so nothing was checked: {unloadable:?}"
    );
}

/// Toggling realtime↔prefetch on a *live* instance must stay spec-clean.
///
/// This is the one setup/data divergence VST3 allows without re-running
/// `setupProcessing` (`ProcessSetupCheck::check`'s explicit exception), so the
/// blocks here deliberately run with `ProcessData::processMode` differing from
/// the negotiated `ProcessSetup::processMode` — and `kLogIdInvalidProcessMode`
/// must still not fire. A host that re-ran setup on the toggle, or one that
/// refused the toggle outright, would both fail to reach this state.
#[ignore = "needs a VST3 SDK sample whose controller inherits from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run with --ignored."]
#[test]
fn live_realtime_prefetch_toggle_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let path = require_host_checker();

    let n = unsafe { hc_num_log_events() } as usize;
    let mut inst = match Active::<f32>::load(&path, 48_000.0, 512) {
        Ok(i) => i,
        Err(e) => panic!("host-checker failed to load: {e:?}"),
    };
    let info = inst.loaded().plugin_info().clone();

    assert_eq!(
        inst.process_mode(),
        ProcessMode::Realtime,
        "plain activation must stay realtime"
    );

    let in_ch: Vec<c_int> = info
        .input_bus_channels
        .iter()
        .map(|&c| c as c_int)
        .collect();
    let out_ch: Vec<c_int> = info
        .output_bus_channels
        .iter()
        .map(|&c| c as c_int)
        .collect();
    let event_in = info.has_midi_input as c_int;
    let event_out = info.has_midi_output as c_int;
    for id in inst.loaded().parameter_ids(&main_thread()) {
        let _hc = hostcheck_lock();
        unsafe { hc_add_parameter(id.get() as c_uint) };
    }

    // Record the mode pair the host presented each block alongside the
    // findings, so the assertions below can tell "clean because the toggle
    // worked" from "clean because nothing ever changed".
    let observed: &'static Mutex<Vec<(c_int, c_int)>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let counts_cell: &'static Mutex<Vec<i64>> = Box::leak(Box::new(Mutex::new(vec![0i64; n])));

    conformance::set_observer(Box::new(move |data, setup| {
        let _hc = hostcheck_lock();
        unsafe {
            hc_configure(
                setup.sampleRate,
                setup.maxSamplesPerBlock,
                setup.symbolicSampleSize,
                setup.processMode,
                in_ch.as_ptr(),
                in_ch.len() as c_int,
                out_ch.as_ptr(),
                out_ch.len() as c_int,
                event_in,
                event_out,
            );
        }
        let mut counts = vec![0i64; n];
        let ptr = std::ptr::from_ref(data).cast::<c_void>();
        unsafe { hc_validate(ptr, data.numInputs, data.numOutputs, counts.as_mut_ptr()) };
        observed
            .lock()
            .unwrap()
            .push((setup.processMode, data.processMode));
        *counts_cell.lock().unwrap() = counts;
    }));

    let transport = TransportInfo::default();
    let mut failures = Vec::new();

    for (i, prefetch) in [false, true, true, false, true].into_iter().enumerate() {
        assert!(
            inst.set_prefetch(prefetch),
            "block {i}: realtime<->prefetch toggle refused on a realtime-activated instance"
        );

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

        let findings = findings_from(&counts_cell.lock().unwrap());
        let errs = errors(&findings);
        if !errs.is_empty() {
            failures.push(format!(
                "block {i} (prefetch={prefetch}):\n{}",
                errs.iter()
                    .map(|e| format!("    {e}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }

    conformance::clear_observer();

    let seen = observed.lock().unwrap().clone();
    assert_eq!(seen.len(), 5, "observer did not fire once per block");
    // The setup half must never move: the whole point of the exception is that
    // a toggle does *not* re-run `setupProcessing`.
    assert!(
        seen.iter().all(|&(setup_mode, _)| setup_mode == seen[0].0),
        "ProcessSetup::processMode changed across a live toggle — the host re-ran \
         setupProcessing instead of using the spec's realtime<->prefetch exception: {seen:?}"
    );
    // ...and the data half must actually have moved, or this test proves
    // nothing beyond "a constant equals itself".
    assert!(
        seen.iter()
            .any(|&(setup_mode, data_mode)| setup_mode != data_mode),
        "ProcessData::processMode never diverged from the setup — the toggle was a no-op, \
         so the spec exception under test was never exercised: {seen:?}"
    );
    assert!(
        failures.is_empty(),
        "VST3 spec violations across a live realtime<->prefetch toggle:\n{}",
        failures.join("\n")
    );
}

/// An offline-activated instance must refuse the live realtime/prefetch toggle.
///
/// Reaching realtime or prefetch from `kOffline` needs a fresh
/// `setupProcessing`; doing it per-block would put `ProcessData` out of
/// agreement with `ProcessSetup` in a way the spec's exception does *not*
/// cover, and `kLogIdInvalidProcessMode` would fire. The host therefore
/// declines rather than producing an invalid block — and must keep rendering
/// offline, which is what a bounce asked for.
#[ignore = "needs a VST3 SDK sample whose controller inherits from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run with --ignored."]
#[test]
fn offline_instance_refuses_the_live_toggle() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let path = require_host_checker();
    let Ok(mut inst) = Active::<f32>::load_with_mode(&path, 48_000.0, 512, ProcessMode::Offline)
    else {
        panic!("host-checker failed to load offline");
    };

    assert_eq!(inst.process_mode(), ProcessMode::Offline);
    assert!(
        !inst.set_prefetch(true),
        "offline instance accepted a prefetch toggle; that would put ProcessData out of \
         agreement with the negotiated ProcessSetup"
    );
    assert!(
        !inst.set_prefetch(false),
        "offline instance accepted a realtime toggle"
    );
    assert_eq!(
        inst.process_mode(),
        ProcessMode::Offline,
        "a refused toggle must leave the mode untouched"
    );
}

/// The plugin's own view: `host-checker` must report back that it saw offline.
///
/// An independent check on the linked-in `HostCheck`. `HostCheckerProcessor`
/// publishes the mode it observed through `outputParameterChanges` as
/// `kParamProcessModeTag` carrying `processMode * 0.5`, so decoding it proves
/// `kOffline` crossed the FFI and arrived inside the plugin — not merely that
/// our own struct held the right integer. If the host regressed to a hardcoded
/// `kRealtime`, this reads 0.0 instead of 1.0 and fails.
#[ignore = "needs a VST3 SDK sample whose controller inherits from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run with --ignored."]
#[test]
fn plugin_observes_the_offline_mode_we_requested() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let path = require_host_checker();
    let Ok(mut inst) = Active::<f32>::load_with_mode(&path, 48_000.0, 512, ProcessMode::Offline)
    else {
        panic!("host-checker failed to load offline");
    };
    let info = inst.loaded().plugin_info().clone();
    let transport = TransportInfo::default();

    // The plugin emits the tag only when the mode *changed* since the last
    // block (`mLastProcessMode`), and it initialises that field to -1 — so the
    // first block reports, and later ones stay silent. Scan every block rather
    // than only the last.
    let mut reported: Option<f64> = None;
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
        let out = inst.process(&mut buffer, &InputEvents::default(), None, &transport);
        if let Some(queue) = out
            .parameter_changes
            .queue(param_address(K_PARAM_PROCESS_MODE_TAG))
            && let Some(point) = queue.points.last()
        {
            reported = Some(point.value.get());
        }
    }

    let Some(value) = reported else {
        panic!(
            "host-checker never published kParamProcessModeTag; either the host emitted no \
             ProcessData or output parameter changes are not being read back"
        );
    };
    // The plugin sends `processMode * 0.5` (hostcheckerprocessor.cpp), so
    // kOffline == 2 arrives as 1.0.
    let observed = (value * 2.0).round() as c_int;
    assert_eq!(
        observed, K_OFFLINE,
        "plugin observed process mode {observed}, but the host was asked for kOffline \
         ({K_OFFLINE}) — the requested mode did not reach ProcessData"
    );
}

/// The host **lends** its `IHostApplication` to the plugin: `IPluginBase::
/// initialize` borrows the context rather than consuming a reference, so a
/// load must not strand one.
///
/// This is the load-path counterpart to the accessor-level test in
/// `com::host_application` — that one pins the `to_com_ptr`/`as_com_ref`
/// primitives, this one pins the call site that has to pick the borrowing form.
///
/// Each `Vst3Loaded` builds its own `HostApplication`, so the count is read
/// within one instance: after `initialize()` it is the host's own reference
/// plus however many the plugin chose to retain. A same-object controller
/// shares the component's retain and a separate one takes its own, so the legal
/// ceiling is host + component + controller. Measured against the SDK sample
/// plugins: 3 with the borrowing hand-off, 4 once `to_com_ptr().into_raw()`
/// strands its extra reference — so this bound discriminates, it is not slack.
///
/// Needs only a sample plugin, not the HostChecker sources.
#[test]
fn host_context_is_borrowed_not_consumed() {
    // Hand-rolled the same vacuous skip `harness_ready` did, for the same
    // reason and with the same consequence. It needs only a plugin, not the
    // HostChecker sources — but a plugin is not optional either, so the
    // requirement is asserted rather than shrugged at.
    assert!(
        !sample_plugins().is_empty(),
        "no VST3 plugin to drive; `build.rs` builds `audio-probe` into \
         {PROBE_DIR_BUILT:?} on every conformance build"
    );
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        let Ok(loaded) = Vst3Loaded::load(&main_thread(), &path, LoadOptions::new()) else {
            eprintln!("  {name}: skipped (load failed)");
            continue;
        };
        exercised += 1;

        let after_init = loaded.host_context_refcount();
        if after_init > 3 {
            failures.push(format!(
                "{name}: host-context refcount {after_init} after initialize() exceeds \
                 host(1) + component(1) + controller(1); the hand-off leaked a reference"
            ));
        }
        drop(loaded);
    }

    // Asserted, not printed: the refcount bound below is only a claim if an
    // instance was actually constructed to measure.
    assert!(
        exercised > 0,
        "no sample plugin loaded, so no host-context refcount was measured"
    );
    {}

    assert!(
        failures.is_empty(),
        "host context is being leaked at initialize():\n{}",
        failures.join("\n")
    );
}
