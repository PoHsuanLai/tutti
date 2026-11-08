use super::*;
use crate::butler::{RegionBuffer, RegionId};
use crate::testing::{MockTransport, block};
use std::path::PathBuf;
use tutti_graph::contract::Direct;
use tutti_types::{Bpm, SamplePosition, Samples};

/// The rate the tests' blocks run at.
const SR: f64 = 44_100.0;

/// A stopped transport: a free-running source reads none.
fn stopped() -> Arc<MockTransport> {
    MockTransport::stopped(Beat::new(0.0), Bpm::of(120.0))
}

/// One block of `n` frames of a free-running `unit`.
fn free(unit: &mut DiskSource, n: usize) -> Vec<Vec<f32>> {
    block(unit, &stopped(), SR, n)
}

/// One block of `n` frames of `voice` under `t`, at `rate`.
fn under(voice: &mut DiskVoice, t: &MockTransport, rate: f64, n: usize) -> Vec<Vec<f32>> {
    block(voice, t, rate, n)
}

/// Tests still author stereo pairs for readability; flatten them at the one
/// boundary rather than rewriting every fixture. The frames land at
/// straight positions `0..`, where a free-running source starts.
fn make_reader_with_samples(samples: &[(f32, f32)]) -> SharedReader {
    make_reader_at(samples, 0)
}

/// [`make_reader_with_samples`], the frames landing at straight positions
/// `start..` (where a placed voice reads them).
fn make_reader_at(samples: &[(f32, f32)], start: u64) -> SharedReader {
    let (mut writer, reader) =
        RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), samples.len() + 64, 2u16);
    writer.reset(start);
    writer.set_play(start);
    let flat: Vec<f32> = samples.iter().flat_map(|&(l, r)| [l, r]).collect();
    writer.push_interleaved(&flat);
    reader
}

fn make_unit(samples: &[(f32, f32)]) -> (DiskSource, Arc<RtState>) {
    let reader = make_reader_with_samples(samples);
    let state = Arc::new(RtState::new());
    let unit = DiskSource::new(reader, Arc::clone(&state));
    (unit, state)
}

/// At unity rate a free-running source reads one file frame per output
/// frame, exactly: output frame `i` is the ring's frame `i`, and after 8
/// blocks of 64 it stands on frame 511.
///
/// A reader that popped four frames of interpolation head-room per block
/// and dropped them would run 68/64 fast: every streamed file a
/// quarter-tone sharp with its level and waveform intact. Reading by
/// position has no fetch step to get wrong, but the step is still the one
/// quantity that decides pitch, so it stays pinned.
///
/// Mutation (run): the position stepped by `n + 4` frames a block → frame
/// 64 reads 68 → fails.
#[test]
fn process_reads_one_file_frame_per_output_frame_at_unity_rate() {
    const BLOCK: usize = 64;
    const BLOCKS: usize = 8;
    let samples: Vec<(f32, f32)> = (0..2048)
        .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
        .collect();
    let (mut unit, _state) = make_unit(&samples);
    unit.play();

    for block in 0..BLOCKS {
        let out = free(&mut unit, BLOCK);
        for (i, &got) in out[0].iter().enumerate() {
            let frame = block * BLOCK + i;
            assert_eq!(got, samples[frame].0, "output frame {frame}");
        }
    }
    assert_eq!(unit.read.read_to(), (BLOCK * BLOCKS - 1) as f64);
}

// --- DiskVoice: placement gate ---

/// A voice at 44.1 kHz over a ring holding `samples` from straight
/// position 0.
fn make_clip_reader(
    samples: &[(f32, f32)],
    start_beat: Beat,
    duration: Option<BeatDuration>,
) -> DiskVoice {
    make_clip_reader_at(samples, 0, start_beat, duration)
}

/// [`make_clip_reader`] over a ring holding `samples` from `at` on.
fn make_clip_reader_at(
    samples: &[(f32, f32)],
    at: u64,
    start_beat: Beat,
    duration: Option<BeatDuration>,
) -> DiskVoice {
    let state = Arc::new(RtState::new());
    let inner = DiskSource::new(make_reader_at(samples, at), Arc::clone(&state));
    DiskVoice::new(
        inner,
        state,
        DiskVoiceConfig {
            window: VoiceWindow {
                start: start_beat,
                duration,
            },
            file_sample_rate: SampleRate(44100.0),
        },
    )
}

/// A disk voice sizes its block positions from `Prepare::max_block`,
/// and a re-prepare resizes them (512, then 1024, then 64); a voice
/// never prepared (one that only plays in a pool) carries none, and
/// neither does a clone.
///
/// Mutation (run): `prepare` not resizing the positions → 0 at 512 →
/// fails.
#[test]
fn a_disk_voice_sizes_its_positions_from_max_block() {
    let mut voice = make_clip_reader(&[(0.0, 0.0); 8], Beat::new(0.0), None);
    assert_eq!(voice.positions.len(), 0);
    for max in [512usize, 1024, 64] {
        voice.prepare(&Prepare::new(SampleRate(SR), Samples(max)));
        assert_eq!(voice.positions.len(), max, "at {max}");
    }
    assert_eq!(voice.clone().positions.len(), 0);
}

/// A 48 kHz file in a 44.1 kHz session must read where the clock puts
/// the playhead in the *file's* frames — `src_ratio` applied exactly once.
///
/// `file_sample_rate` is the file's rate (`ports.rs` hands over the one
/// the butler recorded), so the gate already measures in file frames;
/// composing the conversion into the gate as well applied it twice. Every
/// other streaming fixture hardcodes 44100 against a default `src_ratio` of
/// 1.0 — the one value that makes a double-multiply invisible. Observed
/// where the reader says it plays (`Ring::play`, what the butler fills).
///
/// Mutation (run): the gate's rate composing the conversion
/// (`window_rate` via `read_rate`) → ~104 490 → fails.
#[test]
fn placement_gate_applies_src_ratio_exactly_once() {
    let samples: Vec<_> = (1..64).map(|i| (i as f32, i as f32)).collect();
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));

    let (inner, state) = make_unit(&samples);
    let ring = Arc::clone(inner.read.ring());
    let src = 48_000.0 / 44_100.0;
    state.set_src_ratio(tutti_types::SrcRatio::new(src as f32));

    let mut reader = DiskVoice::new(
        inner,
        state,
        DiskVoiceConfig {
            window: VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            },
            // The file's rate, as `ports.rs` hands it over: session × src.
            file_sample_rate: SampleRate(44_100.0 * src),
        },
    );

    // Two seconds in at 120 BPM = beat 4.0.
    transport.set_beat(Beat::new(4.0));
    reader.set_render_rate(SampleRate(44_100.0));
    under(&mut reader, &transport, SR, 1);
    let offset = ring.play() as f64;

    // 2 s of a 48 kHz file is 96000 file samples — src_ratio applied once.
    let expected = 2.0 * 48_000.0;
    assert!(
        (offset - expected).abs() < 1.0,
        "expected ~{expected} file samples, got {offset} \
         (a double-applied src_ratio gives ~{})",
        expected * src
    );
}

/// A varispeed change moves where the voice reads, without any beat
/// discontinuity.
///
/// A cursor watching beats is blind to it: at beat 20 of a 120 BPM
/// timeline, 1.0x -> 2.0x relocates the file position by 441 000 frames
/// while the transport reports the same beat, at the same tempo, still
/// rolling. The next block reads where the gate puts the playhead at the
/// new speed and tells the butler so (`Ring::play`), which fills there.
///
/// Mutation (run): the gate's rate ignoring varispeed → the voice reads
/// on near 441 000 → fails.
#[test]
fn a_varispeed_change_moves_the_read_without_any_beat_discontinuity() {
    let samples: Vec<_> = (1..4096)
        .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
        .collect();
    let transport = MockTransport::rolling(Beat::new(20.0), Bpm::of(120.0));
    let mut reader = make_clip_reader(&samples, Beat::new(0.0), None);
    let ring = Arc::clone(reader.inner.read.ring());

    under(&mut reader, &transport, SR, 1);
    let before = ring.play();
    assert_eq!(before, 441_000, "10 s of a 44.1 kHz file");

    let beat_before = transport.beat();
    reader.set_speed(PlaybackRate::new(2.0));
    under(&mut reader, &transport, SR, 1);
    assert_eq!(
        transport.beat(),
        beat_before,
        "the playhead must not have moved; otherwise this proves nothing"
    );
    assert_eq!(ring.play(), 882_000, "at 2x beat 20 is 20 s in");
    // One frame on: two file frames on.
    transport.advance(1, SR);
    under(&mut reader, &transport, SR, 1);
    assert_eq!(ring.play(), 882_002, "a frame later, two file frames on");
}

/// A fork past its window closes its file, and one inside it holds it
/// open: a render of many voices keeps a file open per voice sounding.
/// Mutation (run): the close removed from `offline_render`'s past-the-window
/// branch → still open → fails.
#[test]
fn a_fork_closes_its_file_once_past_its_window() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&path, spec).expect("writes");
    for i in 0..60_000 {
        w.write_sample(i as f32 * 1e-5).expect("writes");
        w.write_sample(i as f32 * 1e-5).expect("writes");
    }
    w.finalize().expect("writes");
    let mut streamer =
        crate::DiskStreamer::manual(SampleRate(48_000.0), Default::default()).expect("builds");
    streamer
        .commands()
        .send(crate::Command::Stream {
            channel_index: 0,
            file_path: path,
            offset: SamplePosition(0.0),
        })
        .expect("the butler is alive");
    let _ = streamer.step_until_settled(1_000);
    // Beats [0, 1): 24 000 frames at 120 BPM.
    let voice = streamer
        .status()
        .take_disk_voice(0, Beat::new(0.0), Some(BeatDuration::new(1.0)))
        .expect("the link is installed");

    let render = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut copy = voice.fork_copy();
    copy.set_render_rate(SampleRate(48_000.0));
    let is_open = |copy: &DiskVoice| {
        copy.offline
            .as_ref()
            .and_then(|offline| offline.read.as_ref())
            .is_some_and(OfflineRead::is_open)
    };
    let mut played = 0;
    while played < 30_000 {
        under(&mut copy, &render, 48_000.0, 64);
        played += 64;
        render.advance(64, 48_000.0);
        if played == 1_024 {
            assert!(is_open(&copy), "the file is open while the clip plays");
        }
    }
    assert!(!is_open(&copy), "the file is still open past the window");
}

/// A stopped clock silences a fork mid-clip, in a 64-frame block and
/// in one-frame ones: the clock stops where it stands (its beat does not
/// move), and the read must not run on as if it still rolled. The
/// placement is the memory tier's too (`memory_source`'s
/// `a_stopped_clock_silences_a_placed_read`).
///
/// Mutation (run): `place` ignoring `run.rolling()` → the read plays on
/// through the stop → fails.
#[test]
fn a_stopped_clock_silences_a_fork() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&path, spec).expect("writes");
    for i in 0..20_000 {
        w.write_sample((i + 1) as f32 * 1e-5).expect("writes");
        w.write_sample((i + 1) as f32 * 1e-5).expect("writes");
    }
    w.finalize().expect("writes");
    let mut streamer =
        crate::DiskStreamer::manual(SampleRate(48_000.0), Default::default()).expect("builds");
    streamer
        .commands()
        .send(crate::Command::Stream {
            channel_index: 0,
            file_path: path,
            offset: SamplePosition(0.0),
        })
        .expect("the butler is alive");
    let _ = streamer.step_until_settled(1_000);
    let voice = streamer
        .status()
        .take_disk_voice(0, Beat::new(0.0), None)
        .expect("the link is installed");

    for single in [false, true] {
        let mut copy = voice.fork_copy();
        copy.set_render_rate(SampleRate(48_000.0));
        let clock = MockTransport::rolling(Beat::new(0.25), Bpm::of(120.0));
        let blk = |copy: &mut DiskVoice| -> Vec<f32> {
            if single {
                (0..64)
                    .map(|_| under(copy, &clock, 48_000.0, 1)[0][0])
                    .collect()
            } else {
                under(copy, &clock, 48_000.0, 64).swap_remove(0)
            }
        };
        assert!(
            blk(&mut copy).iter().all(|&s| s != 0.0),
            "single frames {single}: rolling, the clip plays"
        );
        clock.set_rolling(false);
        assert!(
            blk(&mut copy).iter().all(|&s| s == 0.0),
            "single frames {single}: stopped, the fork plays on"
        );
    }
}

/// A severed source lets go of the live reader. A clone never holds
/// it (the reader is the live source's alone), so the one
/// way a severed copy could still speak for the live ring is a source
/// severed in place (what a disk voice's fork does to its copy's source).
/// Here the live source claims a block, is severed, and, stopped, renders
/// again: the ranges the live block claimed must still stand, and a clone
/// of it renders nothing into them either.
///
/// Mutation (run): `sever` not letting go of the reader (`read.sever()`
/// removed) → the severed source's stopped block idles the reader and
/// clears its ranges → fails.
#[test]
fn a_severed_copy_holds_no_live_reader() {
    let (mut writer, ring) = RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 4_096, 2u16);
    writer.push_interleaved(&[0.5; 2 * 1_000]);
    let mut live = DiskSource::new(ring, Arc::new(RtState::new()));
    free(&mut live, 64);
    let claimed = writer.in_flight_end();
    assert!(claimed > 0, "the live block claimed nothing");
    let mut clone = live.clone();
    clone.play();
    free(&mut clone, 64);
    assert_eq!(
        writer.in_flight_end(),
        claimed,
        "a clone spoke for the live reader"
    );
    live.sever();
    free(&mut live, 64);
    assert_eq!(
        writer.in_flight_end(),
        claimed,
        "a severed source spoke for the live reader"
    );
}

/// A fork of a disk voice never touches the live stream: rendered
/// inside its window on the render's transport, it tells the live butler
/// no position (so the live window stays where the live voice reads), and
/// its controls are its own. What it plays instead is
/// `tests/offline_disk_voice.rs`'s. The node's fork source forks it for an
/// offline render and refuses a live one, which would read the file
/// on the audio thread.
///
/// Mutation (run): `fork_copy` keeping the live `shared_state` → the
/// copy's gain write lands on the live cell → fails. Mutation (run):
/// `fork_copy` returning the copy on the live path over the live cell
/// (before it detaches the cell and hands the copy its file) → fails.
/// Mutation (run): `DiskVoiceFork` not refusing `ForkMode::Live` →
/// fails. (Not caught: `fork_copy` not severing its source alone — a
/// clone of a source never holds the live reader, so severing it again
/// changes nothing; `a_severed_copy_holds_no_live_reader` pins `sever`.)
#[test]
fn a_fork_never_touches_the_live_stream() {
    let samples: Vec<_> = (1..4096).map(|i| (i as f32, i as f32)).collect();
    let live = make_clip_reader(&samples, Beat::new(0.0), None);
    let ring = Arc::clone(live.inner.read.ring());
    let (play_before, window_before) = (ring.play(), ring.window());

    let render = MockTransport::rolling(Beat::new(1.0), Bpm::of(120.0));
    let mut copy = live.fork_copy();
    for _ in 0..16 {
        under(&mut copy, &render, SR, 64);
        render.advance(64, SR);
    }
    copy.set_gain(Amplitude::new(0.25));

    assert_eq!(ring.play(), play_before, "a copy moved the live read");
    assert_eq!(ring.window(), window_before, "a copy touched the live ring");
    assert_eq!(
        live.gain(),
        Amplitude::new(1.0),
        "a copy moved the live gain"
    );

    let source = DiskVoiceFork(live.clone());
    assert!(source.fork(ForkMode::Offline).is_ok());
    let refused = source
        .fork(ForkMode::Live)
        .expect_err("a live fork is refused");
    assert!(refused.downcast_ref::<LiveDiskFork>().is_some());
}

#[test]
fn clip_reader_silent_outside_window_audible_inside() {
    // Voice window: beats [4, 8). Non-zero ramp in the ring where beat 5
    // reads (half a second in: 22 050 frames).
    let samples: Vec<_> = (1..64).map(|i| (i as f32, i as f32)).collect();
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut reader = make_clip_reader_at(
        &samples,
        22_046,
        Beat::new(4.0),
        Some(BeatDuration::new(4.0)),
    );

    // Before the window (beat 0): silence, ring untouched.
    let out = under(&mut reader, &transport, SR, 8);
    assert!(
        out.iter().flatten().all(|&s| s == 0.0),
        "before window → silence"
    );

    // Inside the window (beat 5): reads the ring, produces audio.
    transport.set_beat(Beat::new(5.0));
    let out = under(&mut reader, &transport, SR, 8);
    assert!(
        out.iter().flatten().any(|&s| s != 0.0),
        "inside window → audible"
    );

    // Past the window (beat 9): silent again.
    transport.set_beat(Beat::new(9.0));
    let out = under(&mut reader, &transport, SR, 1);
    assert_eq!([out[0][0], out[1][0]], [0.0, 0.0], "after window → silence");
}

/// A NaN or infinite gain is silence on a disk voice, in a window it plays
/// (a placed read, inside and outside it) and free-running: never NaN.
///
/// Mutation (run): `audible` returning its gain unguarded -> the placed read
/// is NaN inside the window and inf * 0.0 = NaN outside it, and the
/// free-running source is NaN -> fails.
#[test]
fn a_non_finite_gain_is_silence_on_a_disk_voice() {
    let samples: Vec<_> = (1..64).map(|i| (i as f32, i as f32)).collect();
    for gain in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
        let mut reader = make_clip_reader_at(
            &samples,
            22_046,
            Beat::new(4.0),
            Some(BeatDuration::new(4.0)),
        );
        reader.set_gain(Amplitude(gain));
        for beat in [0.0, 5.0, 9.0] {
            transport.set_beat(Beat::new(beat));
            let out = under(&mut reader, &transport, SR, 8);
            assert!(
                out.iter().flatten().all(|&s| s == 0.0),
                "placed, beat {beat}, gain {gain}"
            );
        }

        let samples: Vec<_> = (0..20).map(|_| (1.0f32, -1.0f32)).collect();
        let mut source =
            DiskSource::new(make_reader_with_samples(&samples), Arc::new(RtState::new()));
        source.set_gain(Amplitude(gain));
        let out = free(&mut source, 8);
        assert!(
            out.iter().flatten().all(|&s| s == 0.0),
            "free-running, gain {gain}"
        );
    }
}

#[test]
fn clip_reader_stopped_transport_is_silent() {
    let samples: Vec<_> = (1..32).map(|i| (i as f32, i as f32)).collect();
    let transport = MockTransport::stopped(Beat::new(5.0), Bpm::of(120.0)); // inside window but stopped
    let mut reader = make_clip_reader(&samples, Beat::new(4.0), Some(BeatDuration::new(4.0)));

    let out = under(&mut reader, &transport, SR, 1);
    assert_eq!(
        [out[0][0], out[1][0]],
        [0.0, 0.0],
        "stopped transport → silence"
    );
}

// --- DiskVoice: RT no-alloc (steady-state process inside window) ---
//
// The integration-test gate `tests/rt_no_alloc.rs` cannot reach the
// crate-private ring / `RtState` needed to build a streaming reader, so the streaming-variant no-alloc guard lives here, in-crate, with
// a module-local `AllocDisabler`. The disabler only aborts inside an
// `assert_no_alloc` region; every other unit test allocates normally.

#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// A clip reader inside its window, driven by hand (`Direct`) at 48 kHz
/// in `frames`-frame blocks, warmed up: the enter-window edge settled and
/// the interpolation history primed.
fn warm_clip_reader(transport: &MockTransport, frames: usize) -> Direct<DiskVoice> {
    let samples: Vec<_> = (1..2048)
        .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
        .collect();
    let reader = make_clip_reader_at(
        &samples,
        22_046,
        Beat::new(4.0),
        Some(BeatDuration::new(4.0)),
    );
    let mut direct = Direct::new(reader, SampleRate::new(48_000.0), frames);
    for _ in 0..16 {
        direct.block_in(&transport.env(frames, 48_000.0));
    }
    direct
}

#[test]
fn clip_reader_process_steady_state_is_allocation_free() {
    let transport = MockTransport::rolling(Beat::new(5.0), Bpm::of(120.0)); // inside window
    let mut direct = warm_clip_reader(&transport, 64);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            direct.block_in(&transport.env(64, 48_000.0));
        }
    });
}

/// The jump edge (the playhead jumping within the voice window: the reader
/// copies the old continuation into its scratch and starts a crossfade)
/// must be allocation-free. This exercises it *inside* the guarded block
/// by moving the transport beat each iteration, so a regression that
/// allocates on the jump path is caught.
#[test]
fn clip_reader_jump_edge_is_allocation_free() {
    let transport = MockTransport::rolling(Beat::new(5.0), Bpm::of(120.0)); // inside [4, 8)
    let mut direct = warm_clip_reader(&transport, 64);
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            // Jump the playhead back and forth inside the window so the
            // reader keeps taking a scratch copy and starting a fade.
            let beat = if i % 2 == 0 { 5.0 } else { 6.5 };
            transport.set_beat(Beat::new(beat));
            direct.block_in(&transport.env(64, 48_000.0));
        }
    });
}

/// One-frame blocks, the shortest a node is handed: the per-block work
/// (the clock's runs, the ring's claim) allocates nothing either.
#[test]
fn clip_reader_single_frame_steady_state_is_allocation_free() {
    let transport = MockTransport::rolling(Beat::new(5.0), Bpm::of(120.0));
    let mut direct = warm_clip_reader(&transport, 1);
    for _ in 0..256 {
        direct.block_in(&transport.env(1, 48_000.0));
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            direct.block_in(&transport.env(1, 48_000.0));
        }
    });
}

/// A disk voice re-prepared at a 512-frame `max_block` renders ragged
/// blocks up to it without allocating: prepared at 64 first, then at
/// 512 (its positions resized on the control thread), then driven in
/// blocks of 1 to 512 frames — the longest in one piece through its own
/// positions — on a transport that moves and winds back inside the
/// window (a jump, and its crossfade, every few hundred blocks).
///
/// Mutation (run): `process` building its positions per block
/// (`vec![None; frames]`) → aborts.
#[test]
fn clip_reader_ragged_blocks_at_max_block_512_are_allocation_free() {
    const RAGGED: [usize; 10] = [1, 37, 100, 511, 512, 64, 200, 129, 3, 65];
    let transport = MockTransport::rolling(Beat::new(5.0), Bpm::of(120.0));
    let samples: Vec<_> = (1..2048)
        .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
        .collect();
    let reader = make_clip_reader_at(
        &samples,
        22_046,
        Beat::new(4.0),
        Some(BeatDuration::new(4.0)),
    );
    let reader = tutti_graph::contract::prepared(reader, SampleRate::new(48_000.0), 64);
    let mut direct = Direct::new(reader, SampleRate::new(48_000.0), 512);
    let step = |i: usize, direct: &mut Direct<DiskVoice>| {
        let len = RAGGED[i % RAGGED.len()];
        direct.block_in(&transport.env(len, 48_000.0));
        transport.advance(len as i64, 48_000.0);
        if transport.beat() >= Beat::new(7.5) {
            transport.seek(Beat::new(5.0));
        }
    };
    for i in 0..32 {
        step(i, &mut direct);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            step(i, &mut direct);
        }
    });
}

/// Frame `k`, channel `c` of the [`wide_clip`] rings: distinct per channel
/// and per frame, and exact in `f32`.
fn wide_value(k: u64, c: usize) -> f32 {
    (c + 1) as f32 + k as f32 / 1024.0
}

/// A voice placed at beat 0 over a `width`-channel ring holding frames
/// `22_000..24_000` ([`wide_value`]), at 44.1 kHz on both sides: at beat 1
/// (120 BPM) it reads whole frames `22_050 + i`, where the cubic at
/// `t` = 0 returns the frame itself.
fn wide_clip(width: usize) -> DiskVoice {
    const AT: u64 = 22_000;
    let (mut writer, reader) = RegionBuffer::with_capacity(
        RegionId(1),
        PathBuf::new(),
        4096,
        ChannelLayout::try_from(width).expect("a test width is small"),
    );
    writer.reset(AT);
    writer.set_play(AT);
    let flat: Vec<f32> = (AT..AT + 2_000)
        .flat_map(|k| (0..width).map(move |c| wide_value(k, c)))
        .collect();
    writer.push_interleaved(&flat);
    let state = Arc::new(RtState::new());
    let inner = DiskSource::new(reader, Arc::clone(&state));
    DiskVoice::new(
        inner,
        state,
        DiskVoiceConfig {
            window: VoiceWindow::default(),
            file_sample_rate: SampleRate(SR),
        },
    )
}

/// What lane `c` of the caller holds at straight position `k` from a
/// `width`-channel ring: the frame path's `put` zips the reader's frame
/// (the ring's width) with the lanes, and a lane past the reader is
/// silent (`render_lanes` clears it).
fn wide_expected(width: usize, k: u64, c: usize) -> f32 {
    if c < width { wide_value(k, c) } else { 0.0 }
}

/// A steady block read writes the caller's lanes as the frame path's
/// `put` does, for a reader wider than the caller (stereo into mono, 6
/// channels into stereo), as wide, narrower, and mono: each lane is the
/// reader's channel and a lane past the reader is silent. Every run is
/// read along time (`steady_lanes`), and after the first block the read
/// allocates nothing.
///
/// Mutation (run): the lanes cut to the reader's width
/// (`&mut lanes[..w]`) → the 2-into-1 row panics, out of range.
/// Mutation (run): `steady_lanes` answering `false` up front → no run
/// is steady → fails. Mutation (run): `lanes_n` taken as the reader's
/// width alone → the 2-into-1 row panics.
#[test]
fn a_steady_lane_read_writes_the_callers_lanes_as_put_does() {
    const BLOCK: usize = 64;
    for (width, n) in [
        (2usize, 1usize),
        (6, 2),
        (2, 2),
        (6, 6),
        (2, 6),
        (1, 2),
        (1, 1),
    ] {
        let mut voice = wide_clip(width);
        let mut scratch = crate::lanes::BlockScratch::new(BLOCK, n);
        let mut clock = crate::voice::clock::Clock::new();
        let t = MockTransport::rolling(Beat::new(1.0), Bpm::of(120.0));
        let mut block = |voice: &mut DiskVoice| {
            let env = t.env(BLOCK, SR);
            let clock = clock.observe(&env);
            let crate::lanes::BlockScratch {
                out,
                positions,
                gather,
                ..
            } = &mut scratch;
            voice.render_lanes(&clock, 0..BLOCK, out, n, positions, gather);
            t.advance(BLOCK as i64, SR);
        };
        block(&mut voice);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 1..8 {
                block(&mut voice);
            }
        });
        assert_eq!(voice.inner.read.steady_runs(), 8, "{width} into {n}");
        // The last block: straight positions 22 050 + 7 × 64 + i.
        let first = 22_050 + 7 * BLOCK as u64;
        for c in 0..n {
            for (i, &got) in scratch.out.lanes()[c][..BLOCK].iter().enumerate() {
                let want = wide_expected(width, first + i as u64, c);
                assert_eq!(
                    got.to_bits(),
                    want.to_bits(),
                    "{width} into {n}: lane {c}, frame {i}"
                );
            }
        }
    }
}

/// [`wide_clip`] over a ring that states `topology` (its width is the
/// topology's), so the voice places the ring's channels by it.
fn placed_clip(topology: tutti_types::ChannelTopology) -> DiskVoice {
    const AT: u64 = 22_000;
    let width = topology.positions().len();
    let (mut writer, reader) =
        RegionBuffer::for_file(RegionId(1), PathBuf::new(), 4096, topology, usize::MAX);
    writer.reset(AT);
    writer.set_play(AT);
    let flat: Vec<f32> = (AT..AT + 2_000)
        .flat_map(|k| (0..width).map(move |c| wide_value(k, c)))
        .collect();
    writer.push_interleaved(&flat);
    let state = Arc::new(RtState::new());
    let inner = DiskSource::new(reader, Arc::clone(&state));
    DiskVoice::new(
        inner,
        state,
        DiskVoiceConfig {
            window: VoiceWindow::default(),
            file_sample_rate: SampleRate(SR),
        },
    )
}

/// A 7.1 file in WAV order: the rears before the sides.
fn wav_seven_one() -> tutti_types::ChannelTopology {
    use tutti_types::Speaker::*;
    tutti_types::ChannelTopology::new([
        FrontLeft,
        FrontRight,
        FrontCenter,
        LowFrequency,
        BackLeft,
        BackRight,
        SideLeft,
        SideRight,
    ])
}

/// A 10-channel file with no mask, as the decoder names it: WAVE's first
/// ten speakers.
fn named_ten() -> tutti_types::ChannelTopology {
    use tutti_types::Speaker::*;
    tutti_types::ChannelTopology::new([
        FrontLeft,
        FrontRight,
        FrontCenter,
        LowFrequency,
        BackLeft,
        BackRight,
        FrontLeftOfCenter,
        FrontRightOfCenter,
        BackCenter,
        SideLeft,
    ])
}

/// The eight channels a placed 10-channel file ([`named_ten`]) plays in the
/// engine's 7.1 order (FL FR FC LFE SL SR BL BR), from file channel values
/// `v`, by `Fold`'s documented rules and not by `Place`: a speaker the
/// destination has goes straight across; the left and right of centre fold
/// into the centre at -3 dB; the back centre into both backs at -3 dB; the
/// right side has no source, so it is silent.
fn ten_into_seven_one(v: impl Fn(usize) -> f32) -> [f32; 8] {
    let m = tutti_types::M3DB;
    [
        v(0),
        v(1),
        v(2) + m * v(6) + m * v(7),
        v(3),
        v(9),
        0.0,
        v(4) + m * v(8),
        v(5) + m * v(8),
    ]
}

/// Render 8 blocks of `voice` (n lanes) through `render_lanes` as the pool
/// does, the last 7 under `assert_no_alloc`; the last block's lanes and the
/// number of steady (read along time) runs.
fn render_lanes_blocks(voice: &mut DiskVoice, n: usize) -> (Vec<Vec<f32>>, usize) {
    const BLOCK: usize = 64;
    let mut scratch = crate::lanes::BlockScratch::new(BLOCK, n);
    let mut clock = crate::voice::clock::Clock::new();
    let t = MockTransport::rolling(Beat::new(1.0), Bpm::of(120.0));
    let mut block = |voice: &mut DiskVoice| {
        let env = t.env(BLOCK, SR);
        let clock = clock.observe(&env);
        let crate::lanes::BlockScratch {
            out,
            positions,
            gather,
            ..
        } = &mut scratch;
        voice.render_lanes(&clock, 0..BLOCK, out, n, positions, gather);
        t.advance(BLOCK as i64, SR);
    };
    block(voice);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 1..8 {
            block(voice);
        }
    });
    let lanes = scratch.out.lanes()[..n]
        .iter()
        .map(|l| l[..BLOCK].to_vec())
        .collect();
    (lanes, voice.inner.read.steady_runs())
}

/// A placed ring is read through its placement in a block read by lanes (the
/// pool's), not along time: a WAV-order 7.1 comes out in the engine's order and
/// a 10-channel file folded into 8, with no allocation, and neither run is
/// steady (`steady_lanes` reads whole channels, which only a ring read as it
/// stands may be).
///
/// Mutation (run): `&& place.is_direct()` removed from `steady_lanes`' gate
/// -> the 7.1 (as wide as the reader) is read along time in the file's order:
/// its steady runs are 8, not 0, and its rears and sides are exchanged -> fails.
/// Mutation (run): `read_ring` passing `Place::FAN` -> the 7.1 is in the file's
/// order and the fold's 10 channels are cut -> fails.
#[test]
fn a_placed_ring_is_read_through_its_placement_by_lanes() {
    const BLOCK: usize = 64;
    // The last block: straight positions 22 050 + 7 x 64 + i.
    let first = 22_050 + 7 * BLOCK as u64;

    let mut voice = placed_clip(wav_seven_one());
    let (lanes, steady) = render_lanes_blocks(&mut voice, 8);
    assert_eq!(steady, 0, "a permuted ring was read along time");
    // Engine channel -> file channel: FL FR FC LFE SL SR BL BR.
    for (lane, file) in lanes.iter().zip([0usize, 1, 2, 3, 6, 7, 4, 5]) {
        for (i, &got) in lane.iter().enumerate() {
            let want = wide_value(first + i as u64, file);
            assert_eq!(got.to_bits(), want.to_bits(), "7.1: file channel {file}");
        }
    }

    let mut voice = placed_clip(named_ten());
    let (lanes, steady) = render_lanes_blocks(&mut voice, 8);
    assert_eq!(steady, 0, "a folded ring was read along time");
    for i in 0..BLOCK {
        let want = ten_into_seven_one(|c| wide_value(first + i as u64, c));
        for (c, lane) in lanes.iter().enumerate() {
            assert!(
                (lane[i] - want[c]).abs() < 1e-4,
                "10 into 8: lane {c}, frame {i}: {} against {}",
                lane[i],
                want[c]
            );
        }
    }
}

/// A jump inside a placed ring (the old continuation read from the ring, and a
/// crossfade started) allocates nothing, for a permuted and a folded ring.
///
/// Mutation (run): `Place::frame` collecting the fold's source into a `Vec`
/// -> the 10-channel row allocates in the guarded region and aborts.
#[test]
fn a_placed_ring_jump_edge_is_allocation_free() {
    for topology in [wav_seven_one(), named_ten()] {
        let transport = MockTransport::rolling(Beat::new(1.0), Bpm::of(120.0));
        let mut direct = Direct::new(placed_clip(topology), SampleRate::new(SR), 64);
        for _ in 0..16 {
            direct.block_in(&transport.env(64, SR));
        }
        assert_no_alloc::assert_no_alloc(|| {
            for i in 0..2_000 {
                // Within the 2 000 frames the ring holds from 22 000.
                let beat = if i % 2 == 0 { 1.0 } else { 1.04 };
                transport.set_beat(Beat::new(beat));
                direct.block_in(&transport.env(64, SR));
            }
        });
    }
}

/// A streamed clip wider than its pool renders through the pool, the
/// pool's channels taking the clip's first ones: a stereo clip in a mono
/// pool, a 6-channel clip in a stereo pool, over 8 rolling blocks.
///
/// Mutation (run): the steady read's lanes cut to the reader's width
/// (`&mut lanes[..w]`) → panics on the first block ("range end index 2
/// out of range for slice of length 1") → fails.
#[test]
fn a_streamed_clip_wider_than_its_pool_mixes_its_first_channels() {
    use crate::voice::{Playback, SlotId, Voice, VoicePool, VoiceSource};
    const BLOCK: usize = 64;
    for (width, pool_width) in [(2usize, 1usize), (6, 2)] {
        let mut pool = VoicePool::with_channels(
            None,
            ChannelLayout::try_from(pool_width).expect("a test width is small"),
        )
        .expect("a sampler width");
        pool.insert_voice(
            SlotId(0),
            Voice {
                source: VoiceSource::Disk(wide_clip(width)),
                play: Playback::default(),
                channel_index: None,
            },
        );
        let pool = tutti_graph::contract::prepared(pool, SampleRate(SR), BLOCK);
        let mut direct = Direct::new(pool, SampleRate(SR), BLOCK);
        let t = MockTransport::rolling(Beat::new(1.0), Bpm::of(120.0));
        for b in 0..8u64 {
            direct.block_in(&t.env(BLOCK, SR));
            t.advance(BLOCK as i64, SR);
            for c in 0..pool_width {
                for (i, &got) in direct.output(c).iter().enumerate() {
                    let k = 22_050 + b * BLOCK as u64 + i as u64;
                    assert_eq!(
                        got.to_bits(),
                        wide_value(k, c).to_bits(),
                        "{width} into {pool_width}: block {b}, channel {c}, frame {i}"
                    );
                }
            }
        }
    }
}

// --- Existing tests ---

// --- New: play/stop state ---

#[test]
fn play_stop_controls_output() {
    let samples: Vec<_> = (0..100).map(|i| (i as f32, -(i as f32))).collect();
    let (mut unit, _state) = make_unit(&samples);

    assert!(unit.is_playing());

    // Play while playing — should produce non-zero after history primes.
    free(&mut unit, 5);

    unit.stop();
    assert!(!unit.is_playing());
    let out = free(&mut unit, 1);
    assert_eq!(out[0][0], 0.0, "stopped unit must output silence");
    assert_eq!(out[1][0], 0.0);

    unit.play();
    assert!(unit.is_playing());
    let out = free(&mut unit, 1);
    assert_ne!(out[0][0], 0.0, "resumed unit should produce audio");
}

// --- New: one-frame blocks read interpolated output from the ring ---

#[test]
fn single_frames_read_from_ring_buffer_and_interpolate() {
    // Feed a ramp 0,1,2,...,19 into the ring buffer. After enough
    // frames to prime the 4-sample history, output should be
    // non-zero and monotonically increasing (speed=1, src_ratio=1).
    let samples: Vec<_> = (0..20).map(|i| (i as f32, i as f32)).collect();
    let (mut unit, _state) = make_unit(&samples);

    let mut prev = f32::NEG_INFINITY;
    for i in 0..16 {
        let out = free(&mut unit, 1)[0][0];
        if i >= 4 {
            assert!(
                out >= prev,
                "ramp should be monotonic at frame {i}: prev={prev}, got={out}"
            );
        }
        prev = out;
    }
    assert!(prev > 0.0, "should have produced non-zero audio");
}

// --- New: process block reads from ring buffer ---

#[test]
fn process_block_produces_output() {
    let samples: Vec<_> = (0..256)
        .map(|i| (i as f32 * 0.01, -(i as f32) * 0.01))
        .collect();
    let (mut unit, _state) = make_unit(&samples);

    let output = free(&mut unit, 64);

    // After 64 frames at speed=1, output should contain interpolated
    // samples from the ramp. Check last few are non-zero.
    let last = output[0][63];
    assert!(last > 0.0, "process() should produce audio, got {last}");
}

#[test]
fn process_block_silence_when_stopped() {
    let samples: Vec<_> = (0..256).map(|i| (i as f32, 0.0)).collect();
    let (mut unit, _state) = make_unit(&samples);
    unit.stop();

    let output = free(&mut unit, 16);

    for ch in &output[..2] {
        assert_eq!(ch.len(), 16);
        assert!(ch.iter().all(|&s| s == 0.0));
    }
}

// --- New: gain application ---

#[test]
fn gain_scales_the_output() {
    let samples: Vec<_> = (0..20).map(|_| (1.0f32, -1.0f32)).collect();

    let reader1 = make_reader_with_samples(&samples);
    let reader2 = make_reader_with_samples(&samples);
    let state1 = Arc::new(RtState::new());
    let state2 = Arc::new(RtState::new());

    let mut full = DiskSource::new(reader1, state1);
    let mut half = DiskSource::new(reader2, state2);
    half.set_gain(Amplitude::new(0.5));

    // Prime history then compare
    let out_full = free(&mut full, 6)[0][5];
    let out_half = free(&mut half, 6)[0][5];

    assert!(out_full.abs() > 1e-6, "the full-gain source sounds");
    let ratio = out_half / out_full;
    assert!(
        (ratio - 0.5).abs() < 0.05,
        "gain=0.5 should halve output: full={out_full}, half={out_half}, ratio={ratio}"
    );
}

// --- New: reset clears interpolation state ---

#[test]
fn reset_clears_state() {
    let samples: Vec<_> = (0..100).map(|i| (i as f32, 0.0)).collect();
    let (mut unit, _state) = make_unit(&samples);

    free(&mut unit, 10);

    unit.reset();
    assert!(!unit.is_playing());
    assert!(
        unit.read.forgot(),
        "the reader still holds its last position"
    );
}

/// A free-running source follows a relayed seek, from its next block
/// (`Command::Seek` reaches it as `Ring::request_seek`), and a source taken
/// after a seek starts there, not at the stream's origin.
///
/// Mutation (run): the seek epoch not compared (`applied_seek` never set)
/// → the source re-seeks every block and repeats frame 100 → fails.
/// Mutation (run): `applied_seek` starting at the current epoch → the late
/// source starts at 0 → fails.
#[test]
fn a_free_running_source_follows_a_relayed_seek() {
    let samples: Vec<_> = (0..512).map(|i| (i as f32, 0.0)).collect();
    let (mut unit, _state) = make_unit(&samples);
    let ring = Arc::clone(unit.read.ring());
    free(&mut unit, 8);
    ring.request_seek(100);
    // No fade on this bare ring (its fade length is 0): the next block
    // reads the target at once, and the one after reads on from it.
    for want in [100.0, 108.0] {
        assert_eq!(free(&mut unit, 8)[0][0], want);
    }
    // A source taken after a seek: on a stream of its own, since a ring
    // serves one live reader (this test took a second off the same ring
    // before that rule).
    let seeked = make_reader_with_samples(&samples);
    seeked.request_seek(100);
    let mut late = DiskSource::new(seeked, Arc::new(RtState::new()));
    assert_eq!(free(&mut late, 1)[0][0], 100.0, "starts at the seek");
}

/// Build a `channels`-wide ring pre-filled with `frames` interleaved frames.
fn make_wide_reader(frames: &[f32], channels: usize) -> SharedReader {
    let (mut writer, reader) = RegionBuffer::with_capacity(
        RegionId(1),
        PathBuf::new(),
        frames.len() / channels + 64,
        ChannelLayout::try_from(channels).expect("a test width is small"),
    );
    writer.push_interleaved(frames);
    reader
}

/// Outside its transport window a voice must silence EVERY channel.
///
/// An `if output.len() >= 2 { .. }` guard silences nothing at all at width
/// 1, so a mono streaming voice outside its window leaks whatever the
/// caller's buffer already held; at width 6 the same site writes only
/// channels 0 and 1, leaving 2..6 holding the previous block. Both are
/// invisible at width 2, which is what every other streaming fixture uses.
#[test]
fn outside_the_window_silences_every_channel() {
    for width in [1usize, 6] {
        let frames: Vec<f32> = (0..64 * width).map(|i| (i + 1) as f32).collect();
        let ring = make_wide_reader(&frames, width);
        let state = Arc::new(RtState::new());
        let inner = DiskSource::new(ring, state.clone());
        assert_eq!(
            inner.channels(),
            ChannelLayout::try_from(width).expect("a test width is small"),
            "ring width must reach the unit"
        );

        // Transport parked BEFORE the voice's start beat, so the placement
        // gate reports "outside".
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
        let voice = DiskVoice::new(
            inner,
            state,
            DiskVoiceConfig {
                window: VoiceWindow {
                    start: Beat::new(64.0),
                    duration: None,
                },
                file_sample_rate: SampleRate(44100.0),
            },
        );

        // Pre-dirty the node's outputs so a missing write shows, in a
        // one-frame block and an eight-frame one.
        let mut direct = Direct::new(voice, SampleRate(SR), 8);
        for frames in [1usize, 8] {
            for ch in direct.outputs_mut() {
                ch.fill(9.0);
            }
            let mut env = transport.env(8, SR);
            env.block_len = Samples(frames);
            if frames == 8 {
                direct.block_in(&env);
            } else {
                // `Direct` runs its own block length; a one-frame block
                // is a node of its own.
                let mut one = Direct::new(direct.node.clone(), SampleRate(SR), 1);
                for ch in one.outputs_mut() {
                    ch.fill(9.0);
                }
                one.block_in(&env);
                for c in 0..width {
                    assert_eq!(
                        one.output(c)[0],
                        0.0,
                        "width {width}, one frame: channel {c} not silenced"
                    );
                }
                continue;
            }
            for c in 0..width {
                for i in 0..frames {
                    assert_eq!(
                        direct.output(c)[i],
                        0.0,
                        "width {width}: channel {c} sample {i} not silenced"
                    );
                }
            }
        }
    }
}

/// The ring's stride must index its slots exactly as `channels.count()`
/// would — at a width where getting it wrong is audible.
///
/// Width 6 and a constant-per-channel ring make a stride error *visible*:
/// the slots are frame-major, so a wrong stride reads tap `t` of channel
/// `c` from a different channel's slot and every output carries a
/// neighbour's value. At width 2 a stride bug and a correct stride coincide
/// for several access patterns, which is exactly why this is not a stereo
/// test.
#[test]
fn cached_stride_reads_every_channel_at_width_six() {
    let width = 6usize;
    // Every frame is [1, 2, 3, 4, 5, 6]: constant per channel, distinct
    // across channels. Constant in time so cubic interpolation over any four
    // taps of one channel returns that channel's own value exactly — the
    // output then names which channel each slot was read from.
    let frames: Vec<f32> = (0..128)
        .flat_map(|_| (1..=width).map(|c| c as f32))
        .collect();
    let ring = make_wide_reader(&frames, width);
    let state = Arc::new(RtState::new());
    let mut unit = DiskSource::new(ring, state.clone());

    assert_eq!(
        unit.channels(),
        ChannelLayout::from(6u16),
        "the ring's declared width must reach the unit as a layout"
    );
    assert_eq!(
        unit.shape().audio_out.count() as usize,
        width,
        "the cached stride must agree with the declared layout"
    );

    // The block reads frames 0..64, the ring's own.
    let buf = free(&mut unit, 64);
    // The last quarter of the block is well past priming.
    for i in 48..64 {
        for (c, ch) in buf.iter().enumerate().take(width) {
            let got = ch[i];
            let want = (c + 1) as f32;
            assert!(
                (got - want).abs() < 1e-4,
                "sample {i} channel {c}: read {got}, want {want} — \
                 a wrong stride reads a neighbouring channel's tap"
            );
        }
    }

    // One-frame blocks read through the same path, so they must land on
    // the same frame.
    let mut frame = free(&mut unit, 1);
    for _ in 0..7 {
        frame = free(&mut unit, 1);
    }
    for (c, ch) in frame.iter().enumerate() {
        let want = (c + 1) as f32;
        assert!(
            (ch[0] - want).abs() < 1e-4,
            "one-frame block, channel {c}: read {}, want {want}",
            ch[0]
        );
    }
}

/// A gain change must reach a voice that is already rendering, written
/// through a clone of it — and the clone renders nothing.
///
/// The clone half of the live-value rule, at the disk tier: a gain stored
/// by value in `DiskSource` would be written on one copy and never
/// reach the one that renders. The gain lives in the stream's control
/// cell (`RtState`), which every clone shares (a fork detaches it,
/// `fork_copy`).
///
/// The graph renders the node it was given, and the only copy it takes
/// (a fork, `fork_copy`) never renders the live stream, so the reader is
/// the original's alone. What the graph does, and this pins: the original renders at the
/// gain a copy wrote (a copy holding the cell is how a host's handle,
/// `DiskVoiceControls`, reaches it), and a clone, holding no reader,
/// renders silence — it cannot claim the live ring.
///
/// Mutation (run): `LiveRead::clone` keeping the reader (a shared
/// `Arc<Mutex<PosReader>>` again) → the clone renders the stream → fails.
/// Mutation (run): `DiskSource::set_gain` writing a by-value field instead
/// of the shared cell → the original renders at unity → fails.
#[test]
fn a_gain_change_reaches_a_cloned_voice() {
    let (mut unit, _state) = make_unit(&[(1.0, 1.0); 256]);
    unit.prepare(&Prepare::new(SampleRate(48_000.0), Samples(64)));
    unit.play();

    // The clone stands in for a copy the host writes through; the
    // original is what the graph renders.
    let mut copy = unit.clone();
    copy.set_gain(Amplitude::new(0.25));

    let peak = |u: &mut DiskSource| {
        let mut peak = 0.0f32;
        for _ in 0..4 {
            for s in &free(u, 8)[0] {
                peak = peak.max(s.abs());
            }
        }
        peak
    };
    let rendered = peak(&mut unit);
    assert!(
        (rendered - 0.25).abs() < 1e-4,
        "a gain written on one copy of the voice must be seen by the copy \
         that renders; expected ~0.25, got {rendered}. A value near 1.0 \
         means `gain` is stored by value and the write went nowhere."
    );
    let cloned = peak(&mut copy);
    assert_eq!(
        cloned, 0.0,
        "a clone must hold no reader of the live ring, so it renders \
         silence; it rendered {cloned}"
    );
}

// --- The silent-block skip ---

/// Switches the skip off for the life of the guard, on this thread.
struct NoSkip;

impl NoSkip {
    fn on(off: bool) -> Self {
        NO_SKIP.with(|c| c.set(off));
        Self
    }
}

impl Drop for NoSkip {
    fn drop(&mut self) {
        NO_SKIP.with(|c| c.set(false));
    }
}

/// Transport script for the skip tests: `(rolling, blocks)` runs, with
/// an optional seek (beat) taken before the run.
type Script = [(bool, usize, Option<f64>)];

/// Render `script` through a one-voice pool over `wide_clip(2)`, a block
/// of 64 at a time, returning each block's output bits (both channels).
/// The playhead moves only while rolling.
fn scripted_pool(script: &Script, no_skip: bool) -> Vec<[Vec<u32>; 2]> {
    use crate::voice::{Playback, SlotId, Voice, VoicePool, VoiceSource};
    const BLOCK: usize = 64;
    let _guard = NoSkip::on(no_skip);
    let mut pool = VoicePool::with_channels(None, 2u16).expect("a sampler width");
    pool.insert_voice(
        SlotId(0),
        Voice {
            source: VoiceSource::Disk(wide_clip(2)),
            play: Playback::default(),
            channel_index: None,
        },
    );
    let pool = tutti_graph::contract::prepared(pool, SampleRate(SR), BLOCK);
    let mut direct = Direct::new(pool, SampleRate(SR), BLOCK);
    let t = MockTransport::rolling(Beat::new(1.0), Bpm::of(120.0));
    let mut blocks = Vec::new();
    for &(rolling, count, seek) in script {
        if let Some(beat) = seek {
            t.seek(Beat::new(beat));
        }
        t.set_rolling(rolling);
        for _ in 0..count {
            direct.block_in(&t.env(BLOCK, SR));
            if rolling {
                t.advance(BLOCK as i64, SR);
            }
            blocks.push([0, 1].map(|c| direct.output(c).iter().map(|s| s.to_bits()).collect()));
        }
    }
    blocks
}

/// A streamed voice that is silent, then audible, then silent, then
/// audible renders the same bits with the skip as without it, and
/// what it plays is the frame the clock names: a stopped transport holds
/// the playhead, so the voice resumes at the frame it stopped on, and a
/// seek lands on its own frame. Silent blocks are exactly +0.0.
///
/// Mutation (run): `render_lanes` answering `false` for every block (the
/// skip applied to a sounding one) → the rolling blocks are silent →
/// fails. Mutation (run): the skip never taken (`if false`) → this test
/// still passes (the renders agree), which is why the other tests count
/// the skips.
#[test]
fn a_silent_streamed_voice_resumes_at_the_right_frame_and_matches_the_unskipped_render() {
    const BLOCK: usize = 64;
    let script: &Script = &[
        (false, 3, None),
        (true, 4, None),
        (false, 3, None),
        (true, 4, None),
        (false, 2, None),
        // Back to the clip's first audible beat.
        (true, 4, Some(1.0)),
    ];
    let skipped = scripted_pool(script, false);
    let unskipped = scripted_pool(script, true);
    assert_eq!(skipped, unskipped, "the skip changed the render");

    // Straight position of the first frame of each block, `None` while
    // stopped.
    let mut want: Vec<Option<u64>> = Vec::new();
    let mut pos = 22_050u64;
    for &(rolling, count, seek) in script {
        if seek.is_some() {
            pos = 22_050;
        }
        for _ in 0..count {
            want.push(rolling.then_some(pos));
            if rolling {
                pos += BLOCK as u64;
            }
        }
    }
    for (b, (block, first)) in skipped.iter().zip(&want).enumerate() {
        for (c, lane) in block.iter().enumerate() {
            for (i, &bits) in lane.iter().enumerate() {
                let got = f32::from_bits(bits);
                match first {
                    None => assert_eq!(bits, 0, "block {b} is stopped, frame {i}"),
                    Some(first) => {
                        let w = wide_value(first + i as u64, c);
                        assert!(
                            (got - w).abs() < 1e-4,
                            "block {b}, channel {c}, frame {i}: {got} for {w}"
                        );
                    }
                }
            }
        }
    }
}

/// A block with no frame to play skips the lanes and the ring, and
/// one with a frame does not: `render_lanes` answers `false` and leaves
/// the caller's lanes as they were while the transport stands, and
/// `true` (every lane written) once it rolls.
///
/// Mutation (run): the skip never taken (`if false`) → the stopped block
/// returns `true` and clears the sentinel → fails. Mutation (run): the
/// skip always taken (`if true`) → the rolling block returns `false` →
/// fails.
#[test]
fn a_block_with_no_position_leaves_the_lanes_alone() {
    const BLOCK: usize = 64;
    let mut voice = wide_clip(2);
    let mut scratch = crate::lanes::BlockScratch::new(BLOCK, 2);
    let mut clock = crate::voice::clock::Clock::new();
    let t = MockTransport::stopped(Beat::new(1.0), Bpm::of(120.0));
    let mut run = |voice: &mut DiskVoice, scratch: &mut crate::lanes::BlockScratch| {
        for lane in scratch.out.lanes_mut() {
            lane.fill(7.0);
        }
        let env = t.env(BLOCK, SR);
        let clock = clock.observe(&env);
        let crate::lanes::BlockScratch {
            out,
            positions,
            gather,
            ..
        } = scratch;
        voice.render_lanes(&clock, 0..BLOCK, out, 2, positions, gather)
    };
    // The skip allocates nothing.
    assert_no_alloc::assert_no_alloc(|| {
        assert!(
            !run(&mut voice, &mut scratch),
            "a stopped transport sounds nothing"
        );
    });
    for lane in &scratch.out.lanes()[..2] {
        assert!(lane[..BLOCK].iter().all(|&s| s == 7.0), "lanes written");
    }
    t.set_rolling(true);
    assert!(run(&mut voice, &mut scratch), "a rolling transport sounds");
    assert!(scratch.out.lanes()[0][..BLOCK].iter().all(|&s| s != 7.0));
    assert!(voice.inner.read.read_to() > 22_000.0);
}

/// A skipped block forgets a crossfade in progress, as the rendered
/// zeros do: a jump starts a fade, a silent block ends it and the
/// position it continued.
///
/// Mutation (run): `LiveRead::silence` not clearing `st.fade` → the fade
/// survives the silent block → fails. Mutation (run): not clearing
/// `st.last` → fails.
#[test]
fn a_skipped_block_forgets_the_fade_and_the_last_position() {
    const BLOCK: usize = 64;
    const AT: u64 = 22_000;
    let (mut writer, reader) = RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 4096, 1u16);
    RegionBuffer::place(&mut writer, AT, 0, 0, 256);
    let from = writer.window().0;
    let flat: Vec<f32> = (from..from + 2_000).map(|k| k as f32).collect();
    writer.push_interleaved(&flat);
    let state = Arc::new(RtState::new());
    let inner = DiskSource::new(reader, Arc::clone(&state));
    let mut voice = DiskVoice::new(
        inner,
        state,
        DiskVoiceConfig {
            window: VoiceWindow::default(),
            file_sample_rate: SampleRate(SR),
        },
    );
    let mut scratch = crate::lanes::BlockScratch::new(BLOCK, 1);
    let mut clock = crate::voice::clock::Clock::new();
    let t = MockTransport::rolling(Beat::new(1.0), Bpm::of(120.0));
    let mut run = |voice: &mut DiskVoice| {
        let env = t.env(BLOCK, SR);
        let clock = clock.observe(&env);
        let crate::lanes::BlockScratch {
            out,
            positions,
            gather,
            ..
        } = &mut scratch;
        voice.render_lanes(&clock, 0..BLOCK, out, 1, positions, gather)
    };
    assert!(run(&mut voice));
    assert!(!voice.inner.read.forgot(), "a position is held");
    t.advance(BLOCK as i64, SR);
    t.set_beat(Beat::new(1.03));
    assert!(run(&mut voice));
    assert!(voice.inner.read.fading(), "a jump fades");
    t.set_rolling(false);
    assert!(!run(&mut voice), "a stopped block is skipped");
    assert!(voice.inner.read.forgot(), "a silent block forgets");
}

/// A skipped block lets go of the ring, as a rendered silent block
/// does: the reader's last claim is cleared and every shrink so far
/// acknowledged, so a butler that moved the window while the voice was
/// silent is not held off by the stale claim. The ring is 4 096 frames
/// wide; the butler restarts it where it aliases the slots the last
/// block read, and its write is refused until the reader has
/// acknowledged.
///
/// Mutation (run): `LiveRead::silence` not calling `reader.idle()` →
/// the second push lands nothing → fails.
#[test]
fn a_skipped_block_releases_the_ring_to_the_writer() {
    const BLOCK: usize = 64;
    const AT: u64 = 22_000;
    let (mut writer, reader) = RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 4096, 1u16);
    writer.reset(AT);
    writer.set_play(AT);
    let flat: Vec<f32> = (AT..AT + 2_000).map(|k| k as f32).collect();
    writer.push_interleaved(&flat);
    let state = Arc::new(RtState::new());
    let inner = DiskSource::new(reader, Arc::clone(&state));
    let mut voice = DiskVoice::new(
        inner,
        state,
        DiskVoiceConfig {
            window: VoiceWindow::default(),
            file_sample_rate: SampleRate(SR),
        },
    );
    let mut scratch = crate::lanes::BlockScratch::new(BLOCK, 1);
    let mut clock = crate::voice::clock::Clock::new();
    let t = MockTransport::rolling(Beat::new(1.0), Bpm::of(120.0));
    let mut run = |voice: &mut DiskVoice| {
        let env = t.env(BLOCK, SR);
        let clock = clock.observe(&env);
        let crate::lanes::BlockScratch {
            out,
            positions,
            gather,
            ..
        } = &mut scratch;
        voice.render_lanes(&clock, 0..BLOCK, out, 1, positions, gather)
    };
    assert!(run(&mut voice), "the voice reads the ring");
    // The butler restarts the window on slots the last block read
    // (22 050 and 26 146 share one).
    const AGAIN: u64 = 26_000;
    writer.reset(AGAIN);
    writer.set_play(AGAIN);
    let more: Vec<f32> = (AGAIN..AGAIN + 200).map(|k| k as f32).collect();
    let first = writer.push_interleaved(&more);
    // Held at the first slot the last block read: 26 143 = 22 047 + 4 096.
    assert_eq!(first.get(), 143, "the unacknowledged claim holds the write");
    t.set_rolling(false);
    assert!(!run(&mut voice), "a stopped block is skipped");
    let rest = writer.push_interleaved(&more[first.get()..]);
    assert_eq!(rest.get(), 200 - 143, "the skipped block released the ring");
}

/// Render `script` over a 40-second mono stream the butler refills (it
/// is stepped by hand once per block, so nothing is timing-dependent),
/// returning every block's bits and the underruns counted. A skipped
/// block is rendered as the zeros it stands for.
fn butler_fed(script: &Script, no_skip: bool) -> (Vec<Vec<u32>>, u64, usize) {
    use crate::{Command, DiskStreamer, DiskStreamerConfig, StepOutcome};
    const BLOCK: usize = 64;
    let _guard = NoSkip::on(no_skip);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tone.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SR as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&path, spec).expect("create wav");
    for i in 0..(SR as usize * 40) {
        let t = i as f32 / SR as f32;
        // Distinct at every frame: a drifting read is heard.
        w.write_sample((std::f32::consts::TAU * (200.0 + 10.0 * t) * t).sin() * 0.5)
            .expect("write wav");
    }
    w.finalize().expect("finalize wav");
    let mut streamer =
        DiskStreamer::manual(SR, DiskStreamerConfig::default()).expect("streamer builds");
    streamer
        .commands()
        .send(Command::Stream {
            channel_index: 0,
            file_path: path,
            offset: SamplePosition(0.0),
        })
        .expect("the butler is alive");
    for _ in 0..4096 {
        if streamer.step_once() != StepOutcome::Busy {
            break;
        }
    }
    let mut voice = streamer
        .status()
        .take_disk_voice(0, Beat::new(0.0), None)
        .expect("a primed stream gives a voice");
    voice.set_render_rate(SampleRate(SR));
    let mut scratch = crate::lanes::BlockScratch::new(BLOCK, 1);
    let mut clock = crate::voice::clock::Clock::new();
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let (mut blocks, mut skipped) = (Vec::new(), 0);
    for &(rolling, count, seek) in script {
        if let Some(beat) = seek {
            t.seek(Beat::new(beat));
        }
        t.set_rolling(rolling);
        for _ in 0..count {
            let env = t.env(BLOCK, SR);
            let clock = clock.observe(&env);
            let crate::lanes::BlockScratch {
                out,
                positions,
                gather,
                ..
            } = &mut scratch;
            let sounded = voice.render_lanes(&clock, 0..BLOCK, out, 1, positions, gather);
            skipped += usize::from(!sounded);
            blocks.push(if sounded {
                scratch.out.lanes()[0][..BLOCK]
                    .iter()
                    .map(|s| s.to_bits())
                    .collect()
            } else {
                vec![0; BLOCK]
            });
            if rolling {
                t.advance(BLOCK as i64, SR);
            }
            streamer.step_once();
        }
    }
    let underruns = voice.shared_state.take_underruns();
    (blocks, underruns, skipped)
}

/// The butler's ring stays consistent across skipped blocks: a
/// stream played, held silent for 300 blocks (the butler stepping all
/// the while), then resumed where it stopped and after a seek 20 s on,
/// renders the same bits and counts the same underruns with the skip as
/// without it. The skip tells the ring the reader holds nothing, as the
/// silent frames did, so the butler neither starves nor is held back.
/// Also pins that the skip is taken (300 + 50 blocks) and not taken with
/// it off.
///
/// The `reader.idle()` release is pinned by
/// `a_skipped_block_releases_the_ring_to_the_writer`, not here: this
/// script's butler is never held off by a stale claim. Mutation (run):
/// `silence` leaving `st.last` set → the resume after the seek is read
/// as a continuation, not a jump (no crossfade) → bits differ → fails.
#[test]
fn the_butlers_ring_is_consistent_across_skipped_blocks() {
    let script: &Script = &[
        (true, 40, None),
        (false, 300, None),
        (true, 150, None),
        (false, 50, None),
        // 20 s at 120 BPM is beat 40.
        (true, 200, Some(40.0)),
    ];
    let (on, on_underruns, on_skipped) = butler_fed(script, false);
    let (off, off_underruns, off_skipped) = butler_fed(script, true);
    assert_eq!(on_skipped, 350, "the stopped blocks are skipped");
    assert_eq!(off_skipped, 0, "the switch turns the skip off");
    assert_eq!(on_underruns, off_underruns, "the ring's underruns differ");
    assert_eq!(on, off, "the skip changed the render");
    let heard = |from: usize, to: usize| {
        on[from..to]
            .iter()
            .flatten()
            .any(|&b| f32::from_bits(b).abs() > 0.05)
    };
    assert!(heard(0, 40), "the stream plays");
    assert!(heard(340, 490), "the stream resumes");
    assert!(heard(540, 740), "the stream plays after the seek");
}
