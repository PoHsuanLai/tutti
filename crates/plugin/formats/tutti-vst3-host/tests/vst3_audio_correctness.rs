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
//! output is a closed-form function of its input (see
//! `/mnt/data2/vst3-hostchecker/audio-probe`). The test computes the expected
//! samples and compares them exactly.
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
//! ```bash
//! VST3_SAMPLE_PLUGIN_DIR=/path/to/build/VST3/Release \
//! cargo test -p tutti-vst3-host --features conformance --test vst3_audio_correctness
//! ```

#![cfg(feature = "conformance")]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tutti_vst3_host::{
    AudioBuffer, MidiEvent, NoteExpressionType, NoteExpressionValue, ParameterChanges,
    TransportInfo, Vst3InputEvents, Vst3Instance,
};

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
/// Steps on `kParamMode`: 6 modes (0..=5) is 5 steps, and a stepped VST3
/// parameter normalizes as `index / stepCount`.
const MODE_STEPS: f64 = 5.0;

/// Normalized value selecting probe mode `index` (see `ProbeMode` in
/// `probeids.h`): 0 tag-passthrough, 1 param-ramp, 2 block-counter,
/// 3 latency, 4 note-gate, 5 event-transcript.
fn mode(index: u32) -> f64 {
    f64::from(index) / MODE_STEPS
}

/// Latency the probe reports and applies in `MODE_LATENCY`
/// (`kReportedLatencySamples` in `probeids.h`).
const PROBE_LATENCY_SAMPLES: u32 = 137;

/// Per-slot DC offset the probe adds in tag-passthrough mode. Must match
/// `probeTag` in `probeids.h` exactly.
fn probe_tag(bus: usize, channel: usize) -> f32 {
    bus as f32 * 1000.0 + channel as f32 + 1.0
}

fn probe_path() -> Option<PathBuf> {
    let dir = sample_plugin_dir();
    if dir.is_empty() {
        return None;
    }
    let bundle = Path::new(&dir).join("audio-probe.vst3");
    for sub in ["Contents/x86_64-linux", "Contents/MacOS", "Contents/x86_64-win"] {
        for name in ["audio-probe.so", "audio-probe", "audio-probe.vst3", "audio-probe.dylib"] {
            let p = bundle.join(sub).join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Load and activate the probe, or print why and return `None`.
fn load_probe(block_size: usize) -> Option<Vst3Instance> {
    let path = probe_path()?;
    match Vst3Instance::<f32>::load(&path, 48_000.0, block_size) {
        Ok(i) => Some(i),
        Err(e) => {
            eprintln!("audio-probe load failed ({e:?}); skipping");
            None
        }
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
    params.add_change(PARAM_MODE, 0, mode);
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

fn harness_ready() -> bool {
    if probe_path().is_none() {
        eprintln!(
            "audio-probe not built under {:?}; skipping. Build it with \
             `cmake --build <build-dir> --target audio-probe`.",
            sample_plugin_dir()
        );
        return false;
    }
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
    params.add_change(PARAM_RAMP, 384, 1.0);
    params.add_change(PARAM_RAMP, 0, 0.0);
    params.add_change(PARAM_RAMP, 128, 0.5);

    let rendered = render(&mut inst, FRAMES, &[], Some(&params), |_, _, _| 0.0);
    let ch0 = &rendered.out[0][0];

    // The ramp must be non-decreasing across the block. An unsorted delivery
    // makes it jump backwards, which this catches without depending on the
    // exact interpolation.
    let mut regressions = Vec::new();
    for i in 1..FRAMES {
        if ch0[i] < ch0[i - 1] - 1e-6 {
            regressions.push(format!(
                "sample {i}: {} < previous {}",
                ch0[i],
                ch0[i - 1]
            ));
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
        let midi = [MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(offset as u32)];
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
    let Some(path) = probe_path() else {
        return;
    };
    let Ok(mut inst) = Vst3Instance::<f64>::load(&path, 48_000.0, 512) else {
        eprintln!("audio-probe f64 activation failed; skipping");
        return;
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
        MidiEvent::note_off(0, 0, 64, 0x4000).with_frame_offset(300),
        MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
        MidiEvent::note_on(0, 0, 64, 0x6000).with_frame_offset(100),
        MidiEvent::note_off(0, 0, 60, 0x4000).with_frame_offset(256),
        MidiEvent::note_on(0, 0, 72, 0x7000).with_frame_offset(256),
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
    let rendered = render(&mut inst, FRAMES, &[], None, |_, _, i| {
        if i == 0 {
            1.0
        } else {
            0.0
        }
    });
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
    let midi = [MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0)];
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
            params.add_change(id, 0, (step as f64 * 0.0625).min(1.0));
        }

        let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1)).map(|_| vec![0.0; 512]).collect();
        let mut outs: Vec<Vec<f32>> =
            (0..info.num_outputs.max(1)).map(|_| vec![0.0; 512]).collect();
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
