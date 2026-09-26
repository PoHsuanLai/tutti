//! The block read (`PlaybackSlot::process_into`) against the frame read it
//! replaced (doc 013 item 7), bit for bit.
//!
//! The oracle below is the per-sample read as it stood before item 7, kept
//! verbatim apart from where it writes: every voice read a frame at a time
//! (`seated_position` per frame, the stretch filter's and the disk reader's
//! `tick` per frame, gain per frame) and added into the output one sample at
//! a time. Two pools are given the same voices; one renders through the block
//! read, one through the oracle, on one clock, and every output sample must
//! have the same bits.
//!
//! **What it isolates is the block machinery, not the kernels.** The two
//! sides share the interpolation (`cubic_hermite`, `tap_indices`, the loop's
//! taps), the vocoder, and the seat's rules: the oracle's memory arm seats
//! with `Seat::next` per frame, and its disk arm reads through
//! `DiskVoice::tick`, a one-frame `live_render`, whose `Seat::run` over one
//! frame is exactly one `Seat::next` (and one ring claim per frame). So this
//! pins the lanes, the per-block seating and claiming, `hermite_lanes`,
//! `filter_lanes`, the gain and the mix against a per-frame composition of
//! the same kernels; a bug inside a shared kernel shows on both sides and is
//! the kernel's own tests' (`interp`, `loop_span`, `stretch`, the tier
//! tables) to catch.

use std::path::Path;
use std::sync::Arc;

use tutti_core::{
    Amplitude, AudioUnit, Beat, Bpm, BufferVec, Cents, ChannelLayout, PlaybackRate, ReadRate,
    SamplePosition, SampleRate, StretchFactor, Timeline,
};
use tutti_io::Wave;

use super::memory_source::{LoopSetting, MemorySource};
use super::pool::VoicePool;
use super::slot::PlaybackSlot;
use super::types::{Direction, Playback, SlotId, Voice, VoiceSource};
use crate::test_transport::MockTransport;
use crate::{Command, DiskStreamer, DiskStreamerConfig, MAX_SAMPLER_CHANNELS};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
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

/// The per-sample read `PlaybackSlot::process_into` was before doc 013 item
/// 7, adding voice `slot`'s `size` frames into `out` (`n` channels).
fn frame_read(slot: &mut PlaybackSlot, size: usize, n: usize, out: &mut [[f32; BLOCK]]) {
    let direction = slot.voice.play.direction;
    let gain = slot.voice.play.gain;
    let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
    let read_clip = |sampler: &MemorySource, pos: SamplePosition, out: &mut [f32]| {
        sampler.read_placed_into(pos, direction, out);
        let g = gain.get();
        for s in out.iter_mut() {
            *s *= g;
        }
    };
    let mix = |frame: &[f32], i: usize, out: &mut [[f32; BLOCK]]| {
        for (c, &s) in frame.iter().enumerate().take(n) {
            out[c][i] += s;
        }
    };
    let stretching = slot.needs_stretch() && slot.stretch.is_some();
    if stretching {
        let mut raw = [0.0f32; MAX_SAMPLER_CHANNELS];
        let unit = slot.stretch.as_mut().expect("stretching");
        match &mut slot.voice.source {
            VoiceSource::Memory(sampler) => {
                let stretch_rate = unit.input_rate();
                for i in 0..size {
                    let Some(pos) = sampler.seated_position(stretch_rate) else {
                        continue;
                    };
                    read_clip(sampler, pos, &mut raw[..n]);
                    frame[..n].fill(0.0);
                    unit.tick(&raw[..n], &mut frame[..n]);
                    mix(&frame[..n], i, out);
                }
            }
            VoiceSource::Disk(reader) => {
                reader.set_stretch_rate(unit.input_rate());
                for i in 0..size {
                    raw[..n].fill(0.0);
                    reader.tick(&[], &mut raw[..n]);
                    frame[..n].fill(0.0);
                    unit.tick(&raw[..n], &mut frame[..n]);
                    mix(&frame[..n], i, out);
                }
            }
        }
    } else {
        match &mut slot.voice.source {
            VoiceSource::Memory(sampler) => {
                for i in 0..size {
                    let Some(pos) = sampler.seated_position(ReadRate::UNITY) else {
                        continue;
                    };
                    read_clip(sampler, pos, &mut frame[..n]);
                    mix(&frame[..n], i, out);
                }
            }
            VoiceSource::Disk(reader) => {
                reader.set_stretch_rate(ReadRate::UNITY);
                for i in 0..size {
                    frame[..n].fill(0.0);
                    reader.tick(&[], &mut frame[..n]);
                    mix(&frame[..n], i, out);
                }
            }
        }
    }
}

fn memory(w: &Arc<Wave>, clock: &Arc<MockTransport>, start: f64) -> VoiceSource {
    let mut s = MemorySource::with_transport(
        Arc::clone(w),
        Arc::clone(clock) as Arc<dyn Timeline>,
        Beat::new(start),
        None,
    );
    s.set_sample_rate(SampleRate(SR));
    VoiceSource::Memory(s)
}

fn play(f: impl FnOnce(&mut Playback)) -> Playback {
    let mut p = Playback::default();
    f(&mut p);
    p
}

/// **The block read is the frame read, bit for bit**, summed over a pool of
/// voices covering every arm the read forks on: in memory at unity, at 1.37x
/// varispeed and half gain, reversed, on a crossfaded loop, stretched 0.5x,
/// pitched +700 cents, stretched and pitched at half gain, a mono wave fanned out, a
/// 24 kHz wave on a 48 kHz clock, a six-channel wave folded to stereo, a
/// window that opens mid-render (plain, and stretched); from disk live at
/// unity, at 0.75x, and stretched 1.5x; and a disk voice forked offline.
/// Through a transport seek (a re-seat, a ring jump, a stretch flush) and a
/// stop and restart.
///
/// Every mutation below was run and fails this test: the lane read one frame
/// off (`&lane[1..frames]` into `[..frames - 1]` in `process_into`'s
/// accumulate); the pool's accumulate skipping its last voice; the memory
/// arm's gain not applied; the stretched arm's gain not applied before the
/// filter (`scale(raw, 1.0)`); `Seat::run` stepping `k / 2 * 2` frames;
/// `hermite_lanes` at `t / 2`; a frame with no sample keeping the last
/// block's `live` flag in the node-owned gather; the disk stretch arm
/// publishing unity; the disk lanes written one frame late; a forked disk
/// voice's frame cut to one channel; the stretched memory arm feeding the
/// filter a block outside its window; the mono fan left silent; the reverse
/// mirror one frame off.
#[test]
fn block_render_is_the_frame_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("material.wav");
    write_wav(&path);
    let stereo = wave(2, SR);
    let mono = wave(1, SR);
    let low = wave(2, 24_000.0);
    let six = wave(6, SR);
    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));

    // Disk streams: channels 0..4 for the block pool, 4..8 for the oracle's.
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
            .take_disk_voice(
                channel,
                Arc::clone(&clock) as Arc<dyn Timeline>,
                Beat::new(0.0),
                None,
            )
            .expect("a primed stream gives a voice");
        v.set_sample_rate(SampleRate(SR));
        v
    };

    let build = |first_disk: usize| {
        let (mut pool, _handle) = VoicePool::with_channels(
            Some(Arc::clone(&clock) as Arc<dyn Timeline>),
            None,
            ChannelLayout::STEREO,
        )
        .expect("a width the sampler reads");
        pool.set_sample_rate(SampleRate(SR));
        let mut forked = disk(first_disk + 3);
        forked.isolate();
        forked.rebind_offline(&tutti_core::transport::OfflineTransport::new(Arc::clone(
            &clock,
        )));
        forked.set_sample_rate(SampleRate(SR));
        let voices: Vec<(VoiceSource, Playback)> = vec![
            (memory(&stereo, &clock, 0.0), Playback::default()),
            (
                memory(&stereo, &clock, 0.0),
                play(|p| {
                    p.speed = PlaybackRate::new(1.37);
                    p.gain = Amplitude::new(0.5);
                }),
            ),
            (
                memory(&stereo, &clock, 0.0),
                play(|p| p.direction = Direction::Reverse),
            ),
            (
                memory(&stereo, &clock, 0.0),
                play(|p| {
                    p.loop_ = LoopSetting::On {
                        start: SamplePosition(1_000.0),
                        end: SamplePosition(9_000.0),
                        crossfade_frames: 256,
                    }
                }),
            ),
            (
                memory(&stereo, &clock, 0.0),
                play(|p| p.stretch = StretchFactor::new(0.5)),
            ),
            (
                memory(&stereo, &clock, 0.0),
                play(|p| p.pitch = Cents::new(700.0)),
            ),
            (
                memory(&stereo, &clock, 0.0),
                play(|p| {
                    p.stretch = StretchFactor::new(1.5);
                    p.pitch = Cents::new(-300.0);
                    // The stretched arm applies the gain before the filter.
                    p.gain = Amplitude::new(0.5);
                }),
            ),
            (memory(&mono, &clock, 0.0), Playback::default()),
            (memory(&low, &clock, 0.0), Playback::default()),
            (memory(&six, &clock, 0.0), Playback::default()),
            (memory(&stereo, &clock, 1.0), Playback::default()),
            // Stretched, and silent until its window opens: the filter must
            // be fed nothing before then.
            (
                memory(&stereo, &clock, 1.0),
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
    let mut block = build(0);
    let mut oracle = build(4);
    assert_eq!(block.voice_count(), 16);

    let input = BufferVec::new(0);
    let mut output = BufferVec::new(2);
    let mut heard = [false; 2];
    for b in 0..600 {
        match b {
            // A seek back: every voice re-seats, the disk voices jump.
            300 => clock.seek(Beat::new(0.3)),
            // Stopped for a while, then rolling again where it stood.
            450 => clock.set_rolling(false),
            470 => clock.set_rolling(true),
            _ => {}
        }
        block.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        oracle.flush_on_seek(BLOCK);
        let mut want = [[0.0f32; BLOCK]; 2];
        for slot in &mut oracle.voices {
            frame_read(slot, BLOCK, 2, &mut want);
        }
        let got = output.buffer_ref();
        for (c, want) in want.iter().enumerate() {
            for (i, &w) in want.iter().enumerate() {
                assert_eq!(
                    got.at_f32(c, i).to_bits(),
                    w.to_bits(),
                    "block {b}, channel {c}, frame {i}: block read {} against frame \
                     read {w}",
                    got.at_f32(c, i)
                );
                heard[c] |= w.abs() > 0.1;
            }
        }
        if clock.is_rolling() {
            clock.advance(BLOCK as i64, SR);
        }
        let _ = streamer.step_once();
    }
    assert_eq!(heard, [true, true], "silence proves nothing");
}
