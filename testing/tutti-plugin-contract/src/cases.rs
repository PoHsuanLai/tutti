//! The cases. Each is a plain generic function over a [`Format`]; `contract!`
//! makes a test of each. Expected figures come from [`spec`].

use tutti_midi_types::MidiEvent;
use tutti_plugin_types::{
    AudioBuffer, AudioBufferMut, Control, Controller, Loaded, Normalized, ParamFlags,
    ParameterChanges, PluginIo, PluginTail, ProcessContext, ProcessOutput, Processor, SampleFormat,
    Samples,
};
use tutti_ump::{MidiChannel, MidiGroup};

use crate::spec::{self, Mode, Param};
use crate::{Active, BLOCK, Format, Kind, ParamTiming, RATE, prepare, set_mode, transcript};

fn read<C: Control + ?Sized>(c: &C, param: Param) -> Option<f64> {
    c.parameter(param.id()).map(Normalized::get)
}

fn write<C: Control + ?Sized>(c: &mut C, param: Param, value: f64) {
    c.set_parameter(param.id(), Normalized::new(value));
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

fn note(offset: u32, on: bool) -> MidiEvent {
    let (group, channel) = (MidiGroup::FIRST, MidiChannel::of(0));
    let mut event = if on {
        MidiEvent::note_on_7bit(group, channel, 60, 100)
    } else {
        MidiEvent::note_off(group, channel, 60, 0)
    };
    event.frame_offset = offset;
    event
}

fn changes(points: &[(Param, i32, f64)]) -> ParameterChanges {
    let mut changes = ParameterChanges::reserved(points.len(), 4);
    for &(param, offset, value) in points {
        assert!(changes.add_change(param.id(), offset, value));
    }
    changes
}

/// Each input channel lands in its own output slot: `out = in + tag`.
// Mutation (run): the host binds the input channels in reverse order -> fails (and
//   `f64_carries_same_audio`).
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
// Mutation (run): the host activates at a fixed 48 kHz -> fails on the rate the plugin was
//   told.
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
}

/// Reported latency follows the plugin's mode.
// Mutation (run): the host reports the latency plus one -> fails.
pub fn latency_reported<F: Format>() {
    assert_eq!(
        active_in::<F>(Mode::Delay).controller().latency(),
        spec::LATENCY
    );
    assert_eq!(active_in::<F>(Mode::Tag).controller().latency(), Samples(0));
}

/// The reported latency is the delay the plugin applies, across blocks.
// Mutation (run): the plugin's delay line one frame short -> fails at the impulse.
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
// Mutation (run): the host reports the tail plus one -> fails.
pub fn tail_reported<F: Format>() {
    assert_eq!(
        active_in::<F>(Mode::Tag).controller().tail(),
        PluginTail::Finite(spec::TAIL)
    );
}

/// The plugin lists its parameters with their names, defaults and, where the
/// format reports it, that they are automatable. Not step counts: CLAP's stepped
/// flag carries none, and VST2 has no steps.
// Mutation (run): the host lists every parameter but the last -> fails; the
// plugin swaps two names -> fails; the host reports automatable as hidden ->
// fails.
pub fn parameters_listed<F: Format>() {
    let mut got = F::load().parameters();
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
// Mutation (run): the host's loaded `set_parameter` writes nothing -> fails.
pub fn parameter_round_trip<F: Format>() {
    let mut loaded = F::load();
    write(&mut loaded, Param::Aux, 0.75);
    assert_eq!(read(&loaded, Param::Aux), Some(0.75), "loaded");
    let mut a = activate(loaded, BLOCK);
    assert_eq!(read(a.controller(), Param::Aux), Some(0.75), "active");
}

/// An id the plugin lacks reads `None`.
// Mutation (run): the host answers 0.0 for an id the plugin lacks -> fails.
pub fn unlisted_parameter_is_none<F: Format>() {
    let loaded = F::load();
    assert_eq!(loaded.parameter(spec::UNLISTED), None, "loaded");
    let mut a = activate(loaded, BLOCK);
    assert_eq!(a.controller().parameter(spec::UNLISTED), None, "active");
}

/// A write to an active plugin is applied at the head of its next block.
// Mutation (run): the controller drops a queued edit -> fails.
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
// Mutation (run): the host sends every parameter point at offset 0 -> fails.
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
// Mutation (run): the host moves every note one frame later -> fails.
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
// Mutation (run): the host sorts events latest first -> fails; the host hands the plugin one
//   frame fewer than the block -> fails on the block sizes.
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
    assert_eq!(t.of(Kind::Param).count(), 2);
    assert_eq!(t.of(Kind::NoteOn).count(), 2);
    assert_eq!(t.of(Kind::NoteOff).count(), 1);
}

/// State round-trips what the plugin persists.
// Mutation (run): the host's `load_state` loads nothing -> fails.
pub fn state_round_trip<F: Format>() {
    let mut loaded = F::load();
    write(&mut loaded, Param::Level, 0.75);
    write(&mut loaded, Param::Aux, 0.5);
    let saved = loaded.save_state().expect("saves");
    write(&mut loaded, Param::Level, 0.125);
    write(&mut loaded, Param::Aux, 0.0);
    loaded.load_state(&saved).expect("loads");
    assert_eq!(read(&loaded, Param::Level), Some(0.75));
    assert_eq!(read(&loaded, Param::Aux), Some(0.5));
}

/// A parameter written to a loaded plugin reaches its audio at the first block.
// Mutation (run): the plugin resets its level on activation -> fails.
pub fn loaded_write_reaches_the_audio<F: Format>() {
    let mut loaded = F::load();
    set_mode::<F, _>(&mut loaded, Mode::Level);
    write(&mut loaded, Param::Level, 0.75);
    let out = activate(loaded, BLOCK).silent(BLOCK);
    assert!(out[0].iter().all(|&v| v == 0.75), "got {:?}", &out[0][..4]);
}

/// A parameter written to a loaded plugin and saved before it ever renders is
/// in the state; a restore of that state reads it back and renders it.
// Mutation (run): the plugin saves the default level -> fails.
pub fn loaded_write_survives_a_save<F: Format>() {
    let mut loaded = F::load();
    set_mode::<F, _>(&mut loaded, Mode::Level);
    write(&mut loaded, Param::Level, 0.75);
    let saved = loaded.save_state().expect("saves");
    let mut other = F::load();
    other.load_state(&saved).expect("loads");
    assert_eq!(read(&other, Param::Level), Some(0.75), "read");
    let out = activate(other, BLOCK).silent(BLOCK);
    assert!(out[0].iter().all(|&v| v == 0.75), "got {:?}", &out[0][..4]);
}

/// Bytes that are not the plugin's state leave it working.
// Mutation (run): the plugin resets its level before it refuses a state -> fails.
pub fn corrupt_state_leaves_plugin_usable<F: Format>() {
    let mut loaded = F::load();
    write(&mut loaded, Param::Level, 0.75);
    let _refused = loaded.load_state(b"not the plugin's state");
    assert_eq!(read(&loaded, Param::Level), Some(0.75));
    let out = activate(loaded, BLOCK).silent(BLOCK);
    assert!(out[0].iter().all(|&v| v == spec::tag(0)));
}

/// Halves of two activations do not rejoin.
// Mutation (run): the host skips the check that the halves belong together -> fails.
pub fn halves_of_two_activations_are_stuck<F: Format>() {
    let p = prepare(RATE, BLOCK, SampleFormat::Float32);
    let (pa, ca) = F::load().activate(p).map_err(|r| r.reason).expect("first");
    let (pb, cb) = F::load().activate(p).map_err(|r| r.reason).expect("second");
    let stuck = <F::L as Loaded>::deactivate(pa, cb);
    assert!(stuck.is_err(), "halves of two activations joined");
    drop((stuck, pb, ca));
}

/// 64-bit audio carries the same arithmetic.
// Mutation (run): the host's 64-bit port table declares one channel -> fails.
pub fn f64_carries_same_audio<F: Format>() {
    let mut a = active_with::<F>(Mode::Tag, BLOCK, SampleFormat::Float64);
    let ins = [vec![0.25f64; BLOCK], vec![0.5f64; BLOCK]];
    let out = a.render(&ins, BLOCK, &ProcessContext::new());
    assert!(out[0].iter().all(|&v| v == 1.25));
    assert!(out[1].iter().all(|&v| v == 2.5));
}

/// A plugin whose log overflowed fails the oracle instead of passing on the
/// part it kept.
// Mutation (run): the plugin not flagging blocks past the ring -> fails.
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
// Mutation (run): the host allocates once per block -> aborts under `AllocDisabler`.
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
