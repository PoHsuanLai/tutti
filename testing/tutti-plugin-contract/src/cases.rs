//! The cases. Each is a plain generic function over a [`Format`]; `contract!`
//! makes a test of each. Expected figures come from [`spec`].

use crate::active::main_thread;
use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_midi_types::convert::midi1_velocity_to_midi2;
use tutti_plugin_types::{
    AudioBuffer, AudioBufferMut, Control, Controller, Loaded, Normalized, ParamFlags,
    ParameterChanges, PluginIo, PluginTail, ProcessContext, ProcessOutput, Processor, SampleFormat,
    Samples,
};
use tutti_ump::{MidiChannel, MidiGroup, Ump};

use crate::spec::{self, Mode, Param};
use crate::{
    Active, BLOCK, Format, Kind, ParamTiming, RATE, prepare, set_level, set_mode, transcript,
};

fn read<C: Control + ?Sized>(c: &C, param: Param) -> Option<f64> {
    c.parameter(&main_thread(), param.id()).map(Normalized::get)
}

fn write<C: Control + ?Sized>(c: &mut C, param: Param, value: f64) {
    c.set_parameter(&main_thread(), param.id(), Normalized::clamped(value));
}

fn activate<L: Loaded>(loaded: L, block: usize) -> Active<L> {
    Active::activate(loaded, prepare(RATE, block, SampleFormat::Float32))
        .unwrap_or_else(|(_, why)| panic!("activation refused: {why:?}"))
}

fn active_in<F: Format>(mode: Mode) -> Active<F::L> {
    active_with::<F>(mode, BLOCK, SampleFormat::Float32)
}

fn active_with<F: Format>(mode: Mode, block: usize, format: SampleFormat) -> Active<F::L> {
    let mut loaded = F::load();
    set_mode::<F, _>(&mut loaded, mode);
    Active::activate(loaded, prepare(RATE, block, format))
        .unwrap_or_else(|(_, why)| panic!("activation refused: {why:?}"))
}

fn silence(frames: usize) -> [Vec<f32>; 2] {
    [vec![0.0; frames], vec![0.0; frames]]
}

fn ramp(frames: usize, base: f32) -> Vec<f32> {
    (0..frames).map(|i| base + i as f32 * 0.001).collect()
}

fn note(offset: u32, on: bool) -> (Offset, Ump) {
    let (group, channel) = (MidiGroup::FIRST, MidiChannel::of(0));
    let ump = if on {
        // 7-bit 100, widened (M2-115).
        midi::note_on(group, channel, 60, midi1_velocity_to_midi2(100))
    } else {
        midi::note_off(group, channel, 60, 0)
    };
    let offset = Offset::new(offset as usize, Samples(BLOCK)).unwrap();
    (offset, ump)
}

fn changes(points: &[(Param, i32, f64)]) -> ParameterChanges {
    let mut changes = ParameterChanges::reserved(points.len(), 4);
    for &(param, offset, value) in points {
        assert!(changes.add_change(param.id(), offset, value));
    }
    changes
}

/// Each input channel lands in its own output slot: `out = in + tag`.
// Mutation (run): CLAP, VST2 and VST3 hosts each binding or copying channels in reverse ->
//   fails (also f64_carries_same_audio).
pub fn routing_tag_oracle<F: Format>() {
    let mut a = active_in::<F>(Mode::Tag);
    let ins = [ramp(BLOCK, 0.25), ramp(BLOCK, 0.5)];
    let out = a.render(&ins, BLOCK, &ProcessContext::new());
    for c in 0..2 {
        for i in 0..BLOCK {
            assert_eq!(out[c][i], ins[c][i] + spec::tag(c), "ch{c} frame {i}");
        }
    }
}

/// A rate and block change reaches the plugin and rendering goes on.
// Mutation (run): each host telling the plugin 48 kHz instead of the rate -> fails; a
//   plugin counting blocks across an activation (core.rs, core.hpp) -> fails.
pub fn reconfigure_keeps_rendering<F: Format>() {
    let mut a = active_in::<F>(Mode::Tag);
    a.silent(BLOCK);
    a.reconfigure(prepare(44_100.0, 128, SampleFormat::Float32));
    let out = a.render(&silence(128), 128, &ProcessContext::new());
    assert!(out[0].iter().all(|&v| v == spec::tag(0)));
    assert!(out[1].iter().all(|&v| v == spec::tag(1)));
    let t = transcript::<F, _>(a.controller());
    assert_eq!((t.rate, t.max_block), (44_100.0, 128), "what it was told");
    assert_eq!(t.prepares, 2);
    assert_eq!((t.calls, t.frames), (1, 128), "counted from the activation");
    assert_eq!((t.level, t.aux), (0.5, 0.25), "the defaults");
}

/// Reported latency follows the plugin's mode.
// Mutation (run): each host reporting the latency plus one -> fails.
pub fn latency_reported<F: Format>() {
    assert_eq!(
        active_in::<F>(Mode::Delay)
            .controller()
            .latency(&main_thread()),
        spec::LATENCY
    );
    assert_eq!(
        active_in::<F>(Mode::Tag)
            .controller()
            .latency(&main_thread()),
        Samples(0)
    );
}

/// The reported latency is the delay the plugin applies, across blocks.
// Mutation (run): the plugin's delay line one frame short (core.rs and core.hpp) -> fails
//   in all four.
pub fn latency_applied<F: Format>() {
    let mut a = active_with::<F>(Mode::Delay, 128, SampleFormat::Float32);
    let frames = 100;
    let mut impulse = vec![0.0f32; frames];
    impulse[0] = 1.0;
    let mut heard = Vec::new();
    for block in 0..4 {
        let ins = if block == 0 {
            [impulse.clone(), impulse.clone()]
        } else {
            silence(frames)
        };
        heard.extend_from_slice(&a.render(&ins, frames, &ProcessContext::new())[0]);
    }
    for (i, &v) in heard.iter().enumerate() {
        let want = spec::tag(0) + if i == spec::LATENCY.0 { 1.0 } else { 0.0 };
        assert_eq!(v, want, "frame {i}");
    }
}

/// The tail the plugin reports.
// Mutation (run): each host reporting the tail plus one -> fails.
pub fn tail_reported<F: Format>() {
    assert_eq!(
        active_in::<F>(Mode::Tag).controller().tail(&main_thread()),
        PluginTail::Finite(spec::TAIL)
    );
}

/// The plugin lists its parameters with their names, defaults and, where the
/// format reports it, that they are automatable. Not step counts: CLAP's stepped
/// flag carries none, and VST2 has no steps.
// Mutation (run): each host listing all but the last parameter -> fails; a swapped name in
//   core.rs or in the C++ controller -> fails; the CLAP host reporting automatable as hidden
//   -> fails.
pub fn parameters_listed<F: Format>() {
    let mut got = F::load().parameters(&main_thread());
    got.sort_by_key(|p| p.id.get());
    assert_eq!(got.len(), Param::ALL.len(), "the listed parameters");
    for (info, &(param, default)) in got.iter().zip(&Param::ALL) {
        assert_eq!(info.id, param.id());
        assert_eq!(info.name, param.name(), "{param:?} name");
        assert_eq!(info.default_value(), default, "{param:?} default");
        assert_ne!(
            info.flag(ParamFlags::AUTOMATABLE),
            Some(false),
            "{param:?} automatable"
        );
    }
}

/// A parameter written loaded reads back, loaded and active.
// Mutation (run): each host's loaded set_parameter writing nothing -> fails.
pub fn parameter_round_trip<F: Format>() {
    let mut loaded = F::load();
    write(&mut loaded, Param::Aux, 0.75);
    assert_eq!(read(&loaded, Param::Aux), Some(0.75), "loaded");
    let mut a = activate(loaded, BLOCK);
    assert_eq!(read(a.controller(), Param::Aux), Some(0.75), "active");
}

/// An id the plugin lacks reads `None`.
// Mutation (run): each host answering for an id the plugin lacks (VST2 mapping it to index
//   0) -> fails.
pub fn unlisted_parameter_is_none<F: Format>() {
    let loaded = F::load();
    assert_eq!(
        loaded.parameter(&main_thread(), spec::UNLISTED),
        None,
        "loaded"
    );
    let mut a = activate(loaded, BLOCK);
    assert_eq!(
        a.controller().parameter(&main_thread(), spec::UNLISTED),
        None,
        "active"
    );
}

/// A write to an active plugin is applied at the head of its next block.
// Mutation (run): each host's controller dropping the write -> fails; the VST3 host not
//   queueing it to the processor -> fails the C++ plugin (a separate controller), not the
//   Rust one.
pub fn active_write_applied<F: Format>() {
    let mut a = active_in::<F>(Mode::Level);
    assert!(
        a.silent(BLOCK)[0].iter().all(|&v| v == 0.5),
        "default level"
    );
    write(a.controller(), Param::Level, 0.75);
    let out = a.silent(BLOCK);
    assert!(out[0].iter().all(|&v| v == 0.75), "got {:?}", &out[0][..4]);
}

/// A parameter point lands at its sample offset, or at the block head where
/// the format cannot place it.
// Mutation (run): each host sending every point at offset 0 (VST2: writing 0.0) -> fails.
pub fn automation_point_offset<F: Format>() {
    let mut a = active_in::<F>(Mode::Level);
    let points = changes(&[(Param::Level, 32, 0.75)]);
    let ctx = ProcessContext::new().params(&points);
    let out = a.render(&silence(BLOCK), BLOCK, &ctx);
    for (i, &v) in out[0].iter().enumerate() {
        let moved = i >= 32 || F::PARAM_TIMING == ParamTiming::BlockHead;
        assert_eq!(v, if moved { 0.75 } else { 0.5 }, "frame {i}");
    }
}

/// A note starts and stops at its sample offset.
// Mutation (run): each host moving notes one frame later -> fails.
pub fn note_at_offset<F: Format>() {
    let mut a = active_in::<F>(Mode::Notes);
    let events = [note(10, true), note(40, false)];
    let out = a.render(&silence(BLOCK), BLOCK, &ProcessContext::new().midi(&events));
    for (i, (&c0, &c1)) in out[0].iter().zip(&out[1]).enumerate() {
        let held = (10..40).contains(&i);
        assert_eq!(c0, if held { 60.0 } else { 0.0 }, "ch0 frame {i}");
        assert_eq!(c1, if held { 1060.0 } else { 0.0 }, "ch1 frame {i}");
    }
}

/// What the host sent arrives as sent: events by offset, blocks by size.
// Mutation (run): each host ordering latest first, or handing the plugin one frame fewer
//   than the block -> fails; the Rust core, the VST2 shell or the C++ plugin not logging
//   parameters -> fails on the points; the C++ plugin writing the call count 32 bits wide,
//   or the Rust core the frame count, or `parse` reading level and aux swapped -> fails.
pub fn arrival_is_ordered_and_sized<F: Format>() {
    let mut a = active_in::<F>(Mode::Notes);
    let events = [note(5, true), note(20, false), note(30, true)];
    let points = changes(&[(Param::Aux, 3, 0.5), (Param::Aux, 25, 0.75)]);
    let ctx = ProcessContext::new().midi(&events).params(&points);
    a.render(&silence(BLOCK), BLOCK, &ctx);
    for frames in [1, 7, 33] {
        a.silent(frames);
    }
    let t = transcript::<F, _>(a.controller());
    assert_eq!(t.blocks, [64, 1, 7, 33], "block sizes the host sent");
    for kind in [Kind::Param, Kind::NoteOn, Kind::NoteOff] {
        let offsets: Vec<u32> = t.of(kind).map(|e| e.offset).collect();
        assert!(
            offsets.is_sorted(),
            "{kind:?} arrived out of order: {offsets:?}"
        );
        assert!(
            offsets.iter().all(|&o| (o as usize) < BLOCK),
            "{kind:?} outside the block"
        );
    }
    assert_eq!(
        (t.calls, t.frames),
        (4, 105),
        "blocks and frames the host sent"
    );
    assert_eq!(
        (t.level, t.aux),
        (0.5, 0.75),
        "parameters as the events left them"
    );
    let points: Vec<(u32, f64)> = t.of(Kind::Param).map(|e| (e.offset, e.value)).collect();
    match F::PARAM_TIMING {
        ParamTiming::SampleAccurate => assert_eq!(points, [(3, 0.5), (25, 0.75)]),
        // One value per parameter at the head: the latest. This is stricter than
        // VST2 requires (two in-order `setParameter` calls would be legal), and it
        // cannot tell a host that keeps the last point on purpose from one that
        // loses the first by accident. Kept, so a change in either direction is seen.
        ParamTiming::BlockHead => assert_eq!(points, [(0, 0.75)]),
    }
    assert_eq!(t.of(Kind::NoteOn).count(), 2);
    assert_eq!(t.of(Kind::NoteOff).count(), 1);
}

/// State round-trips what the plugin persists.
// Mutation (run): each host's load_state loading nothing -> fails.
pub fn state_round_trip<F: Format>() {
    let mut loaded = F::load();
    write(&mut loaded, Param::Level, 0.75);
    write(&mut loaded, Param::Aux, 0.5);
    let saved = loaded.save_state(&main_thread()).expect("saves");
    write(&mut loaded, Param::Level, 0.125);
    write(&mut loaded, Param::Aux, 0.0);
    loaded.load_state(&main_thread(), &saved).expect("loads");
    assert_eq!(read(&loaded, Param::Level), Some(0.75));
    assert_eq!(read(&loaded, Param::Aux), Some(0.5));
}

/// A parameter written to a loaded plugin reaches its audio at the first block.
// Mutation (run): CLAP and VST2: the plugin resets its level on activation -> fails; VST3:
//   the host dropping the write it carries to the first block -> fails the C++ plugin, and
//   with the plugin's reset the Rust one.
pub fn loaded_write_reaches_the_audio<F: Format>() {
    let mut loaded = F::load();
    set_mode::<F, _>(&mut loaded, Mode::Level);
    write(&mut loaded, Param::Level, 0.75);
    let out = activate(loaded, BLOCK).silent(BLOCK);
    assert!(out[0].iter().all(|&v| v == 0.75), "got {:?}", &out[0][..4]);
}

/// A parameter written to a loaded plugin and saved before it ever renders is
/// in the state; a restore of that state reads it back and renders it.
// Mutation (run): CLAP and VST2: the plugin saving the default level -> fails; VST3: the
//   host leaving the carried write out of the saved state -> fails the C++ plugin, and with
//   the plugin's default the Rust one.
pub fn loaded_write_survives_a_save<F: Format>() {
    let mut loaded = F::load();
    set_mode::<F, _>(&mut loaded, Mode::Level);
    write(&mut loaded, Param::Level, 0.75);
    let saved = loaded.save_state(&main_thread()).expect("saves");
    let mut other = F::load();
    other.load_state(&main_thread(), &saved).expect("loads");
    assert_eq!(read(&other, Param::Level), Some(0.75), "read");
    let out = activate(other, BLOCK).silent(BLOCK);
    assert!(out[0].iter().all(|&v| v == 0.75), "got {:?}", &out[0][..4]);
}

/// Bytes that are not the plugin's state leave it working, with the level it held.
///
/// The level and the mode go in through the state, so they sit in the component
/// itself: a host that carries a later write to the component's first block
/// cannot hide a plugin that loses them on a refusal.
// Mutation (run): the plugin switching to Silent when it refuses a state -> fails CLAP
//   and VST3 (Rust and C++); the plugin resetting its level when it refuses one -> fails
//   CLAP and both VST3 plugins; the VST2 host zeroing parameter 0 when it refuses one ->
//   fails VST2 (the host, not the plugin, sees the bytes).
pub fn corrupt_state_leaves_plugin_usable<F: Format>() {
    let mut loaded = F::load();
    set_mode::<F, _>(&mut loaded, Mode::Level);
    set_level::<F, _>(&mut loaded, 0.75);
    let _refused = loaded.load_state(&main_thread(), b"not the plugin's state");
    assert_eq!(read(&loaded, Param::Level), Some(0.75));
    let out = activate(loaded, BLOCK).silent(BLOCK);
    assert!(out[0].iter().all(|&v| v == 0.75), "got {:?}", &out[0][..4]);
}

/// Halves of two activations do not rejoin.
// Mutation (run): each host skipping the identity check -> fails.
pub fn halves_of_two_activations_are_stuck<F: Format>() {
    let p = prepare(RATE, BLOCK, SampleFormat::Float32);
    let (pa, ca) = F::load()
        .activate(&main_thread(), p)
        .map_err(|r| r.reason)
        .expect("first");
    let (pb, cb) = F::load()
        .activate(&main_thread(), p)
        .map_err(|r| r.reason)
        .expect("second");
    let stuck = <F::L as Loaded>::deactivate(&main_thread(), pa, cb);
    assert!(stuck.is_err(), "halves of two activations joined");
    drop((stuck, pb, ca));
}

/// 64-bit audio carries the same arithmetic, at 64 bits: 0.1 is not exact in 32.
// Mutation (run): each host's 64-bit port table or bus holding one channel -> fails; the
//   VST2 host narrowing through 32 bits -> fails.
pub fn f64_carries_same_audio<F: Format>() {
    let mut a = active_with::<F>(Mode::Tag, BLOCK, SampleFormat::Float64);
    let ins = [vec![0.1f64; BLOCK], vec![0.2f64; BLOCK]];
    let out = a.render(&ins, BLOCK, &ProcessContext::new());
    assert!(
        out[0].iter().all(|&v| v == 0.1 + 1.0),
        "ch0 {:?}",
        &out[0][..2]
    );
    assert!(
        out[1].iter().all(|&v| v == 0.2 + 2.0),
        "ch1 {:?}",
        &out[1][..2]
    );
}

/// A plugin whose log overflowed fails the oracle instead of passing on the
/// part it kept.
// Mutation (run): the plugin not flagging blocks past the ring (core.rs and core.hpp) ->
//   fails in all four.
pub fn overflowing_log_fails_the_oracle<F: Format>() {
    let mut a = active_in::<F>(Mode::Tag);
    for _ in 0..=spec::LOG_BLOCKS {
        a.silent(BLOCK);
    }
    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        transcript::<F, _>(a.controller())
    }));
    let why = read.expect_err("a transcript past the log's room is refused");
    let text = why
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| why.downcast_ref::<&str>().copied());
    assert!(
        text.is_some_and(|t| t.contains("log overflowed")),
        "{text:?}"
    );
}

/// Rendering a block allocates nothing (run under `AllocDisabler`).
///
/// A failure aborts the process, so under plain `cargo test` it takes the whole
/// test binary with it, and a [`Gap`](crate::Gap) cannot express it.
// Mutation (run): each host allocating once per block -> aborts under `AllocDisabler`.
pub fn process_does_not_allocate<F: Format>() {
    let mut a = active_in::<F>(Mode::Notes);
    let events = [note(5, true), note(20, false)];
    let points = changes(&[(Param::Aux, 3, 0.5)]);
    let ctx = ProcessContext::new().midi(&events).params(&points);
    let ins = silence(BLOCK);
    let refs = [ins[0].as_slice(), ins[1].as_slice()];
    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let mut outs = [left.as_mut_slice(), right.as_mut_slice()];
    let mut out = ProcessOutput::default();
    let mut io = PluginIo {
        buffer: AudioBufferMut::F32(AudioBuffer::new(&refs, &mut outs, RATE)),
        ctx: &ctx,
        out: &mut out,
    };
    // The first block may size the host's buffers.
    a.processor().process(&mut io).expect("renders");
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..50 {
            a.processor().process(&mut io).expect("renders");
        }
    });
}
