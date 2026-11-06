use super::*;
use crate::testing::MockTransport;
use tutti_types::Bpm;

fn ramp_wave(len: usize, sample_rate: f64) -> Arc<Wave> {
    let samples: Vec<f32> = (0..len).map(|i| (i + 1) as f32).collect();
    Arc::new(Wave::from_samples(sample_rate, &samples))
}

fn stereo_ramp_wave(len: usize, sample_rate: f64) -> Arc<Wave> {
    let mut wave = Wave::zero(2, sample_rate, len as f64 / sample_rate);
    for i in 0..len {
        wave.set(0, i, (i + 1) as f32);
        wave.set(1, i, -((i + 1) as f32));
    }
    Arc::new(wave)
}

/// The rate the tests' blocks run at (the transport's frames).
const SR: f64 = 44_100.0;

/// One block of `n` frames of `unit` under `t` (a stopped transport when
/// `None`: a free-running source reads none), at [`SR`]; the transport
/// is not moved. Every output channel, planar.
fn planes(unit: &mut MemorySource, t: Option<&MockTransport>, n: usize) -> Vec<Vec<f32>> {
    let stopped = MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0));
    crate::testing::block(unit, t.unwrap_or(&stopped), SR, n)
}

/// [`planes`]' first two channels, as `(left, right)` frames (a mono
/// node's one channel twice).
fn block(unit: &mut MemorySource, t: Option<&MockTransport>, n: usize) -> Vec<(f32, f32)> {
    let p = planes(unit, t, n);
    let r = p.len().min(2) - 1;
    (0..n).map(|i| (p[0][i], p[r][i])).collect()
}

/// `n` one-frame blocks, the transport not moved between them.
fn frames(unit: &mut MemorySource, t: Option<&MockTransport>, n: usize) -> Vec<(f32, f32)> {
    (0..n).map(|_| block(unit, t, 1)[0]).collect()
}

/// One one-frame block into `out` (its channels, as many as `out` holds).
fn frame_into(unit: &mut MemorySource, t: Option<&MockTransport>, out: &mut [f32]) {
    let p = planes(unit, t, 1);
    for (o, c) in out.iter_mut().zip(p) {
        *o = c[0];
    }
}

// --- block-size equivalence ---
//
// A node is handed blocks of any length, so N one-frame blocks — the
// transport moved a frame between each, as a host moves it — must equal
// one N-frame block sample for sample.
//
// KNOWN LIMIT: these are CONSISTENCY checks, not correctness ones. Both
// run the same read, so a change moves them together. What catches a
// wrong placed read is `placed_clip_reads_across_a_block_not_dc` and
// `placed_clip_block_step_follows_playback_rate`, which assert the shape
// of the output *within* one block.

/// `n` one-frame blocks with the playhead moving a frame between each
/// must equal one `n`-frame block. `transport` is the clock both units
/// play under; `None` for free-running units.
///
/// Advancing matters: with a frozen playhead every placed frame derives the
/// same position, both paths emit the same constant, and the assertion holds
/// no matter what the code does. So does a render that is silent (a
/// playhead past the wave's end), which is why the frames must vary.
///
/// Mutation (run): `render_range` reading every placed frame at the
/// piece's first position (`positions[0]`) → the `n`-frame block is DC
/// → "placed @0.5x" fails. (Until the render had to vary, that case
/// stood at beat 1 — 22 050 frames into a 4 096-frame wave — and passed
/// on silence under this mutation.)
fn assert_blocks_match_frames(
    mut a: MemorySource,
    mut b: MemorySource,
    n: usize,
    transport: Option<&Arc<MockTransport>>,
    case: &str,
) {
    let single: Vec<(f32, f32)> = (0..n)
        .map(|_| {
            let f = block(&mut a, transport.map(|t| &**t), 1)[0];
            if let Some(t) = transport {
                t.advance(1, SR);
            }
            f
        })
        .collect();

    // Rewind so the block sees the same span the frames just walked.
    if let Some(t) = transport {
        t.advance(-(n as i64), SR);
    }
    let whole = block(&mut b, transport.map(|t| &**t), n);

    assert!(
        single.windows(2).any(|w| w[0] != w[1]),
        "{case}: the frames do not vary, so the comparison proves nothing"
    );
    assert_eq!(
        single, whole,
        "{case}: {n} one-frame blocks diverged from one {n}-frame block"
    );
}

#[test]
fn one_block_matches_single_frames_free_running() {
    let wave = ramp_wave(64, 44100.0);
    assert_blocks_match_frames(
        MemorySource::new(Arc::clone(&wave)),
        MemorySource::new(wave),
        16,
        None,
        "free-running",
    );
}

#[test]
fn one_block_matches_single_frames_at_non_unity_speed() {
    let wave = ramp_wave(256, 44100.0);
    let build = || {
        let mut u = MemorySource::new(Arc::clone(&wave));
        u.set_speed(PlaybackRate::new(1.5));
        u
    };
    assert_blocks_match_frames(build(), build(), 32, None, "free-running @1.5x");
}

#[test]
fn one_block_matches_single_frames_when_placed() {
    // The case that was actually broken: a placed voice at non-unity speed.
    let wave = ramp_wave(4096, 44100.0);
    // Beat 0.1: 2 205 frames in, read at 0.5x, inside the wave.
    let transport = MockTransport::rolling(Beat::new(0.1), Bpm::new(120.0));
    let build = || {
        MemorySource::with_config(
            Arc::clone(&wave),
            MemorySourceConfig {
                speed: PlaybackRate::new(0.5),
                placement: Some(VoiceWindow::default()),
                ..Default::default()
            },
        )
    };
    assert_blocks_match_frames(build(), build(), 32, Some(&transport), "placed @0.5x");
}

#[test]
fn one_block_matches_single_frames_across_a_loop_wrap() {
    // Span the loop boundary so the wrap arithmetic runs inside the block.
    let wave = ramp_wave(64, 44100.0);
    let build = || {
        MemorySource::with_config(
            Arc::clone(&wave),
            MemorySourceConfig {
                loop_setting: LoopSetting::On {
                    start: SamplePosition::new(0.0),
                    end: SamplePosition::new(8.0),
                    crossfade_frames: 0,
                },
                ..Default::default()
            },
        )
    };
    assert_blocks_match_frames(build(), build(), 32, None, "loop wrap");
}

// --- varispeed actually reaches a placed voice ---
//
// The equivalence tests above cannot catch this class on their own: both
// lengths run the same read, so any change affects them identically.
// These pin the *behaviour* instead — that speed reaches the placed path at
// all. A read deriving position without the rate plays a placed voice at
// 1x no matter what speed was set, and the equivalence pair stays green.

fn placed_unit(wave: &Arc<Wave>, rate: f32) -> MemorySource {
    MemorySource::with_config(
        Arc::clone(wave),
        MemorySourceConfig {
            speed: PlaybackRate::new(rate),
            placement: Some(VoiceWindow::default()),
            ..Default::default()
        },
    )
}

#[test]
fn placed_clip_honours_speed_in_one_frame() {
    // A ramp wave encodes position in its amplitude, so the sample value at
    // a fixed playhead names which source frame was read.
    // Beat 0.25 @ 120 BPM / 44.1 kHz = 5512.5 samples in, comfortably
    // inside a 16k wave at both 1x and 0.5x.
    let wave = ramp_wave(16_384, 44100.0);
    let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));

    let mut unity = placed_unit(&wave, 1.0);
    let mut half = placed_unit(&wave, 0.5);

    let a = block(&mut unity, Some(&transport), 1)[0].0;
    let b = block(&mut half, Some(&transport), 1)[0].0;

    assert!(a > 0.0 && b > 0.0, "both should be sounding: {a}, {b}");
    assert!(
        (b - a / 2.0).abs() < 2.0,
        "at 0.5x the voice should be half as far in ({a} -> expected ~{}, got {b})",
        a / 2.0
    );
}

#[test]
fn placed_clip_honours_speed_in_a_block() {
    // The same assertion through a longer block, so the pair documents
    // that the two agree for the right reason.
    let wave = ramp_wave(16_384, 44100.0);
    let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));

    let mut unity = placed_unit(&wave, 1.0);
    let mut half = placed_unit(&wave, 0.5);

    let a = block(&mut unity, Some(&transport), 4)[0].0;
    let b = block(&mut half, Some(&transport), 4)[0].0;

    assert!((b - a / 2.0).abs() < 2.0, "expected ~{}, got {b}", a / 2.0);
}

/// A placed voice must read ACROSS a block, not emit one frozen frame.
///
/// A block's `Env` carries the transport at its first frame, so deriving
/// position from that beat alone gives every sample in the block the same
/// value. The result is constant DC where the material should be moving.
/// Caught only by asserting *within* one block: the block-length
/// equivalence tests cannot see it, because a frozen transport freezes
/// both paths identically.
#[test]
fn placed_clip_reads_across_a_block_not_dc() {
    let wave = ramp_wave(16_384, 44100.0);
    let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
    let mut u = placed_unit(&wave, 1.0);

    let block = block(&mut u, Some(&transport), 8);
    let first = block[0].0;
    let last = block[7].0;

    assert!(first > 0.0, "voice should be sounding, got {first}");
    assert!(
        last > first,
        "a ramp wave must rise across the block; got constant DC \
         (first={first}, last={last}) — the transport only moves between \
         blocks, so position must step by read_rate within one"
    );
    // Unity rate over a ramp: one source sample per output sample.
    let step = (last - first) / 7.0;
    assert!(
        (step - 1.0).abs() < 0.01,
        "at 1x the ramp should advance ~1 sample per output frame, got {step}"
    );
}

/// Same, at half speed: the block must still rise, but half as fast.
#[test]
fn placed_clip_block_step_follows_playback_rate() {
    let wave = ramp_wave(16_384, 44100.0);
    let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
    let mut u = placed_unit(&wave, 0.5);

    let block = block(&mut u, Some(&transport), 8);
    let step = (block[7].0 - block[0].0) / 7.0;
    assert!(
        (step - 0.5).abs() < 0.01,
        "at 0.5x the in-block step should be ~0.5 samples/frame, got {step}"
    );
}

/// A placed wave at another rate than the clock's steps by the
/// conversion, across block boundaries, in 64-frame blocks and in
/// one-frame ones: a 24 kHz ramp on a 48 kHz clock reads file frame
/// `n / 2` on output frame `n`, a monotone read half a frame per frame,
/// with no jump where a block starts.
///
/// Stepping by the gate's rate (`window_rate`, varispeed alone) read one
/// file frame per output frame within a block and re-seated half a block
/// back at the next (frames 60…63, then 32).
///
/// Mutation (run): `placed_positions`' step `read_rate` → `window_rate`
/// → output frame 1 reads file frame 1 → fails (64-frame blocks).
#[test]
fn a_placed_wave_at_another_rate_steps_by_the_conversion() {
    let wave = ramp_wave(4_096, 24_000.0);
    for len in [64usize, 1] {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let mut u = placed_unit(&wave, 1.0);
        u.set_render_rate(SampleRate::new(48_000.0));
        let out = crate::testing::play(&mut u, &transport, 48_000.0, 512, len);
        // From frame 4: the first frames' taps clamp at the file's start.
        for (n, &l) in out[0].iter().enumerate().skip(4) {
            let want = n as f32 / 2.0 + 1.0;
            assert!(
                (l - want).abs() < 1e-3,
                "{len}-frame blocks: output frame {n} read {l}, want file frame {} ({want})",
                n as f32 / 2.0
            );
        }
    }
}

/// A stretched placed read seats and steps with the stretch, at another
/// rate too — the positions the stretched slot branch feeds its filter.
/// A 24 kHz wave on a 48 kHz clock at a stretch read rate of 0.5: each
/// block seats where the gate puts it with the stretch (16 file frames per
/// 64-frame block) and steps a quarter frame per output frame (the
/// conversion's half, times the stretch's), so the read is one line
/// across every block.
///
/// Asserted on the positions, not on the filter's output: the phase
/// vocoder hides a wrong step (a 440 Hz source read at twice the rate
/// within each block and re-seated at the next still measures 440 Hz), so
/// a pitch test cannot see this. Whether the slot passes the filter's rate
/// here at all is `a_stretched_placed_voice_holds_its_pitch_across_blocks`'s.
///
/// Mutation (run): the step `read_rate` → `window_rate` → half a frame per
/// frame → fails. Mutation (run): the seat by `window_position` (no
/// stretch: `gate` without `stretch_rate`) → each block seats at 32 →
/// fails.
#[test]
fn a_stretched_placed_read_steps_by_the_conversion_and_the_stretch() {
    let wave = ramp_wave(4_096, 24_000.0);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
    let mut u = placed_unit(&wave, 1.0);
    u.set_render_rate(SampleRate::new(48_000.0));
    let mut clock = Clock::new();
    for block in 0..4 {
        let env = transport.env(64, 48_000.0);
        let bc = clock.observe(&env);
        let mut out = [None; 64];
        u.placed_positions(&bc, 0..64, ReadRate(0.5), &mut out);
        for (i, pos) in out.iter().enumerate() {
            let pos = pos.expect("inside the window");
            let want = (block * 64 + i) as f64 * 0.25;
            assert!(
                (pos.get() - want).abs() < 1e-9,
                "block {block}, frame {i}: position {}, want {want}",
                pos.get()
            );
        }
        transport.advance(64, 48_000.0);
    }
}

/// The interpolator's taps wrap through the loop (the memory tier): at
/// half speed over a hard loop `[10, 20)`, a
/// position half a frame before the end interpolates frames 18, 19, then
/// 10, 11 — what the loop plays next — not 20, 21 from past it; and after
/// the wrap, half a frame into the loop, the frame behind is 19, the one
/// the loop just played. The ramp's value is its frame + 1.
///
/// Mutation (run): `LoopSpan::taps` clamping to the file rather than
/// wrapping → reads 20.5 at 19.5 → fails. Mutation (run): the `looped`
/// back tap removed → frame 9 behind 10.5 → fails. Mutation (run): the
/// wrap not marking the cursor `looped` → the same → fails.
#[test]
fn a_loop_reads_through_its_seam_at_a_fractional_rate() {
    use tutti_types::cubic_hermite;
    let wave = ramp_wave(64, 44_100.0);
    let mut u = MemorySource::new(wave);
    u.set_loop_range(SamplePosition::new(10.0), SamplePosition::new(20.0), 0);
    u.set_speed(PlaybackRate::new(0.5));
    u.trigger_at(SamplePosition::new(19.5));
    let ticks = frames(&mut u, None, 3);
    let v = |frame: usize| (frame + 1) as f32;
    assert_eq!(
        ticks[0].0,
        cubic_hermite(v(18), v(19), v(10), v(11), 0.5),
        "at 19.5"
    );
    // 19.5 + 0.5 = 20.0 wraps to 10.0, which is frame 10 exactly.
    assert_eq!(ticks[1].0, v(10), "at 10.0, after the wrap");
    assert_eq!(
        ticks[2].0,
        cubic_hermite(v(19), v(10), v(11), v(12), 0.5),
        "at 10.5, after the wrap"
    );
}

/// A rate change lands on the next block, at the position the new
/// speed gives: 16 frames at 1×, then 2× on the next block, and the
/// read is seated where the gate puts the playhead at 2× (elapsed time at
/// the new speed — what a varispeed change on a placed voice means) and
/// steps 2 from there. A block reads its rates once, so there is no moment between two frames
/// of one block at which a rate can change.
///
/// Mutation (run): `placed_positions` stepping by `read_rate` without
/// the speed (`SrcRatio` alone) → the second block steps 1 → fails.
#[test]
fn a_rate_change_lands_on_the_next_block() {
    let wave = ramp_wave(16_384, 44_100.0);
    let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
    let mut u = placed_unit(&wave, 1.0);
    let first = block(&mut u, Some(&transport), 16);
    transport.advance(16, SR);
    u.set_speed(PlaybackRate::new(2.0));
    let second = block(&mut u, Some(&transport), 16);
    for w in first.windows(2) {
        assert!((w[1].0 - w[0].0 - 1.0).abs() < 1e-3, "1x steps 1");
    }
    for w in second.windows(2) {
        assert!((w[1].0 - w[0].0 - 2.0).abs() < 1e-3, "2x steps 2");
    }
    // Beat 0.25 + 16 frames, at 2x: twice the elapsed frames in.
    let elapsed = 0.25 * 60.0 / 120.0 * SR + 16.0;
    assert!(
        (second[0].0 - (2.0 * elapsed + 1.0) as f32).abs() < 1e-2,
        "seated at the gate at 2x: {}",
        second[0].0
    );
}

/// A crossfaded loop plays the hand-computed frames — the memory
/// tier's oracle, independent of `LoopSpan`'s arithmetic (the disk fork's
/// is `a_fork_loops_as_the_stream_is_looped_when_it_is_taken`). A ramp
/// (value = frame + 1) at unit speed from frame 0, round the loop twice:
///
/// - `[10, 30)`, a 4-frame fade with room before the start: frame `26 + k`
///   blends toward `6 + k`, weighing it `(k + 1) / 5`, and the wrap lands
///   on 10.
/// - `[2, 30)`, the same fade with only 2 frames before the start: frame
///   `26 + k` blends toward the loop's head `2 + k`, and the wrap lands on
///   6, after the head.
///
/// Mutation (run): the weight `k / fade` in `LoopSpan::fade_at` → frame 26
/// reads the pure tail → fails. Mutation (run): the head mode's `resume`
/// left at `start` → the second loop wraps to 2 → fails.
#[test]
fn a_crossfaded_loop_plays_the_hand_computed_frames() {
    for (start, resume, lead) in [(10usize, 10usize, 6usize), (2, 6, 2)] {
        let mut u = MemorySource::new(ramp_wave(64, 44_100.0));
        u.set_loop_range(
            SamplePosition::new(start as f64),
            SamplePosition::new(30.0),
            4,
        );
        let got: Vec<f32> = frames(&mut u, None, 30 + 2 * (30 - resume))
            .iter()
            .map(|f| f.0)
            .collect();
        let frames = (0..30).chain(resume..30).chain(resume..30);
        for (n, (p, &g)) in frames.zip(got.iter()).enumerate() {
            let want = if p >= 26 {
                let k = p - 26;
                let t = (k + 1) as f32 / 5.0;
                (p + 1) as f32 * (1.0 - t) + (lead + k + 1) as f32 * t
            } else {
                (p + 1) as f32
            };
            assert!(
                (g - want).abs() < 1e-5,
                "loop from {start}: output {n} (frame {p}) read {g}, want {want}"
            );
        }
    }
}

/// A stopped clock silences a placed read mid-clip, in a 64-frame
/// block and in one-frame ones: the clock stops where it stands (its
/// beat does not move), and the read must not run on as if it still
/// rolled.
///
/// Mutation (run): `place` ignoring `run.rolling()` → the read plays on
/// through the stop → fails.
#[test]
fn a_stopped_clock_silences_a_placed_read() {
    let wave = ramp_wave(16_384, 44_100.0);
    for single in [false, true] {
        let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
        let mut u = placed_unit(&wave, 1.0);
        let blk = |u: &mut MemorySource| {
            if single {
                frames(u, Some(&transport), 64)
            } else {
                block(u, Some(&transport), 64)
            }
        };
        assert!(
            blk(&mut u).iter().all(|&(l, _)| l > 0.0),
            "single frames {single}: rolling, the clip plays"
        );
        transport.set_rolling(false);
        assert!(
            blk(&mut u).iter().all(|&(l, r)| l == 0.0 && r == 0.0),
            "single frames {single}: stopped, the read plays on"
        );
    }
}

/// A stop inside a block silences the read on its frame: the block's `Env` carries the stop as a change at frame 40, and
/// the clip plays frames 0..40 and not one more.
///
/// Mutation (run): `place` reading the block's first transport for every
/// frame (ignoring `Env::changes`) → frames 40.. play on → fails.
#[test]
fn a_stop_inside_a_block_silences_the_read_on_its_frame() {
    use tutti_graph::{Offset, TransportChanges};
    let wave = ramp_wave(16_384, 44_100.0);
    let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
    let mut u = placed_unit(&wave, 1.0);
    let mut env = transport.env(64, SR);
    let mut changes = TransportChanges::NONE;
    let stop_beat = Beat::new(0.25 + 40.0 * 2.0 / SR);
    changes
        .push(
            Offset::new(40, Samples(64)).unwrap(),
            tutti_graph::Transport::new(false, Bpm::new(120.0), stop_beat, None),
        )
        .unwrap();
    env.changes = changes;
    let out = tutti_graph::contract::drive_input(&mut u, &env, &[], &[], &[]).audio;
    assert!(
        out[0][..40].iter().all(|&s| s > 0.0),
        "plays up to the stop"
    );
    assert!(
        out[0][40..].iter().all(|&s| s == 0.0),
        "silent from the stop"
    );
}

/// A loop moved under a cursor that has been round the old one reads
/// the file where the cursor is: wrapped once on
/// `[1000, 3000)` to frame 1500, then the loop moved to `[2000, 4000)` —
/// what `VoiceCommand::UpdateLoop` does while a loop point is dragged — the
/// next frames are the file's 1500, 1501, …, not 3500, …. The same after
/// the loop is switched off and on again. The ramp's value is its frame + 1.
///
/// Mutation (run): both guards removed — `LoopSpan::taps` wrapping back
/// every tap behind `resume` when `looped`, and the loop setters leaving
/// `looped` set → 3501 → fails. (Either guard alone holds it;
/// `loop_span`'s own test pins the first.)
#[test]
fn a_loop_moved_under_a_looped_cursor_reads_where_the_cursor_is() {
    for toggle in [false, true] {
        let mut u = MemorySource::new(ramp_wave(8_000, 44_100.0));
        u.set_loop_range(
            SamplePosition::new(1_000.0),
            SamplePosition::new(3_000.0),
            0,
        );
        u.trigger_at(SamplePosition::new(2_999.0));
        frames(&mut u, None, 501);
        assert_eq!(u.position().get(), 1_500.0, "wrapped once, to 1500");
        if toggle {
            u.set_looping(false);
        }
        u.set_loop_range(
            SamplePosition::new(2_000.0),
            SamplePosition::new(4_000.0),
            0,
        );
        let got: Vec<f32> = frames(&mut u, None, 3).iter().map(|f| f.0).collect();
        assert_eq!(got, [1_501.0, 1_502.0, 1_503.0], "toggled {toggle}");
    }
}

// --- loop wrap arithmetic ---

#[test]
fn loop_wrap_handles_overshoot_longer_than_the_loop() {
    // At high varispeed one advance can jump past the loop end by more than
    // the loop's own length. A single `loop_start + (pos - loop_end)`
    // subtraction lands *outside* the region and never recovers; modulo
    // lands inside.
    let wrapped = wrap_into_loop(105.0, 10.0, 20.0);
    assert!(
        (10.0..20.0).contains(&wrapped),
        "overshoot of 8.5 loop lengths must land inside [10, 20), got {wrapped}"
    );
    assert_eq!(wrapped, 15.0);
}

#[test]
fn loop_wrap_is_stable_for_a_single_overshoot() {
    // The common single-overshoot case wraps to the same place a plain
    // subtraction would.
    assert_eq!(wrap_into_loop(22.0, 10.0, 20.0), 12.0);
}

#[test]
fn loop_wrap_survives_a_degenerate_region() {
    // A zero-length region has nothing to wrap into; pin rather than NaN.
    assert_eq!(wrap_into_loop(50.0, 10.0, 10.0), 10.0);
}

// --- Existing tests ---

#[test]
fn test_sampler_outputs_silence_when_stopped() {
    let wave = Wave::with_capacity(1, 44100.0, 100);
    let mut sampler = MemorySource::new(Arc::new(wave));

    sampler.stop();

    let mut output = [0.0f32; 2];
    frame_into(&mut sampler, None, &mut output);

    assert_eq!(output[0], 0.0);
    assert_eq!(output[1], 0.0);
}

#[test]
fn test_loop_crossfade_integration() {
    let samples: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();
    let wave = Wave::from_samples(44100.0, &samples);
    let mut sampler = MemorySource::new(Arc::new(wave));

    sampler.set_loop_range(SamplePosition::new(10.0), SamplePosition::new(90.0), 10);
    sampler.trigger();

    for _ in 0..75 {
        let mut output = [0.0f32; 2];
        frame_into(&mut sampler, None, &mut output);
    }

    let mut output = [0.0f32; 2];
    frame_into(&mut sampler, None, &mut output);

    assert!(sampler.is_playing());
}

// --- New coverage tests ---

#[test]
fn config_default_matches_new() {
    let wave = ramp_wave(100, 44100.0);
    let sampler = MemorySource::with_config(wave, MemorySourceConfig::default());

    // Default config must reproduce `new`'s audible baseline: unity gain,
    // normal speed, one-shot — NOT the newtypes' zero default.
    assert_eq!(sampler.gain(), Amplitude::new(1.0));
    assert_eq!(sampler.speed(), PlaybackRate::new(1.0));
    assert!(!sampler.is_looping());
}

#[test]
fn trigger_at_sets_position() {
    let wave = ramp_wave(100, 44100.0);
    let sampler = MemorySource::new(wave);

    sampler.stop();
    sampler.trigger_at(SamplePosition::new(42.0));
    assert!(sampler.is_playing());
    assert_eq!(sampler.position(), SamplePosition::new(42.0));
}

#[test]
fn reset_clears_position_and_stops() {
    let wave = ramp_wave(100, 44100.0);
    let mut sampler = MemorySource::new(wave);

    // Advance position
    let mut output = [0.0f32; 2];
    for _ in 0..10 {
        frame_into(&mut sampler, None, &mut output);
    }
    assert!(sampler.position().get() > 0.0);
    assert!(sampler.is_playing());

    sampler.reset();
    assert_eq!(sampler.position(), SamplePosition::new(0.0));
    assert!(!sampler.is_playing());
}

#[test]
fn mono_wave_duplicates_to_stereo() {
    let wave = ramp_wave(100, 44100.0);
    let mut sampler = MemorySource::new(wave);

    let mut output = [0.0f32; 2];
    frame_into(&mut sampler, None, &mut output);

    assert_eq!(output[0], output[1]);
    assert!(output[0] > 0.0);
}

#[test]
fn stereo_wave_preserves_channels() {
    let wave = stereo_ramp_wave(100, 44100.0);
    let mut sampler = MemorySource::new(wave);

    let mut output = [0.0f32; 2];
    frame_into(&mut sampler, None, &mut output);

    assert!(output[0] > 0.0);
    assert!(output[1] < 0.0);
    assert!((output[0] + output[1]).abs() < 1e-6);
}

#[test]
fn speed_2x_advances_twice_as_fast() {
    let wave = ramp_wave(100, 44100.0);

    let mut normal = MemorySource::new(Arc::clone(&wave));
    let mut fast = MemorySource::new(wave);
    fast.set_speed(PlaybackRate::new(2.0));

    let mut out = [0.0f32; 2];
    for _ in 0..10 {
        frame_into(&mut normal, None, &mut out);
        frame_into(&mut fast, None, &mut out);
    }

    let normal_pos = normal.position().get();
    let fast_pos = fast.position().get();
    assert!((fast_pos - normal_pos * 2.0).abs() < 1e-6);
}

/// The placement gate must apply `src_ratio` exactly once — at the unit,
/// not just at the kernel.
///
/// The in-memory mirror of `disk_voice`'s
/// `placement_gate_applies_src_ratio_exactly_once`. The trap is passing the
/// full `read_rate` against a rate argument (`wave.sample_rate()`) that has
/// already resolved the beat→sample conversion, which applies `src_ratio`
/// twice.
///
/// The mismatched rate is the whole test. Every other test on this path uses
/// a wave at the session rate, where `src_ratio` is `UNITY` and a doubled
/// factor is exactly 1.0 — an 8.8% position error hides there completely.
///
/// Asserted at the unit rather than only at the kernel because the kernel
/// cannot see this: it takes the rate as an argument, so a caller passing the
/// wrong one is invisible there. Re-introducing the bug in `window_rate` fails
/// only this test.
#[test]
fn placement_gate_applies_src_ratio_exactly_once() {
    // A 48 kHz wave in a 44.1 kHz session.
    let wave = ramp_wave(200_000, 48_000.0);
    let transport = MockTransport::rolling(Beat::new(4.0), Bpm::new(120.0));
    let mut sampler = MemorySource::placed(wave, Beat::new(0.0), None);
    sampler.set_render_rate(SampleRate::new(44_100.0));
    assert!(
        (sampler.src_ratio().get() - (48_000.0 / 44_100.0)).abs() < 1e-4,
        "setup: the session rate must produce a non-unity src_ratio, else \
         this test cannot distinguish one application from two"
    );

    // Beat 4 at 120 BPM is two seconds; two seconds of a 48 kHz file is
    // 96,000 file samples.
    let pos = sampler
        .window_position(&transport.transport())
        .expect("inside the window");
    let expected = 2.0 * 48_000.0;
    let doubled = expected * (48_000.0 / 44_100.0);
    assert!(
        (pos.get() - expected).abs() < 1.0,
        "expected ~{expected} file samples, got {} \
         (a double-applied src_ratio gives ~{doubled})",
        pos.get()
    );
}

/// A window set on a free-running source places it, and placing it
/// afterwards at that window leaves it as it was.
///
/// Mutation (run): `set_window` storing the window only when already placed
/// → `window()` stays `None` after the set → fails.
#[test]
fn setting_a_window_places_the_source() {
    let wave = ramp_wave(100, 44_100.0);
    let mut sampler = MemorySource::new(wave);
    assert_eq!(sampler.window(), None);
    assert!(!sampler.is_placed());

    let window = VoiceWindow::span(Beat::new(8.0), BeatDuration::new(4.0));
    sampler.set_window(window);
    assert_eq!(sampler.window(), Some(window));

    let sampler = sampler.placed_at(window);
    assert_eq!(sampler.window(), Some(window));

    // The gate honours it: beat 0 is before the beat-8 start, beat 9 inside.
    let at = |beat: f64| MockTransport::rolling(Beat::new(beat), Bpm::new(120.0)).transport();
    assert!(sampler.window_position(&at(0.0)).is_none());
    assert!(sampler.window_position(&at(9.0)).is_some());
}

/// The block-stepping rate must be the SAME rate the gate used, or a block
/// starts at the right sample and walks away from it — a slow detune rather
/// than an obvious break.
#[test]
fn window_rate_matches_the_gate_and_excludes_src_ratio() {
    let wave = ramp_wave(200_000, 48_000.0);
    let mut sampler = MemorySource::new(wave);
    sampler.set_speed(PlaybackRate::new(2.0));
    sampler.set_render_rate(SampleRate::new(44_100.0));

    // Varispeed alone: the gate already resolved the file rate.
    assert!((sampler.window_rate().get() - 2.0).abs() < 1e-6);

    // The free-running rate DOES carry the conversion — different question,
    // different quantity, and the two must not be conflated.
    let free = 2.0 * (48_000.0 / 44_100.0);
    assert!(
        (sampler.read_rate().get() - free).abs() < 1e-4,
        "read_rate should still be varispeed x conversion, got {}",
        sampler.read_rate().get()
    );
}

/// The read position advances by `wave_rate / session_rate` per tick, so a
/// file recorded at a different rate than the session plays at the right
/// pitch instead of the right speed.
#[test]
fn src_ratio_tracks_the_rate_mismatch() {
    // (wave rate, session rate, frames advanced per tick)
    for (wave_hz, session_hz, want) in [
        (44100.0f64, 44100.0f64, 1.0f64), // matched: unity
        (48000.0, 24000.0, 2.0),          // file faster: read two frames a tick
        (24000.0, 48000.0, 0.5),          // file slower: read half a frame
    ] {
        let wave = ramp_wave(100, wave_hz);
        let mut sampler = MemorySource::new(wave);
        sampler.set_session_sample_rate(session_hz);

        let mut out = [0.0f32; 2];
        frame_into(&mut sampler, None, &mut out);

        let pos = sampler.position().get();
        assert!(
            (pos - want).abs() < 1e-6,
            "a {wave_hz} Hz wave in a {session_hz} Hz session should advance \
             {want} frames per tick, got {pos}"
        );
    }
}

#[test]
fn stops_at_end_when_not_looping() {
    let wave = ramp_wave(10, 44100.0);
    let mut sampler = MemorySource::new(wave);

    let mut out = [0.0f32; 2];
    for _ in 0..20 {
        frame_into(&mut sampler, None, &mut out);
    }

    assert!(!sampler.is_playing());
}

#[test]
fn loops_back_when_looping() {
    let wave = ramp_wave(10, 44100.0);
    let mut sampler = MemorySource::new(wave);
    sampler.set_looping(true);

    let mut out = [0.0f32; 2];
    // Tick exactly 10 times → position reaches 10.0, wraps to 0.0
    for _ in 0..10 {
        frame_into(&mut sampler, None, &mut out);
    }
    assert!(sampler.is_playing());
    let pos = sampler.position().get();
    assert!(
        (pos - 0.0).abs() < 1e-6,
        "10 ticks at speed=1 on len=10 should wrap to 0.0, got {pos}"
    );

    // One more tick reads sample[0] (pos=0.0 after wrap) = 1.0,
    // then advances position to 1.0.
    frame_into(&mut sampler, None, &mut out);
    assert!(
        (out[0] - 1.0).abs() < 1e-6,
        "after wrap to 0.0, should read sample[0] = 1.0, got {}",
        out[0]
    );
    let pos_after = sampler.position().get();
    assert!(
        (pos_after - 1.0).abs() < 1e-6,
        "position should advance to 1.0, got {pos_after}"
    );
}

#[test]
fn looping_overshoot_at_double_speed() {
    let wave = ramp_wave(10, 44100.0);
    let mut sampler = MemorySource::new(wave);
    sampler.set_looping(true);
    sampler.set_speed(PlaybackRate::new(2.0));

    let mut out = [0.0f32; 2];
    // 5 ticks at speed=2 → position advances 0,2,4,6,8 → after tick 5
    // position = 10.0, wraps to 0.0
    for _ in 0..5 {
        frame_into(&mut sampler, None, &mut out);
    }
    assert!(sampler.is_playing());
    let pos = sampler.position().get();
    assert!(
        (pos - 0.0).abs() < 1e-6,
        "5 ticks at speed=2 on len=10 should wrap to 0.0, got {pos}"
    );

    // 6th tick reads sample[0] = 1.0, advances to 2.0
    frame_into(&mut sampler, None, &mut out);
    assert!(
        (out[0] - 1.0).abs() < 1e-6,
        "after wrap, sample[0] should be 1.0, got {}",
        out[0]
    );
}

#[test]
fn process_block_produces_correct_samples() {
    let wave = ramp_wave(100, 44100.0);
    let mut sampler = MemorySource::new(wave);

    let output = planes(&mut sampler, None, 4);

    assert!((output[0][0] - 1.0).abs() < 1e-6);
    assert!((output[0][1] - 2.0).abs() < 1e-6);
    assert!((output[0][2] - 3.0).abs() < 1e-6);
    assert!((output[0][3] - 4.0).abs() < 1e-6);
}

#[test]
fn process_block_silence_when_stopped() {
    let wave = ramp_wave(100, 44100.0);
    let mut sampler = MemorySource::new(wave);
    sampler.stop();

    let output = planes(&mut sampler, None, 4);

    for ch in &output[..2] {
        assert_eq!(ch.len(), 4);
        assert!(ch.iter().all(|&s| s == 0.0));
    }
}

#[test]
fn process_stops_mid_block_when_sample_ends() {
    let wave = ramp_wave(3, 44100.0);
    let mut sampler = MemorySource::new(wave);

    let output = planes(&mut sampler, None, 8);

    assert!(!sampler.is_playing());
    assert!((output[0][0] - 1.0).abs() < 1e-6);
    assert!((output[0][1] - 2.0).abs() < 1e-6);
    assert!((output[0][2] - 3.0).abs() < 1e-6);
    assert_eq!(output[0][4], 0.0);
}

// --- Transport-driven playback ---

#[test]
fn transport_driven_produces_samples_at_beat_position() {
    // At 120 BPM, beat 1.0 = 0.5 seconds = 22050 samples at 44100 Hz.
    // ramp_wave has sample[i] = i+1, so sample[22050] = 22051.0.
    let wave = ramp_wave(44100, 44100.0);
    let transport = MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
    let mut sampler = MemorySource::placed(wave, Beat::new(0.0), None);

    let mut output = [0.0f32; 2];
    frame_into(&mut sampler, Some(&transport), &mut output);

    let expected_sample_idx = 22050.0; // 1 beat * 60/120 * 44100
    let expected_value = expected_sample_idx + 1.0; // ramp offset
    assert!(
        (output[0] - expected_value).abs() < 1.0,
        "beat 1.0 @ 120BPM/44.1k should read near sample 22050, got {}",
        output[0]
    );
}

#[test]
fn transport_stopped_outputs_silence() {
    let wave = ramp_wave(44100, 44100.0);
    let transport = MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0));
    let mut sampler = MemorySource::placed(wave, Beat::new(0.0), None);

    let mut output = [0.0f32; 2];
    frame_into(&mut sampler, Some(&transport), &mut output);

    assert_eq!(output[0], 0.0);
    assert_eq!(output[1], 0.0);
}

#[test]
fn transport_before_start_beat_outputs_silence() {
    let wave = ramp_wave(44100, 44100.0);
    let transport = MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
    let mut sampler = MemorySource::placed(wave, Beat::new(4.0), None);

    let mut output = [0.0f32; 2];
    frame_into(&mut sampler, Some(&transport), &mut output);

    assert_eq!(output[0], 0.0, "beat 1.0 < start_beat 4.0 → silence");
}

#[test]
fn transport_past_duration_beats_outputs_silence() {
    let wave = ramp_wave(44100, 44100.0);
    let transport = MockTransport::rolling(Beat::new(10.0), Bpm::new(120.0));
    let mut sampler = MemorySource::placed(wave, Beat::new(0.0), Some(BeatDuration::new(4.0)));

    let mut output = [0.0f32; 2];
    frame_into(&mut sampler, Some(&transport), &mut output);

    assert_eq!(output[0], 0.0, "beat 10.0 past duration 4.0 → silence");
}

#[test]
fn transport_process_block() {
    let wave = ramp_wave(44100, 44100.0);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
    let mut sampler = MemorySource::placed(wave, Beat::new(0.0), None);

    let output = planes(&mut sampler, Some(&transport), 4);

    assert!((output[0][0] - 1.0).abs() < 1e-6, "beat 0 → sample 0");
}

#[test]
fn transport_process_block_silence_when_stopped() {
    let wave = ramp_wave(44100, 44100.0);
    let transport = MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0));
    let mut sampler = MemorySource::placed(wave, Beat::new(0.0), None);

    let output = planes(&mut sampler, Some(&transport), 4);

    assert_eq!(output[0].len(), 4);
    assert!(output[0].iter().all(|&s| s == 0.0));
}

// --- Clone ---

#[test]
fn clone_preserves_state() {
    let wave = ramp_wave(100, 44100.0);
    let sampler = MemorySource::with_config(
        wave,
        MemorySourceConfig {
            gain: Amplitude::new(0.75),
            speed: PlaybackRate::new(1.5),
            loop_setting: LoopSetting::On {
                start: SamplePosition::new(0.0),
                end: SamplePosition::new(100.0),
                crossfade_frames: 0,
            },
            ..Default::default()
        },
    );
    sampler.trigger_at(SamplePosition::new(42.0));

    let cloned = sampler.clone();
    assert_eq!(cloned.gain(), Amplitude::new(0.75));
    assert_eq!(cloned.speed(), PlaybackRate::new(1.5));
    assert!(cloned.is_looping());
    assert!(cloned.is_playing());
    assert_eq!(cloned.position(), SamplePosition::new(42.0));
}

// --- set_wave ---

#[test]
fn set_wave_resets_position() {
    let wave1 = ramp_wave(100, 44100.0);
    let wave2 = ramp_wave(50, 48000.0);
    let mut sampler = MemorySource::new(wave1);

    sampler.trigger_at(SamplePosition::new(42.0));
    sampler.set_wave(wave2);

    assert_eq!(sampler.position(), SamplePosition::new(0.0));
    assert_eq!(sampler.duration_samples(), 50);
}

// --- prepare (the node's rate) ---

#[test]
fn prepare_updates_src_ratio() {
    let wave = ramp_wave(100, 48000.0);
    let mut sampler = MemorySource::new(wave);

    sampler.prepare(&Prepare::new(SampleRate(24000.0), Samples(64)));

    let mut out = [0.0f32; 2];
    frame_into(&mut sampler, None, &mut out);
    let pos = sampler.position().get();
    assert!((pos - 2.0).abs() < 1e-6, "48k/24k = 2x advance");
}

// --- Interpolation at end of sample ---

#[test]
fn interpolation_at_last_sample_clamps() {
    let wave = ramp_wave(3, 44100.0);
    let mut sampler = MemorySource::new(wave);

    let mut out = [0.0f32; 2];
    frame_into(&mut sampler, None, &mut out);
    assert!((out[0] - 1.0).abs() < 1e-6);

    frame_into(&mut sampler, None, &mut out);
    assert!((out[0] - 2.0).abs() < 1e-6);

    frame_into(&mut sampler, None, &mut out);
    assert!((out[0] - 3.0).abs() < 1e-6);
}

#[test]
fn looping_wraps_and_continues_producing_audio() {
    // Verify that looping a 2-sample wave keeps producing the same
    // values cyclically (not silence, not garbage).
    let wave = ramp_wave(2, 44100.0);
    let mut sampler = MemorySource::new(wave);
    sampler.set_looping(true);

    let mut outputs = Vec::new();
    let mut out = [0.0f32; 2];
    for _ in 0..6 {
        frame_into(&mut sampler, None, &mut out);
        outputs.push(out[0]);
    }

    assert!(sampler.is_playing());
    // ramp_wave(2) = [1.0, 2.0]. Looping: 1, 2, 1, 2, 1, 2
    assert!((outputs[0] - 1.0).abs() < 1e-6, "cycle 0 sample 0");
    assert!((outputs[2] - 1.0).abs() < 1e-6, "cycle 1 sample 0");
    assert!((outputs[4] - 1.0).abs() < 1e-6, "cycle 2 sample 0");
}

/// Channel `c` carries the constant `c + 1`, so a wrong-channel read is a
/// wrong value rather than a plausible one.
fn indexed_wave(channels: usize, len: usize) -> Arc<Wave> {
    let mut w = Wave::zero(channels, 44_100.0, len as f64 / 44_100.0);
    for i in 0..w.len() {
        for c in 0..channels {
            w.set(c, i, (c + 1) as f32);
        }
    }
    Arc::new(w)
}

/// Declared width, not inferred: a 6-channel wave through `new` still yields
/// a stereo node. A node that re-arity'd itself from its content would break
/// edges already wired against its shape.
#[test]
fn new_stays_stereo_even_for_a_wide_wave() {
    let u = MemorySource::new(indexed_wave(6, 32));
    assert_eq!(u.channels(), ChannelLayout::STEREO);
    assert_eq!(u.shape().audio_out, ChannelLayout::STEREO);
}

#[test]
fn with_channels_declares_the_width() {
    let u = MemorySource::with_channels(indexed_wave(6, 32), 6usize);
    assert_eq!(u.channels(), ChannelLayout::from(6u16));
    assert_eq!(u.shape().audio_out.count(), 6);
    assert_eq!(
        MemorySource::with_channels(indexed_wave(2, 32), 0usize).channels(),
        ChannelLayout::MONO
    );
}

/// The node's shape is its declared width, a generator's: no inputs, no
/// latency, never skipped (its tail is unbounded).
///
/// Mutation (run): `shape` declaring stereo whatever the node was built
/// at → fails at width 1.
#[test]
fn the_shape_is_the_declared_width() {
    for w in [1usize, 2, 6, 8] {
        let u = MemorySource::with_channels(indexed_wave(2, 32), w);
        let shape = u.shape();
        assert_eq!(shape.audio_out.count() as usize, w, "width {w}");
        assert_eq!(shape.audio_in.count(), 0);
        assert_eq!(shape.latency.samples(), Samples(0));
        assert_eq!(shape.tail, Tail::Unbounded);
    }
}

/// All six channels must reach all six outputs, free-running and placed
/// (two reads, each scattering into the node's planar outputs).
#[test]
fn six_channel_wave_reaches_all_six_outputs() {
    let mut u = MemorySource::with_channels(indexed_wave(6, 64), 6usize);
    let out = planes(&mut u, None, 8);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
    let mut placed =
        MemorySource::with_channels(indexed_wave(6, 64), 6usize).placed_at(VoiceWindow::default());
    let placed = planes(&mut placed, Some(&transport), 8);
    for (what, out) in [("free-running", out), ("placed", placed)] {
        for (c, ch) in out.iter().enumerate() {
            assert!(
                (ch[0] - (c + 1) as f32).abs() < 1e-4,
                "{what}: channel {c} should carry {}, got {}",
                c + 1,
                ch[0]
            );
        }
    }
}

/// Gain is one scalar across every channel — per-channel level is the mixer
/// strip's job, not the reader's.
#[test]
fn gain_applies_uniformly_across_all_channels() {
    let mut u = MemorySource::with_channels(indexed_wave(6, 64), 6usize);
    u.set_gain(Amplitude::new(0.5));
    let mut out = [0.0f32; 6];
    frame_into(&mut u, None, &mut out);
    for (c, &got) in out.iter().enumerate() {
        let want = (c + 1) as f32 * 0.5;
        assert!(
            (got - want).abs() < 1e-4,
            "channel {c}: expected {want}, got {got}"
        );
    }
}

/// A looping 6-channel voice must keep every channel through the crossfade.
/// The crossfade blends in place across the whole frame, so a stereo-shaped
/// blend would leave channels 2..6 un-faded (or worse, untouched).
#[test]
fn six_channel_loop_crossfade_covers_every_channel() {
    let mut u = MemorySource::with_channels(indexed_wave(6, 64), 6usize);
    u.set_loop_range(SamplePosition::new(0.0), SamplePosition::new(16.0), 4);
    let mut out = [0.0f32; 6];
    // Drive past the loop point so the crossfade engages at least once.
    for _ in 0..40 {
        frame_into(&mut u, None, &mut out);
        for (c, &s) in out.iter().enumerate() {
            assert!(
                s.is_finite(),
                "channel {c} produced a non-finite sample during loop crossfade"
            );
        }
    }
}

/// A gain change reaches the node the graph renders.
///
/// The memory tier's half of the live-value rule: a control a host writes
/// through the node's controls (its [`ParamSet`], or a clone of the
/// source sharing its cell) must be seen by the node that renders, which
/// the executor owns and nothing else can reach. Asserted through the
/// graph: the node inserted with `param_parts`, its gain set through the
/// controls `IntoNode` handed back.
///
/// Mutation (run): `param_set` building its `ParamSet` over a detached
/// copy of the gain (`Param::new(self.gain.load())`) → the render stays
/// at unity → fails.
#[test]
fn a_gain_change_reaches_the_rendering_node() {
    use tutti_graph::Solo;
    let wave = ramp_wave(64, 44_100.0);
    let mut solo = Solo::new(
        MemorySource::with_channels(wave, 1usize),
        Prepare::new(SampleRate(SR), Samples(64)),
    );
    assert!(solo.controls().set(UnitParam::Volume, 0.25));
    let out = solo.render(1);
    // The ramp's first frame is 1.0, so the rendered value *is* the gain.
    assert!(
        (out[0][0] - 0.25).abs() < 1e-4,
        "a gain written through the controls must be seen by the node that \
         renders; expected ~0.25, got {}",
        out[0][0]
    );
}

/// A fork of the node shares nothing with it, and starts from the gain
/// last set — the graph's fork check (`assert_param_fork`): gain is
/// this node's one cell.
///
/// Mutation (run): `fork_fresh` without `detach_gain` → "a live write
/// reached the fork" → fails.
#[test]
fn the_fork_shares_no_gain() {
    tutti_graph::contract::assert_param_fork(MemorySource::new(ramp_wave(48_000, 48_000.0)));
}

/// Its gain is a port, its base the cell
/// ([`VOICE_PARAMS`](super::super::VOICE_PARAMS)).
///
/// Mutation (run): `param_base` answering unity → fails.
#[test]
fn the_shape_declares_its_gain() {
    let s = MemorySource::new(ramp_wave(64, 48_000.0));
    s.set_gain(Amplitude::new(0.25));
    assert_eq!(
        Node::shape(&s).params.as_slice(),
        &[tutti_types::UnitParam::Volume][..]
    );
    assert_eq!(Node::param_base(&s, 0), Some(0.25));
    assert_eq!(Node::param_base(&s, 1), None);
}

/// A fork plays from the render's transport:
/// a placed source forked out of a live graph reads its window of the
/// fork's own blocks' `Env`.
///
/// Mutation (run): `fork_fresh` leaving `placed` false → the fork plays
/// its free-running cursor (stopped by the reset: silence) → fails.
#[test]
fn a_placed_fork_reads_the_renders_transport() {
    let wave = ramp_wave(44_100, 44_100.0);
    let live = MemorySource::placed(wave, Beat::new(1.0), None);
    let mut fork = tutti_graph::ParamFork::new(&live).fork_node();
    // Beat 1 is where the window opens; half a beat later (11 025
    // frames at 120 BPM) the ramp reads 11 026.
    let transport = MockTransport::rolling(Beat::new(1.5), Bpm::new(120.0));
    let out = block(&mut fork, Some(&transport), 2);
    assert!((out[0].0 - 11_026.0).abs() < 1e-3, "{:?}", out[0]);
}
