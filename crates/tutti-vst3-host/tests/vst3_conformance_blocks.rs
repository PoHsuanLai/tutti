//! VST3 host conformance: The blocks the host builds, driven through the real process path, are spec-clean: steady and partial blocks, 64-bit, transport, notes, parameter changes and MIDI.
//!
//! The harness (HostCheck's C ABI, the HostCheck lock, discovery, `drive_block`) is
//! `support/conformance.rs`, which also carries the suite's overview.

#![cfg(feature = "conformance")]

#[path = "support/conformance.rs"]
mod conformance_support;

use std::ffi::CStr;

use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_types::Samples;
use tutti_ump::{MidiChannel, MidiGroup};
use tutti_vst3_host::{AudioBuffer, InputEvents, Library, ParameterChanges, TransportInfo};

/// A block longer than any offset these tests place an event at.
const TEST_BLOCK: usize = 1 << 16;

/// The offset `k` frames into a test block.
fn at(k: u32) -> Offset {
    Offset::new(usize::try_from(k).unwrap(), Samples(TEST_BLOCK)).unwrap()
}

use conformance_support::*;

// ── Tests ────────────────────────────────────────────────────────────────────

/// The harness itself works: checks are linked and the table is populated.
#[test]
fn hostcheck_is_linked() {
    harness_ready();
    let n = unsafe { hc_num_log_events() };
    assert!(
        n > 100,
        "expected the full HostChecker table, got {n} checks"
    );
    let d = unsafe { CStr::from_ptr(hc_log_description(0)) };
    assert!(!d.to_string_lossy().is_empty());
}

/// Report how much of the corpus the per-bundle sweeps actually reach.
///
/// Every other test here loads a bundle by path, and `Vst3Loaded::load` calls
/// `find_audio_class`, which takes the **first** class whose category contains
/// `"Audio"` (`loaded.rs:1617-1630`). One bundle can export many: `mda-vst3`
/// packs the whole mda suite into a single binary, which is the normal shape
/// for commercial VST3 — plugin *suites* ship as one bundle, not one per
/// effect.
///
/// So "19 plugins swept" overstates the coverage: it is 19 *bundles*, and every
/// class after the first in each is never instantiated. This test measures the
/// gap rather than asserting a threshold, because the number is a property of
/// which plugins happen to be built here.
///
/// It also pins the real limitation behind that: there is no public API to
/// instantiate a *chosen* class. A DAW needs one — a user picking "mda Delay"
/// from a bundle that also holds "mda Bandisto" cannot be served by
/// first-audio-class-wins.
#[test]
fn report_multi_class_bundle_coverage() {
    if !harness_ready() {
        return;
    }

    let mut bundles = 0usize;
    let mut audio_classes = 0usize;
    let mut multi = Vec::new();

    for (name, path) in sample_plugins() {
        let Ok(library) = Library::load(&path) else {
            continue;
        };
        let names: Vec<String> = (0..library.count_classes())
            .filter_map(|i| library.class_info(i).ok())
            .filter(|c| c.category.contains("Audio"))
            .map(|c| c.name.clone())
            .collect();
        if names.is_empty() {
            continue;
        }
        bundles += 1;
        audio_classes += names.len();
        if names.len() > 1 {
            multi.push(format!("  {name}: {} classes {names:?}", names.len()));
        }
    }

    eprintln!(
        "corpus: {bundles} bundles, {audio_classes} audio classes; the sweeps \
         instantiate {bundles} (first audio class per bundle), leaving {} \
         untested",
        audio_classes - bundles
    );
    if !multi.is_empty() {
        eprintln!("multi-class bundles:\n{}", multi.join("\n"));
    }

    // The premise of every other sweep: each bundle must yield at least one
    // audio class, or those tests are silently measuring nothing.
    assert!(
        bundles > 0,
        "no bundle in the corpus exposed an audio class — the sweeps that \
         load by path are all vacuous"
    );
    assert!(
        audio_classes >= bundles,
        "counted {audio_classes} audio classes across {bundles} bundles, which \
         is arithmetically impossible — the class enumeration is wrong"
    );
}

/// Drive **every** audio class in the corpus, not just the first per bundle.
///
/// This is the coverage [`report_multi_class_bundle_coverage`] measures: 55
/// classes behind 19 bundles, of which the path-loading sweeps reach 19. The 36
/// others include 33 of `mda-vst3`'s — the closest thing in this corpus to real
/// shipped plugins rather than teaching examples.
///
/// Each is loaded, activated, and driven for a block. Failures are collected
/// rather than panicking on the first, because one broken class should not hide
/// the state of the other 54.
#[test]
fn every_audio_class_survives_a_block() {
    if !harness_ready() {
        return;
    }

    let mut driven = 0usize;
    let mut failures = Vec::new();

    for (bundle, path) in sample_plugins() {
        let Ok(library) = Library::load(&path) else {
            continue;
        };
        let names: Vec<String> = (0..library.count_classes())
            .filter_map(|i| library.class_info(i).ok())
            .filter(|c| c.category.contains("Audio"))
            .map(|c| c.name.clone())
            .collect();
        // Drop before instantiating: `Vst3Loaded` opens its own handle, and
        // holding two to one DSO is needless.
        drop(library);

        for name in names {
            match Active::<f32>::load_class(&path, &name, 48_000.0, 512) {
                Ok(mut inst) => {
                    let info = inst.loaded().plugin_info().clone();
                    // The class actually instantiated must be the one asked
                    // for. Without this the sweep counts *attempts*: a
                    // `load_class` that ignored its argument would load the
                    // first class 55 times and still report "drove 55".
                    // Measured — that mutation passed until this assert existed.
                    if info.name != name {
                        failures.push(format!(
                            "{bundle}: asked for {name:?}, got {:?} — load_class \
                             is not selecting by name",
                            info.name
                        ));
                        continue;
                    }
                    let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1))
                        .map(|_| vec![0.0f32; 512])
                        .collect();
                    let mut outs: Vec<Vec<f32>> = (0..info.num_outputs.max(1))
                        .map(|_| vec![0.0f32; 512])
                        .collect();
                    let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
                    let mut out_refs: Vec<&mut [f32]> =
                        outs.iter_mut().map(|v| v.as_mut_slice()).collect();
                    let mut buffer = AudioBuffer {
                        inputs: &in_refs,
                        outputs: &mut out_refs,
                        num_samples: 512,
                        sample_rate: 48_000.0,
                    };
                    inst.process(
                        &mut buffer,
                        &InputEvents::default(),
                        None,
                        &TransportInfo::default(),
                    );

                    // Silence in must not become non-finite out: a NaN here
                    // propagates through every downstream node in the graph.
                    for (ch, out) in outs.iter().enumerate() {
                        if let Some(pos) = out.iter().position(|s| !s.is_finite()) {
                            failures.push(format!(
                                "{bundle}/{name}: non-finite sample at ch {ch} \
                                 index {pos} ({}) from silent input",
                                out[pos]
                            ));
                            break;
                        }
                    }
                    driven += 1;
                }
                Err(e) => failures.push(format!("{bundle}/{name}: load failed: {e:?}")),
            }
        }
    }

    eprintln!("drove {driven} audio classes");
    assert!(
        failures.is_empty(),
        "{} of {} audio classes failed:\n{}",
        failures.len(),
        driven + failures.len(),
        failures.join("\n")
    );
    // Guard the premise: a sweep that drove nothing must not report success.
    //
    // This was `driven > 40`, a figure taken from one developer's external SDK
    // build (~55 audio classes across ~18 bundles). That number is a property of
    // `VST3_SAMPLE_PLUGIN_DIR`, not of this host, and it is unset on essentially
    // every machine — so the moment these tests stopped skipping vacuously, the
    // literal failed on the in-repo corpus of two bundles. A premise guard that
    // only holds on one machine guards nothing on the others.
    //
    // The regression it was aimed at — class enumeration collapsing to
    // one-per-bundle — is now caught relative to the corpus actually present.
    // The *other* half of that concern, `load_class` ignoring which class it was
    // asked for, is caught by the `info.name != name` check above, which is a
    // per-class assertion and needs no corpus size at all.
    let bundles = sample_plugins().len();
    assert!(
        driven >= bundles && bundles > 0,
        "drove {driven} audio classes across {bundles} bundles — every bundle in \
         the corpus publishes at least one audio class, so a lower count means \
         class enumeration regressed"
    );
}

/// A plugin's sidechain inputs must be staged, not dropped.
///
/// `host-checker` declares a stereo main input plus several `kAux` inputs
/// (`hostcheckerprocessor.cpp:76-88`). `Vst3Active::activate_buses` loops
/// over `getBusCount` and activates every one, which is correct VST3: the
/// plugin's own `activateBus` scores `index > 0` as the informational feature
/// "IComponent::activateBus for SideChain supported!" rather than an error
/// (`hostcheckerprocessor.cpp:817-823`). Only an index *outside* the bus list
/// is a violation.
///
/// The observable consequence — and what this asserts — is that the host's own
/// per-bus staging covers every declared input bus. A host that stopped at bus
/// 0 would leave a compressor's sidechain reading silence, with no error raised
/// anywhere: `HostCheck::validate` reports process-time findings only, and the
/// plugin's `activateBus` log flushes from `setActive`, so neither channel
/// catches it. That is why this checks geometry rather than findings.
#[ignore = "needs a VST3 SDK sample whose controller inherits from VSTGUI::VST3EditorDelegate; VSTGUI is not one of this repo's pinned submodules. Set VST3_SAMPLE_PLUGIN_DIR to an external SDK build and run with --ignored."]
#[test]
fn sidechain_input_buses_are_staged() {
    if !harness_ready() {
        return;
    }
    let path = require_host_checker();
    let inst = Active::<f32>::load(&path, 48_000.0, 512)
        .unwrap_or_else(|e| panic!("host-checker failed to load: {e:?}"));

    let buses = &inst.loaded().plugin_info().input_bus_channels;
    eprintln!("host-checker input buses: {buses:?}");

    // The premise: this plugin must actually declare a sidechain, or the test
    // proves nothing. Assert it rather than silently passing on a one-bus build.
    assert!(
        buses.len() > 1,
        "host-checker is expected to declare aux inputs beside its main bus, \
         but the host resolved {} input bus(es) — either the plugin was built \
         without them or the host is collapsing the bus list",
        buses.len()
    );

    // Every declared bus must carry channels. A staged-but-empty aux bus is the
    // shape a dropped sidechain takes: present in the count, silent in use.
    for (index, &channels) in buses.iter().enumerate() {
        assert!(
            channels > 0,
            "input bus {index} of {buses:?} was staged with 0 channels — a \
             plugin reading its sidechain here would get nothing"
        );
    }
}

/// A plain steady-state block against every sample plugin must produce no
/// Error-severity findings. This is the broad sweep: it exercises bus
/// staging, channel pointers, block size, and setup agreement for each
/// plugin's own bus layout.
#[test]
fn steady_state_block_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let mut failures = Vec::new();
    // Every plugin that could not be driven at all. Counted rather than merely
    // printed: `failures` holds only *spec violations*, so a run where nothing
    // loaded leaves it empty and the assertion below passes having checked
    // nothing. That is the same vacuity the skip guards had, one level down.
    let mut unloadable: Vec<String> = Vec::new();

    for (name, path) in sample_plugins() {
        match drive_block::<f32>(&path, 512, 48_000.0, 512, &[], None) {
            Ok(findings) => {
                let errs = errors(&findings);
                if !errs.is_empty() {
                    failures.push(format!(
                        "{name}:\n{}",
                        errs.iter()
                            .map(|e| format!("    {e}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                } else {
                    eprintln!("  {name}: clean ({} advisory)", findings.len());
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
        "VST3 spec violations in the ProcessData this host built:\n{}",
        failures.join("\n")
    );

    // Guard the premise. `failures` counts violations, not work done, so this
    // is what separates "every plugin was clean" from "no plugin ran".
    assert!(
        unloadable.len() < sample_plugins().len().max(1),
        "no plugin could be driven, so nothing was checked: {unloadable:?}"
    );
}

/// A partial block (fewer frames than the negotiated max) must stay clean —
/// the host must not report a stale `numSamples` or mis-size its bus buffers.
#[test]
fn partial_block_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let mut failures = Vec::new();
    // Every plugin that could not be driven at all. Counted rather than merely
    // printed: `failures` holds only *spec violations*, so a run where nothing
    // loaded leaves it empty and the assertion below passes having checked
    // nothing. That is the same vacuity the skip guards had, one level down.
    let mut unloadable: Vec<String> = Vec::new();

    for (name, path) in sample_plugins() {
        for frames in [1usize, 17, 64, 511] {
            match drive_block::<f32>(&path, frames, 48_000.0, 512, &[], None) {
                Ok(findings) => {
                    let errs = errors(&findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} @ {frames} frames:\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
                Err(e) => {
                    eprintln!("  {name} @ {frames}: skipped ({e})");
                    unloadable.push(format!("{name} @ {frames}: {e}"));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations on partial blocks:\n{}",
        failures.join("\n")
    );

    // Guard the premise. `failures` counts violations, not work done, so this
    // is what separates "every plugin was clean" from "no plugin ran".
    assert!(
        unloadable.len() < sample_plugins().len().max(1),
        "no plugin could be driven, so nothing was checked: {unloadable:?}"
    );
}

/// f64 processing must build the same spec-clean `ProcessData` as f32.
///
/// This is a separate code path, not a parameter: `Vst3Sample` selects
/// `symbolicSampleSize` *and* which arm of the `channelBuffers32`/`64` union
/// the host writes. A host that sets one without the other hands the plugin a
/// pointer table it will read at the wrong width — silent garbage, since the
/// two union members overlay the same bytes. `ProcessSetupCheck` catches the
/// mismatch by comparing against the negotiated `ProcessSetup`.
#[test]
fn f64_processing_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let mut failures = Vec::new();
    // Every plugin that could not be driven at all. Counted rather than merely
    // printed: `failures` holds only *spec violations*, so a run where nothing
    // loaded leaves it empty and the assertion below passes having checked
    // nothing. That is the same vacuity the skip guards had, one level down.
    let mut unloadable: Vec<String> = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        // Activating f64 on a plugin that only does f32 is the host's error,
        // not something to assert on.
        if !supports_f64(&path) {
            continue;
        }
        for frames in [512usize, 1, 64] {
            exercised += 1;
            match drive_block::<f64>(&path, frames, 48_000.0, 512, &[], None) {
                Ok(findings) => {
                    let errs = errors(&findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} @ {frames} frames (f64):\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
                Err(e) => {
                    eprintln!("  {name} (f64): skipped ({e})");
                    unloadable.push(format!("{name} (f64): {e}"));
                }
            }
        }
    }

    if exercised == 0 {
        eprintln!("no f64-capable sample plugin available; f64 path not exercised");
    } else {
        eprintln!("f64: {exercised} blocks exercised");
    }
    assert!(
        failures.is_empty(),
        "VST3 spec violations on the f64 path:\n{}",
        failures.join("\n")
    );

    // Guard the premise. `failures` counts violations, not work done, so this
    // is what separates "every plugin was clean" from "no plugin ran".
    assert!(
        unloadable.len() < sample_plugins().len().max(1),
        "no plugin could be driven, so nothing was checked: {unloadable:?}"
    );
}

/// A playing transport across consecutive blocks must stay spec-clean.
///
/// Reaches `ProcessContextCheck`, which the other tests leave almost untouched:
/// it verifies the context sample rate matches `ProcessSetup` and — the part
/// that needs a *sequence* — that `systemTime` increases monotonically. A host
/// that recomputes system time per block from a wall clock, or forwards a
/// stale value, trips the latter.
#[test]
fn transport_across_blocks_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let mut failures = Vec::new();
    // Every plugin that could not be driven at all. Counted rather than merely
    // printed: `failures` holds only *spec violations*, so a run where nothing
    // loaded leaves it empty and the assertion below passes having checked
    // nothing. That is the same vacuity the skip guards had, one level down.
    let mut unloadable: Vec<String> = Vec::new();

    for (name, path) in sample_plugins() {
        let transport = TransportInfo::default();
        let blocks: Vec<Block<'_>> = (0..8)
            .map(|_| Block::of(512).transport(&transport))
            .collect();

        match drive_blocks::<f32>(&path, 48_000.0, 512, &blocks) {
            Ok(per_block) => {
                for (i, findings) in per_block.iter().enumerate() {
                    let errs = errors(findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} block {i}:\n{}",
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
        "VST3 spec violations with a transport across blocks:\n{}",
        failures.join("\n")
    );

    // Guard the premise. `failures` counts violations, not work done, so this
    // is what separates "every plugin was clean" from "no plugin ran".
    assert!(
        unloadable.len() < sample_plugins().len().max(1),
        "no plugin could be driven, so nothing was checked: {unloadable:?}"
    );
}

/// Note-on / note-off pairs spanning blocks must arrive correctly paired.
///
/// `EventListCheck` tracks live notes across `process` calls by both pitch and
/// note id, so this reaches four checks nothing else does: note-off with no
/// matching note-on (by id, and by pitch), and note-on for a pitch/id already
/// sounding. A host that drops, duplicates, or reorders note events across the
/// block boundary — or mangles the note id — shows up here as a stuck or
/// orphaned note, which is exactly how it would be heard.
#[test]
fn note_lifecycle_across_blocks_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let mut failures = Vec::new();
    // Every plugin that could not be driven at all. Counted rather than merely
    // printed: `failures` holds only *spec violations*, so a run where nothing
    // loaded leaves it empty and the assertion below passes having checked
    // nothing. That is the same vacuity the skip guards had, one level down.
    let mut unloadable: Vec<String> = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        if !accepts_midi(&path) {
            continue;
        }
        exercised += 1;

        // Block 0 starts three notes; block 1 ends them. Nothing is left
        // sounding, and no note-off lacks its note-on.
        let on = [
            (
                at(0),
                midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
            ),
            (
                at(64),
                midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x6000),
            ),
            (
                at(128),
                midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 67, 0x7000),
            ),
        ];
        let off = [
            (
                at(0),
                midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x4000),
            ),
            (
                at(64),
                midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x4000),
            ),
            (
                at(128),
                midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 67, 0x4000),
            ),
        ];
        let blocks = [
            Block::of(512).midi(&on),
            Block::of(512).midi(&off),
            Block::of(512), // silence: nothing should still be sounding
        ];

        match drive_blocks::<f32>(&path, 48_000.0, 512, &blocks) {
            Ok(per_block) => {
                for (i, findings) in per_block.iter().enumerate() {
                    let errs = errors(findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} block {i}:\n{}",
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

    if exercised == 0 {
        eprintln!("no MIDI-capable sample plugin available; note lifecycle not exercised");
    }
    assert!(
        failures.is_empty(),
        "VST3 spec violations in note lifecycle across blocks:\n{}",
        failures.join("\n")
    );

    // Guard the premise. `failures` counts violations, not work done, so this
    // is what separates "every plugin was clean" from "no plugin ran".
    assert!(
        unloadable.len() < sample_plugins().len().max(1),
        "no plugin could be driven, so nothing was checked: {unloadable:?}"
    );
}

/// Parameter automation staged into `inputParameterChanges` must reach the
/// plugin spec-legally.
///
/// This unlocks a whole check category the other tests never reach: with a
/// null `inputParameterChanges` pointer, HostCheck's `ParameterChangesCheck`
/// exits immediately. Populated, it verifies value range (0..=1), that every
/// queue's ParamID is one `IEditController` actually declared, that no ParamID
/// appears twice in one list, that points within a queue are sorted by sample
/// offset, and that no queue pointer is null at a valid index.
///
/// Points are fed deliberately out of order — the host must sort them.
#[test]
fn parameter_changes_are_spec_clean() {
    if !harness_ready() {
        return;
    }
    let mut failures = Vec::new();
    // Every plugin that could not be driven at all. Counted rather than merely
    // printed: `failures` holds only *spec violations*, so a run where nothing
    // loaded leaves it empty and the assertion below passes having checked
    // nothing. That is the same vacuity the skip guards had, one level down.
    let mut unloadable: Vec<String> = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        // Automate the plugin's own first parameter — an invented ParamID
        // would (correctly) be reported as unknown, testing the fixture
        // rather than the host.
        let Some(param_id) = first_parameter_id(&path) else {
            continue;
        };
        let mut params = ParameterChanges::reserved(1, 3);
        let param_id = param_address(param_id);
        params.add_change(param_id, 384, 0.75);
        params.add_change(param_id, 0, 0.25);
        params.add_change(param_id, 128, 0.5);

        exercised += 1;
        match drive_block::<f32>(&path, 512, 48_000.0, 512, &[], Some(&params)) {
            Ok(findings) => {
                let errs = errors(&findings);
                if !errs.is_empty() {
                    failures.push(format!(
                        "{name}:\n{}",
                        errs.iter()
                            .map(|e| format!("    {e}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
            }
            Err(e) => {
                eprintln!("  {name}: skipped ({e})");
                unloadable.push(format!("{name}: {e}"));
            }
        }
    }

    if exercised == 0 {
        eprintln!("no sample plugin exposes a parameter; automation path not exercised");
    }
    assert!(
        failures.is_empty(),
        "VST3 spec violations in the staged parameter changes:\n{}",
        failures.join("\n")
    );

    // Guard the premise. `failures` counts violations, not work done, so this
    // is what separates "every plugin was clean" from "no plugin ran".
    assert!(
        unloadable.len() < sample_plugins().len().max(1),
        "no plugin could be driven, so nothing was checked: {unloadable:?}"
    );
}

/// MIDI staged into the input event list must reach the plugin spec-legally:
/// sorted by sample offset, in-range pitch/velocity, valid bus and channel.
/// Feed events deliberately out of order — the host is required to sort.
#[test]
fn midi_event_list_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let midi = [
        (
            at(200),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        ),
        (
            at(50),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x5000),
        ),
        (
            at(400),
            midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x4000),
        ),
        (
            at(100),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 67, 0x7000),
        ),
    ];
    let mut failures = Vec::new();
    // Every plugin that could not be driven at all. Counted rather than merely
    // printed: `failures` holds only *spec violations*, so a run where nothing
    // loaded leaves it empty and the assertion below passes having checked
    // nothing. That is the same vacuity the skip guards had, one level down.
    let mut unloadable: Vec<String> = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        // Only plugins that declare an event input bus can legally receive
        // MIDI; feeding an audio-only effect a note-on is an invalid call by
        // the *host*, and HostCheck rightly flags the bus index.
        if !accepts_midi(&path) {
            continue;
        }
        exercised += 1;
        match drive_block::<f32>(&path, 512, 48_000.0, 512, &midi, None) {
            Ok(findings) => {
                let errs = errors(&findings);
                if !errs.is_empty() {
                    failures.push(format!(
                        "{name}:\n{}",
                        errs.iter()
                            .map(|e| format!("    {e}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
            }
            Err(e) => {
                eprintln!("  {name}: skipped ({e})");
                unloadable.push(format!("{name}: {e}"));
            }
        }
    }

    if exercised == 0 {
        eprintln!("no MIDI-capable sample plugin available; event-list path not exercised");
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations in the staged event list:\n{}",
        failures.join("\n")
    );

    // Guard the premise. `failures` counts violations, not work done, so this
    // is what separates "every plugin was clean" from "no plugin ran".
    assert!(
        unloadable.len() < sample_plugins().len().max(1),
        "no plugin could be driven, so nothing was checked: {unloadable:?}"
    );
}
