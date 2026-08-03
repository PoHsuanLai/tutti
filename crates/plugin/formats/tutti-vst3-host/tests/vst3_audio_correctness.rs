//! Does this host hand the plugin the **right samples**, in the right slots, at
//! the right times?
//!
//! Every other test in this crate is structural: HostChecker proves the
//! `ProcessData` was *shaped* legally — bus counts agree, no null pointers,
//! events sorted. None of it looks at a single sample value. A host that hands
//! the plugin a perfectly-formed struct full of the wrong audio — swapped
//! channels, an aux bus wired onto the main one, automation applied at the
//! wrong offset — passes all 185 of those checks silently.
//!
//! This suite closes that gap using `audio-probe`, a reference plugin whose
//! output is a closed-form function of its input. Its sources live in-repo
//! under `tests/support/audio-probe/` and `build.rs` compiles them into a real
//! `.vst3` bundle as part of this same `cargo test` run. The test computes the
//! expected samples and compares them exactly.
//!
//! **Exact comparison is deliberate.** A correct host does no arithmetic on the
//! samples it forwards — it passes pointers — so any difference at all is a
//! real routing or timing bug rather than accumulated float error. The one
//! place tolerance is warranted is the automation ramp, where the *host* picks
//! the interpolation; that test says so where it applies.
//!
//! The probe declares a deliberately asymmetric layout — input buses `[2, 1]`,
//! output buses `[2, 1]` — because a host that assumes "one stereo bus in, one
//! stereo bus out" (the shape of every other sample plugin) would otherwise be
//! accidentally correct.
//!
//! ## Running
//!
//! The probe is built from in-repo sources, so only the SDK path is needed:
//!
//! ```bash
//! VST3_SDK_DIR=/path/to/vst3sdk \
//! cargo test -p tutti-vst3-host --features conformance --test vst3_audio_correctness
//! ```
//!
//! Set `VST3_SAMPLE_PLUGIN_DIR` to substitute an externally built probe.

#![cfg(feature = "conformance")]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
use tutti_plugin_types::{ParamAddress, ParamId};
use tutti_vst3_host::{
    AudioBuffer, MidiEvent, NoteExpressionType, NoteExpressionValue, ParameterChanges,
    TransportInfo, Vst3InputEvents, Vst3Instance,
};

/// A VST3 `ParamID` as the automation vocabulary's address.
///
/// The probe-contract constants below stay bare `u32` because they mirror
/// `probeids.h` literally and are also passed to `set_parameter`; only the
/// queue calls need the address form. VST3 ids are `Opaque` — the format hands
/// out numbers whose meaning only the plugin knows.
fn param_address(tag: u32) -> ParamAddress {
    ParamAddress::Opaque(ParamId::new(tag))
}

/// Compile-time default, baked in by `build.rs`; overridable at runtime.
const SAMPLE_PLUGIN_DIR_BUILT: &str = env!("VST3_SAMPLE_PLUGIN_DIR");

fn sample_plugin_dir() -> String {
    std::env::var("VST3_SAMPLE_PLUGIN_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| SAMPLE_PLUGIN_DIR_BUILT.to_string())
}

/// VST3 module lifecycle is not thread-safe across concurrent load/unload of
/// the same DSO; serialise every test that touches a plugin.
static PLUGIN_LOCK: Mutex<()> = Mutex::new(());

fn plugin_guard() -> std::sync::MutexGuard<'static, ()> {
    PLUGIN_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

// ── Probe contract (mirrors audio-probe/source/probeids.h) ───────────────────

const PARAM_MODE: u32 = 100;
const PARAM_RAMP: u32 = 101;
/// Steps on `kParamMode`: 9 modes (0..=8) is 8 steps, and a stepped VST3
/// parameter normalizes as `index / stepCount`. Adding a mode shifts every
/// other mode's normalized value, so this must track `kModeStepCount` exactly —
/// a stale count silently selects the wrong mode rather than failing.
const MODE_STEPS: f64 = 9.0;

/// Normalized value selecting probe mode `index` (see `ProbeMode` in
/// `probeids.h`): 0 tag-passthrough, 1 param-ramp, 2 block-counter,
/// 3 latency, 4 note-gate, 5 event-transcript, 6 event-bus-active,
/// 7 connect-balance, 8 activation-count, 9 audio-bus-active.
fn mode(index: u32) -> f64 {
    f64::from(index) / MODE_STEPS
}

/// Latency the probe reports and applies in `MODE_LATENCY`
/// (`kReportedLatencySamples` in `probeids.h`).
const PROBE_LATENCY_SAMPLES: u32 = 137;

/// What mode 6 writes when the host activated the probe's event input bus
/// (`kEventBusActiveCode` in `probeids.h`).
const EVENT_BUS_ACTIVE_CODE: f32 = 7000.0;

/// Offset mode 7 adds to its connect balance (`kConnectBalanceBase`). Offset
/// rather than raw so a balance of 0 cannot be confused with a zeroed buffer.
const CONNECT_BALANCE_BASE: f32 = 8000.0;

/// Offset mode 8 adds to its activation count (`kActivationCountBase`).
const ACTIVATION_COUNT_BASE: f32 = 9000.0;

/// Offset mode 9 adds to its audio-bus activation mask
/// (`kAudioBusActiveBase`). Bit 0/1 = input bus 0/1, bit 2/3 = output bus 0/1.
const AUDIO_BUS_ACTIVE_BASE: f32 = 10000.0;

/// Writing non-zero here makes the probe's controller request
/// `restartComponent(kIoChanged)` (`kParamRequestIoChanged`).
const PARAM_REQUEST_IO_CHANGED: u32 = 103;

/// Controller-only UI state (`kParamUiState`). The probe's *component* stream
/// does not carry it and `setComponentState` does not touch it — only
/// `IEditController::getState`/`setState` do. That makes it the one value that
/// can tell the two state streams apart.
const PARAM_UI_STATE: u32 = 104;

/// Per-slot DC offset the probe adds in tag-passthrough mode. Must match
/// `probeTag` in `probeids.h` exactly.
fn probe_tag(bus: usize, channel: usize) -> f32 {
    bus as f32 * 1000.0 + channel as f32 + 1.0
}

/// The in-repo probe bundle, built by `build.rs` from
/// `tests/support/audio-probe/`. Empty only if the `conformance` feature is off.
const PROBE_DIR_BUILT: &str = env!("VST3_PROBE_DIR");

/// Locate the probe binary inside a directory holding `audio-probe.vst3`.
fn probe_in(dir: &str) -> Option<PathBuf> {
    if dir.is_empty() {
        return None;
    }
    let bundle = Path::new(dir).join("audio-probe.vst3");
    for sub in [
        "Contents/x86_64-linux",
        "Contents/aarch64-linux",
        "Contents/MacOS",
        "Contents/x86_64-win",
    ] {
        for name in [
            "audio-probe.so",
            "audio-probe",
            "audio-probe.vst3",
            "audio-probe.dylib",
        ] {
            let p = bundle.join(sub).join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Path to the reference probe.
///
/// **Panics rather than returning `None`.** The probe is built from in-repo
/// sources by `build.rs` as part of this very `cargo test` invocation, so its
/// absence is a build failure, not an environmental one. Skipping instead is
/// what let this suite print `ok. 9 passed` while executing nothing at all: the
/// bundle was simply named differently than the lookup expected, and nothing
/// said so.
///
/// `VST3_SAMPLE_PLUGIN_DIR` still wins when it holds a probe, so an externally
/// built one can be substituted deliberately.
fn probe_path() -> PathBuf {
    let external = sample_plugin_dir();
    if let Some(p) = probe_in(&external) {
        return p;
    }
    if let Some(p) = probe_in(PROBE_DIR_BUILT) {
        return p;
    }
    panic!(
        "audio-probe not found. build.rs builds it from tests/support/audio-probe \
         into {PROBE_DIR_BUILT:?} whenever the `conformance` feature is on, so this \
         means the build did not produce it (or VST3_SAMPLE_PLUGIN_DIR={external:?} \
         points somewhere without an audio-probe.vst3)."
    );
}

/// Load and activate the probe.
///
/// Returns `Option` only so call sites keep their existing shape; a load
/// failure is a hard error, because the probe is ours and built from this tree.
fn load_probe(block_size: usize) -> Option<Vst3Instance> {
    let path = probe_path();
    match Vst3Instance::<f32>::load(&path, 48_000.0, block_size) {
        Ok(i) => Some(i),
        Err(e) => panic!("audio-probe failed to load from {path:?}: {e:?}"),
    }
}

/// Put the probe into `mode` and confirm it took effect.
///
/// `set_parameter` writes to the *controller* only — which is correct VST3
/// behaviour: the host is what carries parameter changes across to the
/// processor, in `ProcessData::inputParameterChanges`. So switching a mode
/// needs a real block driven through the audio path, not just a controller
/// write. (Getting this wrong is what made three of these tests fail on their
/// first run: the probe stayed in mode 0 and every assertion saw the constant
/// `probe_tag(0, 0) == 1.0`.)
fn set_mode(inst: &mut Vst3Instance, mode: f64) {
    inst.set_parameter(PARAM_MODE, mode);

    let mut params = ParameterChanges::new();
    params.add_change(param_address(PARAM_MODE), 0, mode);
    // One throwaway block so the processor consumes the change. Its output is
    // rendered in the *old* mode and deliberately discarded.
    render(inst, 64, &[], Some(&params), |_, _, _| 0.0);
}

/// Result of driving one block: the output samples, per bus, per channel.
struct Rendered {
    /// `out[bus][channel][sample]`
    out: Vec<Vec<Vec<f32>>>,
}

/// Drive one block with per-slot distinct input and return what came back.
///
/// Input for bus `b`, channel `c`, sample `i` is `input_at(b, c, i)`, which
/// lets a test make every slot uniquely identifiable.
fn render(
    inst: &mut Vst3Instance,
    frames: usize,
    midi: &[MidiEvent],
    params: Option<&ParameterChanges>,
    input_at: impl Fn(usize, usize, usize) -> f32,
) -> Rendered {
    let info = inst.info().clone();

    // `AudioBuffer` is flat: bus 0's channels first, then bus 1's, etc. Build
    // the flat vectors while remembering the per-bus split so the result can be
    // regrouped.
    let in_layout: Vec<usize> = if info.input_bus_channels.is_empty() {
        vec![info.num_inputs.max(1)]
    } else {
        info.input_bus_channels.clone()
    };
    let out_layout: Vec<usize> = if info.output_bus_channels.is_empty() {
        vec![info.num_outputs.max(1)]
    } else {
        info.output_bus_channels.clone()
    };

    let mut ins: Vec<Vec<f32>> = Vec::new();
    for (bus, &channels) in in_layout.iter().enumerate() {
        for ch in 0..channels {
            ins.push((0..frames).map(|i| input_at(bus, ch, i)).collect());
        }
    }
    let total_out: usize = out_layout.iter().sum();
    let mut outs: Vec<Vec<f32>> = (0..total_out).map(|_| vec![0.0f32; frames]).collect();

    let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();

    let mut buffer = AudioBuffer {
        inputs: &in_refs,
        outputs: &mut out_refs,
        num_samples: frames,
        sample_rate: 48_000.0,
    };
    let events = Vst3InputEvents {
        midi,
        ..Default::default()
    };
    inst.process(&mut buffer, &events, params, &TransportInfo::default());

    // Regroup the flat outputs by bus.
    let mut grouped = Vec::new();
    let mut flat = outs.into_iter();
    for &channels in &out_layout {
        let mut bus = Vec::new();
        for _ in 0..channels {
            bus.push(flat.next().unwrap_or_default());
        }
        grouped.push(bus);
    }
    Rendered { out: grouped }
}

/// Kept as a no-op so every test still names its precondition at the top.
///
/// It used to decide whether to skip; [`probe_path`] now panics instead, because
/// the probe is built from this tree rather than found on the machine. Returning
/// a constant `true` keeps the call sites honest without reintroducing a path
/// where a test reports success having run nothing.
fn harness_ready() -> bool {
    // Resolve eagerly: this is what turns a missing probe into a failure at the
    // start of the test rather than a confusing error part-way through.
    let _ = probe_path();
    true
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Every (bus, channel) slot must carry the samples the host claims it does.
///
/// The probe writes `in[bus][ch][i] + tag(bus, ch)`, and the test feeds a
/// distinct value per slot. Any crossed channel, swapped bus, or aux bus routed
/// onto the main one produces arithmetically wrong samples — the single
/// highest-value assertion in this file, because this bug class is completely
/// invisible to structural checking.
#[test]
fn every_bus_and_channel_carries_its_own_audio() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();
    let Some(mut inst) = load_probe(512) else {
        return;
    };
    set_mode(&mut inst, mode(0));

    const FRAMES: usize = 128;
    // A value unique to each (bus, channel, sample).
    let input_at = |bus: usize, ch: usize, i: usize| -> f32 {
        (bus as f32) * 100.0 + (ch as f32) * 10.0 + (i as f32) * 0.001
    };

    let rendered = render(&mut inst, FRAMES, &[], None, input_at);

    let info = inst.info().clone();
    let out_layout = if info.output_bus_channels.is_empty() {
        vec![info.num_outputs.max(1)]
    } else {
        info.output_bus_channels.clone()
    };
    let in_layout = if info.input_bus_channels.is_empty() {
        vec![info.num_inputs.max(1)]
    } else {
        info.input_bus_channels.clone()
    };

    let mut mismatches = Vec::new();
    for (bus, &channels) in out_layout.iter().enumerate() {
        for ch in 0..channels {
            // The probe pairs input and output slots by index, and substitutes
            // silence where the host supplied no matching input.
            let has_input = bus < in_layout.len() && ch < in_layout[bus];
            for i in 0..FRAMES {
                let src = if has_input { input_at(bus, ch, i) } else { 0.0 };
                let expected = src + probe_tag(bus, ch);
                let actual = rendered.out[bus][ch][i];
                if actual != expected {
                    mismatches.push(format!(
                        "bus {bus} ch {ch} sample {i}: expected {expected}, got {actual}"
                    ));
                    if mismatches.len() >= 8 {
                        break;
                    }
                }
            }
        }
    }

    assert!(
        mismatches.is_empty(),
        "audio landed in the wrong slot — the host is misrouting buses or \
         channels:\n  {}",
        mismatches.join("\n  ")
    );
}

/// Parameter automation must reach the plugin at the sample offsets the host
/// was given.
///
/// The probe renders the automation curve *as audio*, consuming points in the
/// order the host delivered them without sorting. So a host that forwards
/// points out of order produces an audibly inverted ramp — which is exactly the
/// damage the earlier `refill_from_queue` sorting bug could do, expressed as
/// wrong samples rather than as a spec violation.
///
/// Compared with a small epsilon: unlike the routing test, the values here go
/// through the host's own normalisation on the way in.
#[test]
fn automation_ramp_is_rendered_at_the_right_offsets() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();
    let Some(mut inst) = load_probe(512) else {
        return;
    };
    set_mode(&mut inst, mode(1));

    const FRAMES: usize = 512;
    // Deliberately out of order: the host must sort before delivery.
    let mut params = ParameterChanges::new();
    let ramp = param_address(PARAM_RAMP);
    params.add_change(ramp, 384, 1.0);
    params.add_change(ramp, 0, 0.0);
    params.add_change(ramp, 128, 0.5);

    let rendered = render(&mut inst, FRAMES, &[], Some(&params), |_, _, _| 0.0);
    let ch0 = &rendered.out[0][0];

    // The ramp must be non-decreasing across the block. An unsorted delivery
    // makes it jump backwards, which this catches without depending on the
    // exact interpolation.
    let mut regressions = Vec::new();
    for i in 1..FRAMES {
        if ch0[i] < ch0[i - 1] - 1e-6 {
            regressions.push(format!("sample {i}: {} < previous {}", ch0[i], ch0[i - 1]));
            if regressions.len() >= 5 {
                break;
            }
        }
    }
    assert!(
        regressions.is_empty(),
        "automation ramp runs backwards — parameter points reached the plugin \
         out of order:\n  {}",
        regressions.join("\n  ")
    );

    // And it must actually span the range requested, not sit flat: a host that
    // drops all but one point would otherwise pass the monotonicity check.
    let first = ch0[0];
    let last = ch0[FRAMES - 1];
    assert!(
        (last - first).abs() > 0.25,
        "automation barely moved across the block (first {first}, last {last}) \
         — points were probably dropped"
    );
}

/// Consecutive blocks must be delivered exactly once each, in order.
///
/// The probe emits a strictly increasing clock (`blockIndex + i/frames`), so a
/// duplicated, dropped, or reordered block shows up as a discontinuity the test
/// can locate precisely.
#[test]
fn consecutive_blocks_are_delivered_in_order() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();
    let Some(mut inst) = load_probe(512) else {
        return;
    };
    set_mode(&mut inst, mode(2));

    const FRAMES: usize = 64;
    const BLOCKS: usize = 16;

    // `set_mode` drove a block of its own, so the clock does not start at 0.
    // Only monotonicity matters here, so seed from whatever it reads now.
    let mut last_seen = f32::NEG_INFINITY;
    let mut faults = Vec::new();
    for block in 0..BLOCKS {
        let rendered = render(&mut inst, FRAMES, &[], None, |_, _, _| 0.0);
        let ch0 = &rendered.out[0][0];
        for (i, &v) in ch0.iter().enumerate() {
            if v <= last_seen {
                faults.push(format!(
                    "block {block} sample {i}: clock went {v} after {last_seen}"
                ));
                if faults.len() >= 5 {
                    break;
                }
            }
            last_seen = v;
        }
    }

    assert!(
        faults.is_empty(),
        "the plugin's block clock is not strictly increasing — blocks were \
         duplicated, dropped, or reordered:\n  {}",
        faults.join("\n  ")
    );
}

/// `kIoChanged` runs a real deactivate/reactivate cycle, not an in-place re-read.
///
/// `ivsteditcontroller.h:125-127`: *"The host has to deactivate the plug-in,
/// asks the plug-in for its wanted new bus configurations, adapts its
/// processing graph and reactivate the plug-in."* Only the middle third was
/// done — `reconcile_bus_counts()` re-read the layout with the plugin still
/// active, which is the one ordering the spec rules out.
///
/// Bus counts cannot show this: this probe's layout does not actually change,
/// and a plugin whose layout *would* change is exactly the one that answers
/// wrongly while active. What separates the two is whether the plugin was
/// reactivated at all, so `kModeActivationCount` reports its own
/// `setActive(true)` count.
///
/// Driven through `Vst3Instance` rather than the server: this is the layer that
/// owns activation, and the restart method lives here.
#[test]
fn an_io_change_reactivates_the_plugin_rather_than_re_reading_it_live() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();
    let Some(mut inst) = load_probe(512) else {
        return;
    };
    set_mode(&mut inst, mode(8));

    let activations_before = render(&mut inst, 512, &[], None, |_, _, _| 0.0).out[0][0][0];
    assert_eq!(
        activations_before,
        ACTIVATION_COUNT_BASE + 1.0,
        "a freshly loaded instance should have been activated exactly once"
    );

    // Ask the plugin to request the restart, then drain the notification the
    // way a host's poll loop does.
    inst.set_parameter(PARAM_REQUEST_IO_CHANGED, 1.0);
    let notifications = inst.poll_plugin_notifications();
    assert!(
        notifications.restart.io_changed,
        "the probe requested restartComponent(kIoChanged) but it did not reach \
         the host — the rest of this test would be vacuous"
    );

    inst.restart_bus_configuration()
        .expect("the probe accepts reactivation");

    // The mode parameter survives the cycle on the controller side, but the
    // processor's copy was re-read from a fresh activation; re-assert it so the
    // render below is definitely in mode 8.
    set_mode(&mut inst, mode(8));
    let activations_after = render(&mut inst, 512, &[], None, |_, _, _| 0.0).out[0][0][0];

    assert_eq!(
        activations_after,
        ACTIVATION_COUNT_BASE + 2.0,
        "the plugin was never reactivated — the host re-read its bus layout in \
         place while it was still active, which is the ordering kIoChanged \
         exists to prevent"
    );
}

/// A half-refused connection is unwound, not left dangling.
///
/// Wiring a separate controller takes two `connect` calls, and a plugin may
/// take the first and refuse the second. Both returns were discarded, so the
/// component was left holding a peer that never reciprocated while
/// `initialize()` carried on — a state no legal call sequence produces.
///
/// The asymmetry is invisible from the host: the SDK base class keeps only the
/// current peer pointer, so "never connected" and "connected then unwound" look
/// identical, and a dangling connect causes no immediate error. Hence
/// `kModeConnectBalance`, which reports the processor half's own
/// connect-minus-disconnect count. Only that count separates the two.
#[test]
fn a_half_refused_connection_is_unwound_on_the_component_half() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();

    // `TUTTI_PROBE_MISBEHAVIOUR` is process-global and the probe reads it at
    // `connect` time. `plugin_guard` serialises this file, and the probe is
    // loaded and dropped entirely inside this scope, so the window is closed
    // before any other test can load it. Restored unconditionally below.
    let previous = std::env::var("TUTTI_PROBE_MISBEHAVIOUR").ok();
    std::env::set_var("TUTTI_PROBE_MISBEHAVIOUR", "10");

    let balance = {
        let Some(mut inst) = load_probe(512) else {
            match previous {
                Some(v) => std::env::set_var("TUTTI_PROBE_MISBEHAVIOUR", v),
                None => std::env::remove_var("TUTTI_PROBE_MISBEHAVIOUR"),
            }
            return;
        };
        set_mode(&mut inst, mode(7));
        let rendered = render(&mut inst, 512, &[], None, |_, _, _| 0.0);
        rendered.out[0][0][0]
    };

    match previous {
        Some(v) => std::env::set_var("TUTTI_PROBE_MISBEHAVIOUR", v),
        None => std::env::remove_var("TUTTI_PROBE_MISBEHAVIOUR"),
    }

    // `kConnectBalanceBase + balance`. 0 means the component was never left
    // joined — either the connect never happened or it was unwound. 1 means it
    // is still holding the peer the controller refused.
    assert_eq!(
        balance, CONNECT_BALANCE_BASE,
        "the controller refused its half of the connection, so the component's \
         half must be unwound; a balance of +1 means it is still wired to a \
         peer that never reciprocated"
    );
}

/// The host must activate a plugin's event bus, not merely count it.
///
/// `ivstcomponent.h:52` says "All busses are initially inactive" without
/// qualification, and `kEvent` is a `MediaTypes` value beside `kAudio`, so an
/// event bus needs the same `activateBus` call an audio bus does. The host
/// used to enumerate event buses only to decide whether the plugin spoke MIDI,
/// and activate none of them.
///
/// Every other MIDI test here passes with or without that call, which is why
/// this one exists. Steinberg's samples and this probe's other modes all read
/// `data.inputEvents` regardless of bus state — the lenient behaviour most
/// real plugins have — so the omission is invisible from the audio. It is also
/// invisible from the host: `BusInfo` has no active field, so a host cannot
/// read its own activation back, and `HostChecker` validates the `ProcessData`
/// handed over rather than the lifecycle before it. Only the plugin knows,
/// which is why `kModeEventBusActive` asks it directly.
#[test]
fn event_buses_are_activated_not_merely_counted() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();
    let Some(mut inst) = load_probe(512) else {
        return;
    };
    set_mode(&mut inst, mode(6));

    const FRAMES: usize = 512;
    let rendered = render(&mut inst, FRAMES, &[], None, |_, _, _| 0.0);
    let ch0 = &rendered.out[0][0];

    // The probe fills the whole block with one code, so any sample answers —
    // but check them all, since a partial fill would mean something else is
    // wrong with the render path.
    assert!(
        ch0.iter().all(|&v| v == ch0[0]),
        "the probe should fill the block with one code; got a varying buffer"
    );

    assert_eq!(
        ch0[0], EVENT_BUS_ACTIVE_CODE,
        "the probe reports its event input bus was left inactive — a plugin \
         that honours the spec's inactive default receives no MIDI"
    );
}

/// A note-on must take effect at exactly the sample offset it carried.
///
/// The probe gates its output on at the note's `sampleOffset`. Measuring where
/// the edge actually lands turns MIDI timing into a sample index — far stronger
/// than asserting the event list merely looked well-formed.
#[test]
fn note_on_takes_effect_at_its_sample_offset() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();
    let Some(mut inst) = load_probe(512) else {
        return;
    };
    set_mode(&mut inst, mode(4));

    const FRAMES: usize = 512;
    let mut failures = Vec::new();

    for &offset in &[0usize, 1, 63, 128, 511] {
        let midi = [
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(offset as u32),
        ];
        let rendered = render(&mut inst, FRAMES, &midi, None, |_, _, _| 0.0);
        let ch0 = &rendered.out[0][0];

        // Locate the rising edge.
        let edge = ch0.iter().position(|&v| v > 0.5);
        match edge {
            Some(found) if found == offset => {}
            Some(found) => failures.push(format!(
                "note-on at offset {offset} produced its edge at sample {found}"
            )),
            None => failures.push(format!(
                "note-on at offset {offset} produced no edge at all — the event \
                 never reached the plugin"
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "MIDI events did not take effect at the offsets they carried:\n  {}",
        failures.join("\n  ")
    );
}

/// The f64 path must carry the same audio as f32.
///
/// `symbolicSampleSize` and the `channelBuffers32`/`64` union arm must agree:
/// they overlay the same bytes, so a host that sets one without the other hands
/// the plugin a pointer table read at the wrong width — silent garbage, not a
/// crash. Structural checks only verify the *declaration* is consistent.
#[test]
fn f64_path_carries_the_same_audio() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();
    let path = probe_path();
    // The probe declares f64 support, so a failure here is a real one — either
    // in the probe or in the host's f64 activation path. Skipping would hide
    // exactly the regression this test exists to catch.
    let mut inst = match Vst3Instance::<f64>::load(&path, 48_000.0, 512) {
        Ok(i) => i,
        Err(e) => panic!("audio-probe f64 activation failed: {e:?}"),
    };
    inst.set_parameter(PARAM_MODE, mode(0));

    const FRAMES: usize = 64;
    let info = inst.info().clone();
    let in_layout = if info.input_bus_channels.is_empty() {
        vec![info.num_inputs.max(1)]
    } else {
        info.input_bus_channels.clone()
    };
    let out_layout = if info.output_bus_channels.is_empty() {
        vec![info.num_outputs.max(1)]
    } else {
        info.output_bus_channels.clone()
    };

    let input_at = |bus: usize, ch: usize, i: usize| -> f64 {
        (bus as f64) * 100.0 + (ch as f64) * 10.0 + (i as f64) * 0.001
    };

    let mut ins: Vec<Vec<f64>> = Vec::new();
    for (bus, &channels) in in_layout.iter().enumerate() {
        for ch in 0..channels {
            ins.push((0..FRAMES).map(|i| input_at(bus, ch, i)).collect());
        }
    }
    let total_out: usize = out_layout.iter().sum();
    let mut outs: Vec<Vec<f64>> = (0..total_out).map(|_| vec![0.0f64; FRAMES]).collect();
    let in_refs: Vec<&[f64]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut out_refs: Vec<&mut [f64]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();

    let mut buffer = AudioBuffer {
        inputs: &in_refs,
        outputs: &mut out_refs,
        num_samples: FRAMES,
        sample_rate: 48_000.0,
    };
    inst.process(
        &mut buffer,
        &Vst3InputEvents::default(),
        None,
        &TransportInfo::default(),
    );

    let mut flat = outs.into_iter();
    let mut mismatches = Vec::new();
    for (bus, &channels) in out_layout.iter().enumerate() {
        for ch in 0..channels {
            let got = flat.next().unwrap_or_default();
            let has_input = bus < in_layout.len() && ch < in_layout[bus];
            for (i, &actual) in got.iter().enumerate().take(FRAMES) {
                let src = if has_input { input_at(bus, ch, i) } else { 0.0 };
                let expected = src + f64::from(probe_tag(bus, ch));
                if actual != expected {
                    mismatches.push(format!(
                        "bus {bus} ch {ch} sample {i}: expected {expected}, got {actual}",
                    ));
                    if mismatches.len() >= 8 {
                        break;
                    }
                }
            }
        }
    }

    assert!(
        mismatches.is_empty(),
        "f64 audio landed wrong — symbolicSampleSize and the channel-buffer \
         union arm may disagree:\n  {}",
        mismatches.join("\n  ")
    );
}

/// The full MIDI event list must reach the plugin intact — every event, with
/// its kind, pitch and offset preserved, and none merged or dropped.
///
/// `note_on_takes_effect_at_its_sample_offset` only finds the *first* rising
/// edge, so it cannot see a dropped note-off, a mangled pitch, or two events
/// collapsed onto one offset. The transcript mode encodes note-on as
/// `+(pitch + 1)` and note-off as `-(pitch + 1)` at the event's own sample, so
/// all of that is recoverable from the audio. A stuck note in a real session
/// is precisely a dropped note-off.
#[test]
fn full_midi_event_list_survives_intact() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();
    let Some(mut inst) = load_probe(512) else {
        return;
    };
    set_mode(&mut inst, mode(5));

    const FRAMES: usize = 512;
    // Deliberately out of order, with two events sharing offset 256 — the host
    // must sort without merging, and must not drop the note-offs.
    let midi = [
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x4000)
            .with_frame_offset(300),
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000).with_frame_offset(0),
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x6000).with_frame_offset(100),
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x4000)
            .with_frame_offset(256),
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 72, 0x7000).with_frame_offset(256),
    ];

    let rendered = render(&mut inst, FRAMES, &midi, None, |_, _, _| 0.0);
    let ch0 = &rendered.out[0][0];

    // (offset, expected code). Offset 256 carries note-off 60 and note-on 72,
    // which the probe accumulates: -(60+1) + (72+1) = 12.
    let expected: &[(usize, f32)] = &[
        (0, 61.0),    // note-on 60
        (100, 65.0),  // note-on 64
        (256, 12.0),  // note-off 60 + note-on 72
        (300, -65.0), // note-off 64
    ];

    let mut failures = Vec::new();
    for &(offset, code) in expected {
        if ch0[offset] != code {
            failures.push(format!(
                "sample {offset}: expected code {code}, got {}",
                ch0[offset]
            ));
        }
    }
    // Everything else must be silent: a spurious or mistimed event shows up as
    // a nonzero sample where none was sent.
    let marked: Vec<usize> = expected.iter().map(|&(o, _)| o).collect();
    for (i, &v) in ch0.iter().enumerate() {
        if !marked.contains(&i) && v != 0.0 {
            failures.push(format!("unexpected event code {v} at sample {i}"));
            if failures.len() >= 8 {
                break;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "the MIDI event list did not survive the trip intact:\n  {}",
        failures.join("\n  ")
    );
}

/// A main bus is activated whatever its flags say; an aux bus only when it
/// asks.
///
/// The probe declares main + aux in both directions. Both aux buses carry an
/// explicit `flags = 0`, so they are not `kDefaultActive` and stay inactive.
///
/// Its main *output* also carries `flags = 0`, which no SDK sample does — that
/// is the plugin bug the main-bus rule exists for. It makes this test
/// distinguish the two policies that matter: honouring the flag strictly would
/// leave that bus inactive and render silence, and every corpus plugin flags
/// its main buses, so nothing else can tell the two apart.
///
/// Expected mask is main-in | main-out — the second one activated *despite*
/// its flags, not because of them.
///
/// **Nothing else in this suite can see the difference.** `BusInfo` carries no
/// active field, so the host cannot read its own decision back, and the probe
/// writes its aux output whether or not the bus was activated — exactly as a
/// lenient real plugin does, which is why `every_bus_and_channel_carries_its_own_audio`
/// passes identically under either policy. Only the plugin knows, so mode 9
/// asks it.
#[test]
fn only_main_and_default_active_buses_are_activated() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();
    let Some(mut inst) = load_probe(512) else {
        return;
    };
    set_mode(&mut inst, mode(9));

    const FRAMES: usize = 512;
    let rendered = render(&mut inst, FRAMES, &[], None, |_, _, _| 0.0);
    let ch0 = &rendered.out[0][0];

    assert!(
        ch0.iter().all(|&v| v == ch0[0]),
        "the probe should fill the block with one code; got a varying buffer"
    );

    // bit 0 = input 0 (main), bit 2 = output 0 (main). Buses 1 in each
    // direction are aux with no kDefaultActive, so they stay off.
    const EXPECTED_MASK: f32 = 0b0101 as f32;
    let mask = ch0[0] - AUDIO_BUS_ACTIVE_BASE;

    assert_eq!(
        mask, EXPECTED_MASK,
        "expected only the two main buses active (mask {EXPECTED_MASK}), got \
         mask {mask}. Bits from 0 are: main-in, aux-in, main-out, aux-out. \
         Mask 15 means the host still activates every bus regardless of \
         kDefaultActive; mask 1 means it honours the flag strictly and has \
         left the probe's unflagged main output inactive, which is silence."
    );
}

/// Controller-only UI state must survive a save/restore.
///
/// The spec gives `IEditController` its own `getState`/`setState` pair, distinct
/// from the component's, holding what only the UI knows — scroll position, the
/// selected tab, a meter's display mode. A host that saves only the component
/// stream silently discards all of it, and the loss is invisible from the
/// component stream alone: it round-trips perfectly while the editor reopens at
/// its defaults.
///
/// `kParamUiState` is reachable *only* through the controller's own stream —
/// the probe's `setComponentState` deliberately leaves it alone — so the value
/// arriving in a second, freshly loaded instance can only have travelled
/// through `IEditController::getState`. Restoring into a fresh instance rather
/// than the same one is what makes this a test of the blob rather than of
/// memory that was never cleared.
#[test]
fn controller_only_ui_state_survives_a_save_and_restore() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();

    // A value no default could be confused with, and exactly representable in
    // f64 so the comparison needs no epsilon.
    const UI_VALUE: f64 = 0.625;

    let saved = {
        let Some(mut inst) = load_probe(512) else {
            return;
        };
        inst.set_parameter(PARAM_UI_STATE, UI_VALUE);
        assert_eq!(
            inst.parameter(PARAM_UI_STATE),
            UI_VALUE,
            "the probe did not accept the UI-state write, so the rest of this \
             test would be vacuous"
        );
        inst.state().expect("probe should expose state")
    };

    let Some(mut restored) = load_probe(512) else {
        return;
    };
    assert_eq!(
        restored.parameter(PARAM_UI_STATE),
        0.0,
        "a freshly loaded probe should start at the UI-state default; if it \
         does not, the restore below proves nothing"
    );

    restored.set_state(&saved).expect("restore should succeed");

    assert_eq!(
        restored.parameter(PARAM_UI_STATE),
        UI_VALUE,
        "controller-only UI state was lost across save/restore — the host is \
         persisting IComponent's stream but not IEditController's, so a \
         reopened editor shows defaults"
    );
}

/// The host must report the plugin's latency, so an embedder can compensate.
///
/// Delay compensation itself lives in `tutti-core` (`LatencyGraph` /
/// `Compensation` / `PdcDelay` — explicit and opt-in over the whole graph), not
/// in this crate. What *this* crate owns is the number PDC is fed, so that is
/// what this asserts: the reported figure equals what the plugin declares, and
/// the plugin's output really is delayed by that much. A stale zero would pass
/// the first check alone and silently misalign every compensated graph.
#[test]
fn reported_latency_matches_the_plugins_actual_delay() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();
    let Some(mut inst) = load_probe(512) else {
        return;
    };
    set_mode(&mut inst, mode(3));

    // The probe only reports latency while in latency mode, so this must be
    // read after the mode switch.
    let reported = inst.read_latency_samples();
    assert_eq!(
        reported, PROBE_LATENCY_SAMPLES,
        "host reported {reported} samples of latency; the plugin declares \
         {PROBE_LATENCY_SAMPLES}"
    );

    // Send an impulse at sample 0 and find where it emerges. The probe's delay
    // line spans one latency period, so a block longer than that sees it.
    const FRAMES: usize = 512;
    let rendered = render(
        &mut inst,
        FRAMES,
        &[],
        None,
        |_, _, i| {
            if i == 0 {
                1.0
            } else {
                0.0
            }
        },
    );
    let ch0 = &rendered.out[0][0];

    let found = ch0.iter().position(|&v| v != 0.0);
    assert_eq!(
        found,
        Some(PROBE_LATENCY_SAMPLES as usize),
        "the plugin declares {PROBE_LATENCY_SAMPLES} samples of latency, but its \
         impulse emerged at {found:?} — the reported figure does not describe \
         the actual delay, so compensating by it would misalign the audio"
    );
}

/// Note-expression events must reach the plugin with their type *and* value.
///
/// A host that forwards the event but loses the value — or maps the wrong
/// `typeId` — produces per-note modulation that is silently inert or applied to
/// the wrong dimension. The transcript encodes
/// `kNoteExpressionBaseCode + typeId + value`, so both survive or neither does.
#[test]
fn note_expression_reaches_the_plugin_with_its_value() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();
    let Some(mut inst) = load_probe(512) else {
        return;
    };
    set_mode(&mut inst, mode(5));

    const FRAMES: usize = 512;
    const NOTE_EXPR_BASE: f32 = 5000.0;

    // A note to attach the expressions to, then two expressions of different
    // types and values at distinct offsets.
    let midi = [
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000).with_frame_offset(0),
    ];
    let expressions = [
        NoteExpressionValue {
            sample_offset: 64,
            note_id: -1,
            expression_type: NoteExpressionType::Volume,
            value: 0.25,
        },
        NoteExpressionValue {
            sample_offset: 192,
            note_id: -1,
            expression_type: NoteExpressionType::Pan,
            value: 0.75,
        },
    ];

    let info = inst.info().clone();
    let in_layout = if info.input_bus_channels.is_empty() {
        vec![info.num_inputs.max(1)]
    } else {
        info.input_bus_channels.clone()
    };
    let out_layout = if info.output_bus_channels.is_empty() {
        vec![info.num_outputs.max(1)]
    } else {
        info.output_bus_channels.clone()
    };
    let mut ins: Vec<Vec<f32>> = Vec::new();
    for &channels in &in_layout {
        for _ in 0..channels {
            ins.push(vec![0.0f32; FRAMES]);
        }
    }
    let total_out: usize = out_layout.iter().sum();
    let mut outs: Vec<Vec<f32>> = (0..total_out).map(|_| vec![0.0f32; FRAMES]).collect();
    let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
    let mut buffer = AudioBuffer {
        inputs: &in_refs,
        outputs: &mut out_refs,
        num_samples: FRAMES,
        sample_rate: 48_000.0,
    };
    let events = Vst3InputEvents {
        midi: &midi,
        note_expressions: &expressions,
        ..Default::default()
    };
    inst.process(&mut buffer, &events, None, &TransportInfo::default());
    let ch0 = &outs[0];

    // VST3 typeIds: Volume = 0, Pan = 1.
    let mut failures = Vec::new();
    for (offset, type_id, value) in [(64usize, 0.0f32, 0.25f32), (192, 1.0, 0.75)] {
        let expected = NOTE_EXPR_BASE + type_id + value;
        if (ch0[offset] - expected).abs() > 1e-4 {
            failures.push(format!(
                "sample {offset}: expected {expected} (base + typeId {type_id} + \
                 value {value}), got {}",
                ch0[offset]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "note-expression events lost their type or value in transit:\n  {}",
        failures.join("\n  ")
    );
}

/// MIDI the plugin *emits* must reach the host.
///
/// The reverse direction of every other MIDI test here. `legacy-midicc-out`
/// emits a legacy MIDI CC from its `process` when its controller parameter
/// changes; the host has to decode that from `outputEvents` and surface it as a
/// `MidiEvent`. A host that never drains the output list silently drops
/// everything an arpeggiator or MIDI-effect plugin produces.
#[test]
fn midi_emitted_by_the_plugin_reaches_the_host() {
    if !harness_ready() {
        return;
    }
    let _guard = plugin_guard();

    let dir = sample_plugin_dir();
    let bundle = Path::new(&dir).join("legacy-midicc-out.vst3");
    let mut path = None;
    for sub in ["Contents/x86_64-linux", "Contents/MacOS"] {
        for name in ["legacy-midicc-out.so", "legacy-midicc-out"] {
            let p = bundle.join(sub).join(name);
            if p.is_file() {
                path = Some(p);
            }
        }
    }
    let Some(path) = path else {
        eprintln!("legacy-midicc-out not built; skipping MIDI-output test");
        return;
    };
    let Ok(mut inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("legacy-midicc-out load failed; skipping");
        return;
    };

    // Drive blocks while sweeping the plugin's parameters: it emits CC when a
    // parameter changes, so a static value produces nothing.
    let info = inst.info().clone();
    let param_ids: Vec<u32> = (0..inst.parameter_count())
        .filter_map(|i| inst.parameter_id_at(i))
        .collect();

    let mut emitted = 0usize;
    for step in 0..16 {
        let mut params = ParameterChanges::new();
        for &id in &param_ids {
            params.add_change(param_address(id), 0, (step as f64 * 0.0625).min(1.0));
        }

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
        let out = inst.process(
            &mut buffer,
            &Vst3InputEvents::default(),
            Some(&params),
            &TransportInfo::default(),
        );
        emitted += out.midi_events.len();
    }

    assert!(
        emitted > 0,
        "legacy-midicc-out emits MIDI CC on parameter change, but the host \
         surfaced none across 16 blocks — plugin-emitted MIDI is being dropped"
    );
    eprintln!("plugin-emitted MIDI events surfaced: {emitted}");
}
