//! RT-safety regression: `Vst3Active::process` must not allocate on the
//! audio thread in steady state.
//!
//! The plugin is `audio-probe`, which allocates nothing in `process`, so an
//! allocation in the guarded region is the host's. The binary's global
//! allocator is `AllocDisabler` (in `main.rs`) and an allocation in the guarded
//! region **aborts the process**, so nextest (one process per test) reports an
//! abort as the failure of that case.
//! `audio-probe` is built by `build.rs` on the `conformance` feature, which
//! this crate's dev-dependency on itself turns on; a missing plugin is a panic
//! naming the build command, never a skip.
//!
//! Mirrors the pattern in `tutti-plugin-host/tests/vst2_in_process_no_alloc.rs`
//! and `clap-host/tests/clap_process_no_alloc.rs`.
//!
//! ## What each test is actually guarding
//!
//! The buffer plumbing is a fixed stereo pair so all per-iteration setup stays
//! on the stack; only the *host* is under test, never the harness.
//!
//! Note that the silent and MIDI cases pass `None` for parameter changes, so
//! neither reaches the automation path — they were measured passing with a
//! deliberate `vec![]` inserted into `ParamValueQueueImpl::refill_from_queue`.
//! [`process_with_automation_does_not_allocate`] is what covers that path, and
//! it catches the same `vec![]`.

use std::path::Path;
use tutti_graph::UmpAt;

use tutti_graph::Offset;
use tutti_types::Samples;
use tutti_ump::MidiChannel;
use tutti_vst3_host::{AudioBuffer, InputEvents, ParameterChanges, TransportInfo};

/// A block longer than any offset these tests place an event at.
const TEST_BLOCK: usize = 1 << 16;

/// The offset `k` frames into a test block.
fn at(k: usize) -> Offset {
    Offset::of(Samples(k), Samples(TEST_BLOCK))
}

#[allow(unused_imports)]
use crate::active::{Activate, Active, ActiveExt, LoadError, ParamExt, StateExt};
use tutti_fixtures::midi;

/// Loads `audio-probe` from the directory `build.rs` builds it into, at 64
/// frames per block.
fn load() -> Active {
    let bundle = Path::new(env!("VST3_PROBE_DIR")).join("audio-probe.vst3");
    let module = tutti_plugin_types::bundle::any_module_in_bundle(
        &bundle,
        tutti_plugin_types::bundle::ModuleKind::Vst3,
    )
    .unwrap_or_else(|| {
        panic!(
            "no audio-probe module in {}. `build.rs` builds it when the `conformance` \
             feature is on:\n\n  cargo build -p tutti-vst3-host --features conformance --tests\n",
            bundle.display()
        )
    });
    Active::<f32>::load(&module, 48_000.0, 64).expect("audio-probe loads")
}

/// Run the plugin `iters` times with silent input. Buffer setup is done
/// per-iteration but uses only stack arrays — no heap allocs.
fn drive_silent(inst: &mut Active, iters: usize, transport: &TransportInfo) {
    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];
    let midi: [UmpAt; 0] = [];
    for _ in 0..iters {
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let ins: &[&[f32]] = &[];
        let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
        let events = InputEvents {
            midi: &midi,
            ..Default::default()
        };
        let _ = inst.process(&mut buffer, &events, None, transport);
    }
}

/// Two parameters, several points each, deliberately **not** in ascending
/// sample-offset order.
///
/// The unsorted order is the point. `ParamValueQueueImpl::refill_from_queue`
/// sorts on the audio thread (VST3 requires ascending offsets, and callers
/// assembling automation from several sources have no obligation to interleave
/// them in order). A sort that spills — or an `is_sorted` fast path that stops
/// covering the sort behind it — allocates in the callback, and only an
/// unsorted input reaches that branch at all.
///
/// Point count stays under the 16 points every host queue is built with
/// (`MIN_POINT_ROOM`), so a correct implementation has nothing to grow into.
fn unsorted_automation() -> ParameterChanges {
    let mut changes = ParameterChanges::reserved(2, 5);
    let lanes: [(u32, &[i32]); 2] = [(100, &[48, 0, 32, 16, 8]), (101, &[56, 24, 60, 4])];
    for (id, offsets) in lanes {
        for (i, off) in offsets.iter().enumerate() {
            changes.add_change(id.into(), *off, i as f64 / 8.0);
        }
    }
    changes
}

#[test]
fn process_with_automation_does_not_allocate() {
    let mut inst = load();
    let transport = TransportInfo::default()
        .with_tempo(120.0)
        .with_playing(true);

    // Built once, off the audio thread: the assertion is about what the host
    // does per block, not about assembling the automation.
    let changes = unsorted_automation();

    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];
    let midi: [UmpAt; 0] = [];

    let mut drive = |inst: &mut Active, iters: usize| {
        for _ in 0..iters {
            let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
            let ins: &[&[f32]] = &[];
            let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
            let events = InputEvents {
                midi: &midi,
                ..Default::default()
            };
            let _ = inst.process(&mut buffer, &events, Some(&changes), &transport);
        }
    };

    // Warm: the first block grows the host's pooled queue storage to the
    // needed capacity. Steady state must then reuse it.
    drive(&mut inst, 32);

    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, 256);
    });
}

#[test]
fn process_steady_state_does_not_allocate() {
    let mut inst = load();
    let transport = TransportInfo::default()
        .with_tempo(120.0)
        .with_playing(true);

    // Warm up — primes plugin internal state and the host's pooled
    // return buffers (first call grows them to needed capacity).
    drive_silent(&mut inst, 32, &transport);

    assert_no_alloc::assert_no_alloc(|| {
        drive_silent(&mut inst, 256, &transport);
    });
}

#[test]
fn process_with_midi_does_not_allocate() {
    let mut inst = load();
    let transport = TransportInfo::default()
        .with_tempo(120.0)
        .with_playing(true);

    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];

    // Warm: one note on/off, then 32 silent blocks to flush any
    // first-call lazy allocations inside the plugin or its voice
    // allocator.
    {
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let ins: &[&[f32]] = &[];
        let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
        let warm = [
            (Offset::ZERO, midi::note_on(MidiChannel::FIRST, 60, 0x8000)),
            (Offset::ZERO, midi::note_off(MidiChannel::FIRST, 60, 0)),
        ];
        let events = InputEvents {
            midi: &warm,
            ..Default::default()
        };
        let _ = inst.process(&mut buffer, &events, None, &transport);
    }
    drive_silent(&mut inst, 32, &transport);

    let on_event = [(Offset::ZERO, midi::note_on(MidiChannel::FIRST, 60, 0x8000))];
    let off_event = [(Offset::ZERO, midi::note_off(MidiChannel::FIRST, 60, 0))];

    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..128usize {
            let events: &[UmpAt] = match i % 32 {
                0 => &on_event,
                16 => &off_event,
                _ => &[],
            };
            let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
            let ins: &[&[f32]] = &[];
            let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
            let src = InputEvents {
                midi: events,
                ..Default::default()
            };
            let _ = inst.process(&mut buffer, &src, None, &transport);
        }
    });
}

/// **A block of 512 events from two sources goes through `process` without
/// allocating, none dropped**: 256 MIDI notes on frames ascending and 256
/// per-note expressions on frames descending, interleaved by the sort across
/// the whole list. Std's stable sort, which the input list used before, kept
/// 73 events on the stack and heap-allocated for the 74th. Then a block past
/// the expressions' budget: the rest is dropped and counted, still without
/// allocating.
#[test]
fn process_with_hundreds_of_events_does_not_allocate() {
    use tutti_vst3_host::{NoteExpressionType, NoteExpressionValue};

    let mut inst = load();
    let transport = TransportInfo::default()
        .with_tempo(120.0)
        .with_playing(true);

    // Built off the audio thread. Four events a frame across the 64-frame
    // block, alternating on and off so no note is left sounding.
    let midi: Vec<UmpAt> = (0..256usize)
        .map(|i| {
            let note = (i / 2 % 128) as u8;
            let e = if i % 2 == 0 {
                (
                    Offset::ZERO,
                    midi::note_on(MidiChannel::FIRST, note, 0x8000),
                )
            } else {
                (Offset::ZERO, midi::note_off(MidiChannel::FIRST, note, 0))
            };
            (at(i / 4), e.1)
        })
        .collect();
    let expressions: Vec<NoteExpressionValue> = (0..256i32)
        .map(|j| NoteExpressionValue {
            sample_offset: 63 - j / 4,
            note_id: j % 128,
            expression_type: NoteExpressionType::Tuning,
            value: 0.5,
        })
        .collect();
    let too_many: Vec<NoteExpressionValue> = expressions
        .iter()
        .chain(expressions.iter())
        .copied()
        .collect();

    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];
    let mut drive = |inst: &mut Active, exprs: &[NoteExpressionValue], iters: usize| {
        for _ in 0..iters {
            let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
            let ins: &[&[f32]] = &[];
            let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
            let events = InputEvents {
                midi: &midi,
                note_expressions: exprs,
                ..Default::default()
            };
            let _ = inst.process(&mut buffer, &events, None, &transport);
        }
    };

    // Warm the plugin's own voice handling, then gate.
    drive(&mut inst, &expressions, 8);
    assert_no_alloc::assert_no_alloc(|| drive(&mut inst, &expressions, 64));
    assert_eq!(inst.events_dropped(), 0, "512 events fit the input list");

    assert_no_alloc::assert_no_alloc(|| drive(&mut inst, &too_many, 4));
    // 512 per-note expressions offered a block, 256 held (their own
    // budget, a MIDI block's worth), so 256 dropped in each of four.
    assert_eq!(inst.events_dropped(), 4 * 256, "past the budget, counted");
}
