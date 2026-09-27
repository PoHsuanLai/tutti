//! The block read is the same **whatever the block length, and wherever a
//! transport change falls**, bit for bit.
//!
//! Doc 013 item 7 made the voice read block-at-a-time (`PlaybackSlot::render_into`)
//! and pinned it against the per-frame read it replaced; items 8 and 9 took
//! the per-frame read away with `AudioUnit` (a graph node has one entry
//! point) and moved the transport into each block's `Env`. What a block read
//! must now hold to is the host's side of that: a host hands the graph
//! blocks of any length, and a start, a stop or a seek lands inside a block
//! as a change in its `Env`, not at its edge. So two pools are given the same
//! voices; one renders in 256-frame blocks with every transport event inside
//! a block (a change in its `Env`), the other in 64-frame blocks with the
//! same events at block starts, on one host clock, and every output sample
//! must have the same bits.
//!
//! **What it isolates is the block machinery and the clock.** The two sides
//! share every kernel; a bug inside one shows on both and is the kernel's own
//! tests' (`interp`, `loop_span`, `stretch`, the tier tables) to catch. This
//! pins the pieces (a longer block read as the same 64-frame pieces), the
//! runs (the clock's cut at a change, a jump seen on its frame), the flush at
//! a jump, the lanes, `hermite_lanes`, `filter_lanes`, the gain and the mix.

use std::path::Path;
use std::sync::Arc;

use tutti_core::{
    Amplitude, Beat, Bpm, Cents, ChannelLayout, Frame, FrameClock, PlaybackRate, SamplePosition,
    SampleRate, Samples, StretchFactor,
};
use tutti_graph::{contract, Env, Node, Offset, Prepare, Transport, TransportChanges};
use tutti_io::Wave;

use super::memory_source::{LoopSetting, MemorySource};
use super::pool::VoicePool;
use super::types::{Direction, Playback, SlotId, Voice, VoiceSource};
use crate::{Command, DiskStreamer, DiskStreamerConfig};

const SR: f64 = 48_000.0;
const LEN: usize = 3 * 48_000;

/// Distinguishable stereo material: two partials on the left, one on the
/// right, so a swapped, dropped or folded channel shows.
fn sample(c: usize, i: usize) -> f32 {
    let t = i as f32 / SR as f32;
    match c {
        0 => (std::f32::consts::TAU * 440.0 * t).sin() * 0.4 + (i % 97) as f32 * 1e-3,
        _ => (std::f32::consts::TAU * 660.0 * t).sin() * 0.3 - (i % 89) as f32 * 1e-3,
    }
}

fn wave(channels: usize, rate: f64) -> Arc<Wave> {
    let mut w = Wave::new(channels, rate);
    for i in 0..LEN {
        let frame: Vec<f32> = (0..channels).map(|c| sample(c % 2, i + 7 * c)).collect();
        w.push_frame(&frame);
    }
    Arc::new(w)
}

fn write_wav(path: &Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("writes");
    for i in 0..LEN {
        w.write_sample(sample(0, i)).expect("writes");
        w.write_sample(sample(1, i + 7)).expect("writes");
    }
    w.finalize().expect("writes");
}

fn memory(w: &Arc<Wave>, start: f64) -> VoiceSource {
    let mut s = MemorySource::placed(Arc::clone(w), Beat::new(start), None);
    s.set_render_rate(SampleRate(SR));
    VoiceSource::Memory(s)
}

/// The host's transport: a frame clock (counted, as an engine's), rolling or
/// standing.
#[derive(Clone, Copy)]
struct Host {
    clock: FrameClock,
    playing: bool,
}

impl Host {
    fn transport(&self) -> Transport {
        Transport::counted(self.playing, self.clock.tempo(), self.clock.origin(), None)
    }

    fn advance(&mut self, frames: usize) {
        if self.playing {
            self.clock.advance(Samples(frames), None);
        }
    }
}

/// What happens to the transport at the start of a 64-frame block.
#[derive(Clone, Copy)]
enum Event {
    Seek(f64),
    Stop,
    Start,
}

fn apply(host: &mut Host, event: Event) {
    match event {
        Event::Seek(beat) => host.clock.seat(Beat::new(beat)),
        Event::Stop => host.playing = false,
        Event::Start => host.playing = true,
    }
}

fn env(frame: u64, len: usize, transport: Transport, changes: TransportChanges) -> Env {
    Env {
        frame: Frame(frame),
        sample_rate: SampleRate(SR),
        block_len: Samples(len),
        transport,
        changes,
    }
}

fn play(f: impl FnOnce(&mut Playback)) -> Playback {
    let mut p = Playback::default();
    f(&mut p);
    p
}

/// **The block read is the same in 256-frame blocks with the transport
/// changing inside them as in 64-frame blocks with it changing at their
/// starts**, bit for bit, summed over a pool of voices covering every arm the
/// read forks on: in memory at unity, at 1.37x varispeed and half gain,
/// reversed, on a crossfaded loop, stretched 0.5x, pitched +700 cents,
/// stretched and pitched at half gain, a mono wave fanned out, a 24 kHz wave
/// on a 48 kHz clock, a six-channel wave folded to stereo, a window that
/// opens mid-render (plain, and stretched); from disk live at unity, at 0.75x,
/// and stretched 1.5x; and a disk voice forked offline. Through a seek (a
/// re-seat, a ring jump, a stretch flush) and a stop and restart, each on a
/// frame inside a 256-frame block.
///
/// Mutation (run): `Runs` ignoring `Env::changes` (one segment per block)
/// → the long blocks play through the stop → fails.
///
/// Not caught here, because both sides share it (this is a consistency
/// check): `render_into` not flushing at a jump — the voice pool's
/// `a_transport_seek_flushes_stretch_state` and
/// `a_transport_seek_flushes_a_standalone_voice_node` fail on it (run) —
/// and the clock reporting a jump on a restart where the playhead stood,
/// which `clock`'s `a_stop_a_seek_while_stopped_and_a_restart_are_no_jump`
/// pins.
#[test]
fn the_block_read_is_the_same_at_any_block_length() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("material.wav");
    write_wav(&path);
    let stereo = wave(2, SR);
    let mono = wave(1, SR);
    let low = wave(2, 24_000.0);
    let six = wave(6, SR);

    // Disk streams: channels 0..4 for the long-block pool, 4..8 for the
    // short-block one.
    let mut streamer =
        DiskStreamer::manual(SR, DiskStreamerConfig::default()).expect("streamer builds");
    for channel_index in 0..8 {
        streamer
            .commands()
            .send(Command::Stream {
                channel_index,
                file_path: path.clone(),
                offset: SamplePosition(0.0),
            })
            .expect("the butler is alive");
    }
    assert!(
        streamer.step_until_settled(512) < 512,
        "the rings never primed"
    );
    let disk = |channel: usize| {
        let mut v = streamer
            .status()
            .take_disk_voice(channel, Beat::new(0.0), None)
            .expect("a primed stream gives a voice");
        v.set_render_rate(SampleRate(SR));
        v
    };

    let build = |first_disk: usize| {
        let mut pool = VoicePool::with_channels(None, ChannelLayout::STEREO)
            .expect("a width the sampler reads");
        pool.prepare(&Prepare::new(SampleRate(SR), Samples(256)));
        let mut forked = disk(first_disk + 3).fork_copy();
        forked.set_render_rate(SampleRate(SR));
        let voices: Vec<(VoiceSource, Playback)> = vec![
            (memory(&stereo, 0.0), Playback::default()),
            (
                memory(&stereo, 0.0),
                play(|p| {
                    p.speed = PlaybackRate::new(1.37);
                    p.gain = Amplitude::new(0.5);
                }),
            ),
            (
                memory(&stereo, 0.0),
                play(|p| p.direction = Direction::Reverse),
            ),
            (
                memory(&stereo, 0.0),
                play(|p| {
                    p.loop_ = LoopSetting::On {
                        start: SamplePosition(1_000.0),
                        end: SamplePosition(9_000.0),
                        crossfade_frames: 256,
                    }
                }),
            ),
            (
                memory(&stereo, 0.0),
                play(|p| p.stretch = StretchFactor::new(0.5)),
            ),
            (memory(&stereo, 0.0), play(|p| p.pitch = Cents::new(700.0))),
            (
                memory(&stereo, 0.0),
                play(|p| {
                    p.stretch = StretchFactor::new(1.5);
                    p.pitch = Cents::new(-300.0);
                    // The stretched arm applies the gain before the filter.
                    p.gain = Amplitude::new(0.5);
                }),
            ),
            (memory(&mono, 0.0), Playback::default()),
            (memory(&low, 0.0), Playback::default()),
            (memory(&six, 0.0), Playback::default()),
            (memory(&stereo, 1.0), Playback::default()),
            // Stretched, and silent until its window opens: the filter must
            // be fed nothing before then.
            (
                memory(&stereo, 1.0),
                play(|p| p.stretch = StretchFactor::new(0.5)),
            ),
            (VoiceSource::Disk(disk(first_disk)), Playback::default()),
            (
                VoiceSource::Disk(disk(first_disk + 1)),
                play(|p| p.speed = PlaybackRate::new(0.75)),
            ),
            (
                VoiceSource::Disk(disk(first_disk + 2)),
                play(|p| p.stretch = StretchFactor::new(1.5)),
            ),
            (VoiceSource::Disk(forked), Playback::default()),
        ];
        for (i, (source, play)) in voices.into_iter().enumerate() {
            pool.insert_voice(
                SlotId(i as u128),
                Voice {
                    source,
                    play,
                    channel_index: None,
                },
            );
        }
        pool
    };
    let mut long = build(0);
    let mut short = build(4);
    assert_eq!(long.voice_count(), 16);

    // Events at the start of a 64-frame block, none on a 256-frame one: in
    // the long blocks each is a change in the block's `Env`.
    let events = |b: usize| match b {
        // A seek back: every voice re-seats, the disk voices jump.
        301 => Some(Event::Seek(0.3)),
        // Stopped for a while, then rolling again where it stood.
        450 => Some(Event::Stop),
        471 => Some(Event::Start),
        _ => None,
    };
    let mut host = Host {
        clock: FrameClock::new(Beat::new(0.0), Bpm::new(120.0), SampleRate(SR)),
        playing: true,
    };
    let mut heard = [false; 2];
    for big in 0..150usize {
        // The long block: its first frame's transport (an event there is
        // its start), and a change in its `Env` at each event after.
        let mut walk = host;
        if let Some(e) = events(big * 4) {
            apply(&mut walk, e);
        }
        let first = walk.transport();
        let mut changes = TransportChanges::NONE;
        for k in 1..4 {
            walk.advance(64);
            if let Some(e) = events(big * 4 + k) {
                apply(&mut walk, e);
                changes
                    .push(
                        Offset::new(k * 64, Samples(256)).expect("inside the block"),
                        walk.transport(),
                    )
                    .expect("a change per piece");
            }
        }
        let frame = (big * 256) as u64;
        let got =
            contract::drive_in(&mut long, &env(frame, 256, first, changes), &[], &[], &[]).audio;

        // The same span in 64-frame blocks, each event at a block's start.
        let mut want = [Vec::with_capacity(256), Vec::with_capacity(256)];
        for k in 0..4 {
            if let Some(e) = events(big * 4 + k) {
                apply(&mut host, e);
            }
            let at = frame + (k * 64) as u64;
            let out = contract::drive_in(
                &mut short,
                &env(at, 64, host.transport(), TransportChanges::NONE),
                &[],
                &[],
                &[],
            )
            .audio;
            for (w, o) in want.iter_mut().zip(out) {
                w.extend(o);
            }
            host.advance(64);
        }

        for c in 0..2 {
            for i in 0..256 {
                assert_eq!(
                    got[c][i].to_bits(),
                    want[c][i].to_bits(),
                    "block {big}, channel {c}, frame {i}: a 256-frame block read {} \
                     against 64-frame blocks' {}",
                    got[c][i],
                    want[c][i]
                );
                heard[c] |= want[c][i].abs() > 0.1;
            }
        }
        let _ = streamer.step_once();
    }
    assert_eq!(heard, [true, true], "silence proves nothing");
}
