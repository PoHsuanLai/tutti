//! **A voice renders the same frames whatever its blocks.**
//!
//! A voice node sizes its scratch from `Prepare::max_block` and renders each
//! block in one piece. Nothing a voice computes per frame depends on where a
//! block starts or how long it is: the per-frame arithmetic (taps, cubic,
//! gain, the stretch filter's tick) is the same arithmetic in any piece,
//! and the placed read re-seats at each block's first frame where its step
//! would have put it (the seat and the step are one model; a seat that
//! landed elsewhere is the drift the mutations below plant). So the same
//! voice, under the same transport, renders the same samples, bit for bit,
//! prepared at 64 frames in 64-frame blocks, at 512 in 512-frame blocks, and
//! at 512 in a ragged schedule (one frame, a prime, a block one short of
//! the maximum, the maximum).
//!
//! The seat is a float computation (beat → seconds → frames), so "where the
//! step would have put it" holds to the bit at this clock (48 kHz, 120 BPM,
//! a frame-derived beat) and to rounding in general; the golden digests in
//! `tutti-export` and `bevy-tutti`, recorded with 64-frame pieces, are
//! unchanged by the one-piece render.
//!
//! What may move with the blocks, and is held still here: a control read
//! once per block (a voice's gain, varispeed and stretch, the disk tier's
//! rates) lands on the next block's first frame, so a control changed
//! mid-render is heard at a block edge that depends on the schedule. The
//! controls here are set before the first block.
//!
//! The transport is derived from the frame (`beat = frame / frames per
//! beat`), not accumulated block by block, so every schedule hands each
//! frame the same beat; a mock that summed per-block increments would round
//! differently per schedule and the comparison would test the mock.
//!
//! Every case is checked to sound (a silent render matches anything).
//!
//! Beside `voice::block_render`'s in-crate check (a pool at 256-frame
//! blocks with transport changes inside them against 64), this covers the
//! larger and ragged blocks and each voice node on its own: a `VoiceNode`,
//! a `MemorySource` and a `DiskVoice`.
//!
//! Mutations (each run): the memory tier's step disagreeing with its seat
//! (`placed_positions`' step `× 1.0001`) → every reseat lands elsewhere per
//! schedule → "pool, plain" fails at frame 64; the disk tier's the same
//! (`live_render`'s rate `× 1.0001`) → "disk voice node" fails at frame 64;
//! the stretch filter forgetting its intake debt between calls (`start =
//! 0.0`) → "pool, pitch +700c" fails. The cold path (a block longer than
//! the scratch, in pieces), each node's rendering only its first piece
//! (`from = frames`): the pool's → "pool, plain" fails, `VoiceNode`'s →
//! "voice node, plain", `MemorySource`'s → "memory source, placed",
//! `DiskVoice`'s → "disk voice node", each "prepared at 64, 512-frame
//! blocks" at frame 64.
//!
//! Not caught here, by design: a node that ignored `max_block` and kept
//! rendering 64-frame pieces renders the same frames (that is the
//! invariant). `lanes::tests::every_voice_node_sizes_its_scratch_from_max_block`
//! and the disk voice's `a_disk_voice_sizes_its_positions_from_max_block`
//! pin the sizing; `rt_no_alloc`'s ragged 512-frame gates pin that it costs
//! no allocation.

use std::f32::consts::TAU;
use std::path::Path;
use std::sync::Arc;

use tutti_core::{
    Beat, Bpm, Cents, Frame, PlaybackRate, SamplePosition, SampleRate, Samples, StretchFactor,
};
use tutti_graph::contract::{self, Direct};
use tutti_graph::{Env, Node, Transport, TransportChanges};
use tutti_io::Wave;
use tutti_sampler::{
    Command, Direction, DiskStreamer, DiskStreamerConfig, LoopSetting, MemorySource, Playback,
    SlotId, StepOutcome, Voice, VoiceNode, VoicePool, VoiceSource,
};

const SR: f64 = 48_000.0;
const BPM: f64 = 120.0;
/// Frames rendered per case.
const FRAMES: usize = 24_000;
/// One frame, a prime, a block one short of the maximum and the maximum,
/// and lengths that straddle 64 — repeated until [`FRAMES`].
const RAGGED: &[usize] = &[1, 37, 100, 511, 512, 64, 200, 129, 3, 65];

/// The schedules every case renders under: (`max_block`, block lengths).
const SCHEDULES: &[(usize, &[usize])] = &[(64, &[64]), (512, &[512]), (512, RAGGED)];

/// A stereo tone with an overtone, `secs` long: a waveform whose every
/// sample differs, so a frame read at the wrong position shows.
fn tone(secs: f64) -> Arc<Wave> {
    let mut w = Wave::new(2, SR);
    for i in 0..(secs * SR) as usize {
        w.push_frame(&frame_at(i));
    }
    Arc::new(w)
}

fn frame_at(i: usize) -> [f32; 2] {
    let t = i as f32 / SR as f32;
    [
        (TAU * 440.0 * t).sin() * 0.4 + (TAU * 1320.0 * t).sin() * 0.2,
        (TAU * 660.0 * t).sin() * 0.4,
    ]
}

/// The transport at absolute frame `frame`: rolling from beat 0, the beat
/// derived from the frame.
fn env_at(frame: usize, len: usize) -> Env {
    let beat = frame as f64 * BPM / 60.0 / SR;
    Env {
        frame: Frame(frame as u64),
        sample_rate: SampleRate(SR),
        block_len: Samples(len),
        transport: Transport::new(true, Bpm::new(BPM), Beat::new(beat), None),
        changes: TransportChanges::NONE,
        play: tutti_graph::PlayClock::ZERO,
    }
}

/// [`FRAMES`] of `node`, prepared for `max_block`, in blocks of `schedule`
/// (cycled); planar, the first two channels.
fn render<N: Node>(node: N, max_block: usize, schedule: &[usize]) -> [Vec<f32>; 2] {
    let mut d = Direct::new(node, SampleRate(SR), max_block);
    let mut out = [Vec::with_capacity(FRAMES), Vec::with_capacity(FRAMES)];
    let mut frame = 0;
    for &len in schedule.iter().cycle() {
        if frame >= FRAMES {
            break;
        }
        let len = len.min(FRAMES - frame);
        d.block_in(&env_at(frame, len));
        for (c, o) in out.iter_mut().enumerate() {
            o.extend_from_slice(d.output(c));
        }
        frame += len;
    }
    out
}

/// [`FRAMES`] of `node` driven by hand (`contract::drive_in`) in blocks of
/// `schedule`, prepared for `prepared` frames first or not at all: the
/// cold path, a block longer than the node's scratch, rendered in pieces.
fn render_by_hand<N: Node>(
    mut node: N,
    prepared: Option<usize>,
    schedule: &[usize],
) -> [Vec<f32>; 2] {
    if let Some(max) = prepared {
        node = contract::prepared(node, SampleRate(SR), max);
    }
    let mut out = [Vec::with_capacity(FRAMES), Vec::with_capacity(FRAMES)];
    let mut frame = 0;
    for &len in schedule.iter().cycle() {
        if frame >= FRAMES {
            break;
        }
        let len = len.min(FRAMES - frame);
        let audio = contract::drive_in(&mut node, &env_at(frame, len), &[], &[], &[]).audio;
        for (c, o) in out.iter_mut().enumerate() {
            o.extend_from_slice(&audio[c]);
        }
        frame += len;
    }
    out
}

/// `got` equals `want` bit for bit, or the first frame that differs.
fn assert_same(case: &str, name: &str, got: &[Vec<f32>; 2], want: &[Vec<f32>; 2]) {
    for c in 0..2 {
        assert_eq!(got[c].len(), FRAMES);
        if let Some(i) = (0..FRAMES).find(|&i| got[c][i].to_bits() != want[c][i].to_bits()) {
            panic!(
                "{case}: {name}: channel {c} frame {i} is {} where the reference renders {}",
                got[c][i], want[c][i]
            );
        }
    }
}

/// Renders `build()` under every schedule and asserts the renders are equal
/// bit for bit, and that they sound. Also the **cold path**, a block longer
/// than the node's scratch, rendered in pieces: prepared at 64 but handed
/// 512-frame blocks (against the 64-frame render), and never prepared, in
/// 512-frame blocks (against the same, unprepared, in 64-frame blocks: an
/// unprepared node keeps its construction-time rate, 44.1 kHz against this
/// 48 kHz clock, so it is compared with itself; and at a mismatched rate
/// its seat and step agree only to rounding, so to within 1e-6 there, not
/// to the bit — a piece seated a block's beat plus 64 frames on, where a
/// block seats on its own frame-derived beat).
fn assert_block_invariant<N: Node>(case: &str, mut build: impl FnMut() -> N) {
    let renders: Vec<_> = SCHEDULES
        .iter()
        .map(|&(max, schedule)| render(build(), max, schedule))
        .collect();
    let past_max = render_by_hand(build(), Some(64), &[512]);
    assert_same(
        case,
        "prepared at 64, 512-frame blocks",
        &past_max,
        &renders[0],
    );
    let unprepared = render_by_hand(build(), None, &[512]);
    let unprepared_64 = render_by_hand(build(), None, &[64]);
    for c in 0..2 {
        assert_eq!(unprepared[c].len(), FRAMES);
        if let Some(i) =
            (0..FRAMES).find(|&i| (unprepared[c][i] - unprepared_64[c][i]).abs() > 1e-6)
        {
            panic!(
                "{case}: unprepared, 512-frame blocks: channel {c} frame {i} is {} where \
                 64-frame blocks render {}",
                unprepared[c][i], unprepared_64[c][i]
            );
        }
    }
    let peak = unprepared[0].iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.05, "{case}: unprepared, silent (peak {peak})");
    let peak = renders[0][0].iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.05, "{case}: silent (peak {peak}); proves nothing");
    let (want, rest) = renders.split_first().expect("schedules");
    for (r, (max, schedule)) in rest.iter().zip(&SCHEDULES[1..]) {
        for c in 0..2 {
            assert_eq!(r[c].len(), FRAMES);
            if let Some(i) = (0..FRAMES).find(|&i| r[c][i].to_bits() != want[c][i].to_bits()) {
                panic!(
                    "{case}: max_block {max}, blocks {schedule:?}: channel {c} frame {i} is \
                     {} where 64-frame blocks render {}",
                    r[c][i], want[c][i]
                );
            }
        }
    }
}

/// A placed memory voice playing `play`.
fn memory_voice(wave: &Arc<Wave>, play: Playback) -> Voice {
    let source = MemorySource::placed(Arc::clone(wave), Beat::new(0.0), None);
    Voice {
        source: VoiceSource::Memory(source),
        play,
        channel_index: None,
    }
}

/// The memory-tier playbacks every pool case renders.
fn playbacks() -> Vec<(&'static str, Playback)> {
    vec![
        ("plain", Playback::default()),
        (
            "varispeed",
            Playback {
                speed: PlaybackRate::new(1.37),
                ..Default::default()
            },
        ),
        (
            "reverse",
            Playback {
                direction: Direction::Reverse,
                ..Default::default()
            },
        ),
        (
            "loop with crossfade",
            Playback {
                loop_: LoopSetting::On {
                    start: SamplePosition(1_000.0),
                    end: SamplePosition(5_797.0),
                    crossfade_frames: 300,
                },
                ..Default::default()
            },
        ),
        (
            "stretch 0.5x",
            Playback {
                stretch: StretchFactor::new(0.5),
                ..Default::default()
            },
        ),
        (
            "pitch +700c",
            Playback {
                pitch: Cents::new(700.0),
                ..Default::default()
            },
        ),
    ]
}

/// **A pool's memory voices, and a lone `VoiceNode`, render the same frames
/// at 64, 512 and ragged blocks**: plain, varispeed, reversed, looped across
/// a crossfade, stretched and pitched (the phase vocoder fed and drained
/// across every block edge).
#[test]
fn memory_voices_render_the_same_frames_at_any_block_length() {
    let wave = tone(1.5);
    for (name, play) in playbacks() {
        assert_block_invariant(&format!("pool, {name}"), || {
            let mut pool = VoicePool::new();
            pool.insert_voice(SlotId(1), memory_voice(&wave, play.clone()));
            pool
        });
        assert_block_invariant(&format!("voice node, {name}"), || {
            VoiceNode::new(memory_voice(&wave, play.clone()))
        });
    }
}

/// **A `MemorySource` as its own node renders the same frames at any block
/// length**, placed (the gate's read) and free-running (its cursor).
#[test]
fn a_memory_source_node_renders_the_same_frames_at_any_block_length() {
    let wave = tone(1.5);
    assert_block_invariant("memory source, placed", || {
        MemorySource::placed(Arc::clone(&wave), Beat::new(0.0), None)
    });
    assert_block_invariant("memory source, free-running", || {
        let s = MemorySource::new(Arc::clone(&wave));
        s.trigger();
        s
    });
}

/// A stereo float WAV of [`frame_at`], `frames` long.
fn write_wav(path: &Path, frames: usize) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("create wav");
    for i in 0..frames {
        for s in frame_at(i) {
            w.write_sample(s).expect("write wav");
        }
    }
    w.finalize().expect("finalize wav");
}

/// A live disk voice streaming `path` from its start, on its own
/// hand-driven butler, its ring primed (a file this short is prefilled
/// whole, so the render never waits on the butler).
fn disk_voice(path: &Path) -> (DiskStreamer, tutti_sampler::DiskVoice) {
    let mut s = DiskStreamer::manual(SR, DiskStreamerConfig::default()).expect("streamer builds");
    s.commands()
        .send(Command::Stream {
            channel_index: 0,
            file_path: path.to_path_buf(),
            offset: SamplePosition(0.0),
        })
        .expect("the butler is alive");
    for _ in 0..4096 {
        if s.step_once() != StepOutcome::Busy {
            break;
        }
    }
    let v = s
        .status()
        .take_disk_voice(0, Beat::new(0.0), None)
        .expect("a primed stream gives a voice");
    (s, v)
}

/// **A live disk voice renders the same frames at any block length**: as
/// its own node and in a pool (the ring claimed once per run of the block,
/// its positions in the owner's scratch).
#[test]
fn a_disk_voice_renders_the_same_frames_at_any_block_length() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tone.wav");
    write_wav(&path, (1.5 * SR) as usize);
    // Each render gets its own stream: a ring is read once.
    let mut streamers = Vec::new();
    assert_block_invariant("disk voice node", || {
        let (s, v) = disk_voice(&path);
        streamers.push(s);
        v
    });
    assert_block_invariant("pool, disk voice", || {
        let (s, v) = disk_voice(&path);
        streamers.push(s);
        let mut pool = VoicePool::new();
        pool.insert_voice(
            SlotId(1),
            Voice {
                source: VoiceSource::Disk(v),
                play: Playback::default(),
                channel_index: None,
            },
        );
        pool
    });
}
