//! The engine an offline session builds, rendered from the builder's own
//! wiring (the click it inserts). Moved from `bevy-tutti`'s
//! `engine::build` tests, which rendered the same engine through the
//! adapter's `assemble`.
//!
//! Each test stands on analytic figures (onset frames, the tone a dry voice
//! reads), on what needs no oracle (a voice's render at two block sizes,
//! which a voice's frames do not depend on), and, for the samples
//! themselves, on a golden digest of a render checked against an independent
//! reference implementation when it was recorded. The click and the pitched
//! voice call `sin` (and the vocoder's FFT), libm quality-of-implementation
//! that differs in the last ulp between C runtimes, so the digests are
//! asserted on Linux/glibc only, where they were recorded ([`GOLDEN_HERE`]).

use std::sync::Arc;

use tutti_core::{ChannelLayout, InterleavedMut, MetronomeMode, MotionEvent, SampleRate, Samples};
use tutti_session::{GraphSource, Session};

const RATE: f64 = 48_000.0;

/// Whether this target is the one the golden digests were recorded on:
/// Linux with glibc's libm (see the module docs).
const GOLDEN_HERE: bool = cfg!(all(target_os = "linux", target_env = "gnu"));

/// FNV-1a over the samples' little-endian `f32` bits.
fn digest(samples: &[f32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in samples.iter().flat_map(|s| s.to_le_bytes()) {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Assert `got` is the digest recorded as `want`, where the goldens hold.
fn assert_golden(what: &str, got: u64, want: u64) {
    if GOLDEN_HERE {
        assert_eq!(
            got, want,
            "{what}: digest {got:#018x}, recorded {want:#018x}"
        );
    }
}

/// An offline session at `RATE`, stereo, graph blocks of up to 1 024 frames
/// (the live engine's `LIVE_MAX_BLOCK`, which the digests were recorded at).
fn offline() -> Session {
    Session::builder()
        .build_offline(SampleRate(RATE), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds")
}

/// `frames` stereo frames of `s` in `block`-frame device blocks, the
/// transport rolling from the first block, and (with `seek`) a locate to
/// beat 0.75 between two blocks past the first 60 000 frames.
fn run(s: &Session, frames: usize, block: usize, seek: bool) -> Vec<f32> {
    let transport = s.transport();
    transport.motion.try_send(MotionEvent::Play).expect("room");
    let mut out = Vec::with_capacity(frames * 2);
    let mut buf = vec![0.0f32; block * 2];
    let mut located = !seek;
    while out.len() < frames * 2 {
        // A seek between two blocks, past the first second: the beat clock
        // has to follow the transport, not just count frames.
        if !located && out.len() >= 2 * 60_000 {
            located = true;
            transport
                .motion
                .try_send(MotionEvent::Locate {
                    beat: tutti_core::Beat(0.75),
                    fade: tutti_core::FadeOut::Immediate,
                    then: tutti_core::Then::Keep,
                })
                .expect("room");
        }
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        out.extend_from_slice(&buf);
    }
    out.truncate(frames * 2);
    out
}

/// The session's click at full volume on every beat, on both global
/// outputs, rolling from the first block: `frames` stereo frames in
/// 512-frame device blocks, with a seek past the first second.
///
/// The click has no inputs: it reads its block's `Env`.
fn render_click(frames: usize) -> Vec<f32> {
    let mut s = offline();
    let settings = Arc::clone(s.click_settings());
    settings.set_mode(MetronomeMode::Always);
    settings.set_volume(1.0);
    let click = s.click_node().expect("the builder inserts the click");
    for port in 0..2 {
        s.graph()
            .set_output_source(port, GraphSource::Node(click, port));
    }
    s.update();
    run(&s, frames, 512, true)
}

/// Frames on which the left channel starts sounding.
fn onsets(stereo: &[f32]) -> Vec<usize> {
    let left: Vec<f32> = stereo.iter().step_by(2).copied().collect();
    (1..left.len())
        .filter(|&f| left[f] != 0.0 && left[f - 1] == 0.0)
        .chain((left.first().is_some_and(|&x| x != 0.0)).then_some(0))
        .collect()
}

/// **The builder's engine clicks on every beat, across a seek**, and
/// (Linux/glibc) clicks the golden digest's samples. The click reads the
/// beat of every frame from its block's `Env` (the transport the engine's
/// own `TransportClock` reports). The transport starts before the first
/// block, so the play gate opens on the first frame.
///
/// The onsets, analytically: at 120 BPM a beat is 24 000 frames, the seek
/// lands on the 118th 512-frame block (frame 60 416) at beat 0.75, and the
/// next beat is a quarter beat (6 000 frames) later; a click is heard from
/// the frame after its beat (its first sample is `sin(0)`). The locate
/// itself clicks too (60 417): it moves the whole beat from 2 to 0, which
/// the click takes for a new beat.
///
/// Mutations (run in `bevy-tutti`, where this test was recorded):
/// - the click reading the block's first beat for the whole block → the
///   onsets land on block starts and move;
/// - the click's volume at 0.99 → the digest moves.
///
/// Mutation (run here): `SessionBuilder::parts` building the click with
/// its own `ClickSettings` (not the session's) → the settings the test sets
/// reach no click, which stays silent → fails.
#[test]
fn the_click_sounds_on_every_beat_across_the_seek() {
    let frames = 3 * RATE as usize;
    let rendered = render_click(frames);
    assert_eq!(
        onsets(&rendered),
        vec![1, 24_001, 48_001, 60_417, 66_417, 90_417, 114_417, 138_417],
        "a click on every beat, across the seek"
    );
    assert_golden("the click", digest(&rendered), 0x9ac0_787e_520a_8085);
}

/// **The graph runs at the rate it was built at.** At 120 BPM and 48 kHz
/// the click lands every 24 000 frames. Every unit inserted is prepared at
/// the graph's rate, so a graph left at its 44.1 kHz default would click
/// every 22 050 frames.
///
/// Mutation (run): `build_offline` building the graph at
/// `SampleRate::DEFAULT` → clicks at 22 050 and fails.
#[test]
fn the_graph_runs_at_the_rate_it_was_built_at() {
    let on = onsets(&render_click(50_000));
    assert_eq!(on, vec![1, 24_001, 48_001]);
}

// ---- a clip reader through the builder's engine ---------------------------

/// The tone's frame `i`: 440 Hz at `RATE`.
fn tone_at(i: usize) -> f32 {
    (std::f32::consts::TAU * 440.0 * i as f32 / RATE as f32).sin()
}

/// One sampler voice on a second of the tone, placed at beat 0 and pitched
/// by `cents`. It reads the transport from its blocks' `Env`.
fn voice(cents: f32) -> tutti_sampler::VoicePool {
    use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};
    let mut wave = tutti_io::Wave::new(1, RATE);
    for i in 0..RATE as usize {
        wave.push_frame(&[tone_at(i)]);
    }
    let source = MemorySource::placed(Arc::new(wave), tutti_core::Beat(0.0), None);
    let mut pool = VoicePool::new();
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(source),
            play: Playback {
                pitch: tutti_core::Cents::new(cents),
                ..Default::default()
            },
            channel_index: None,
        },
    );
    pool
}

/// An offline session with one sampler voice ([`voice`]) on both global
/// outputs, the transport rolling from the first block: `frames` stereo
/// frames in `block`-frame device blocks.
fn render_voice(cents: f32, frames: usize, block: usize) -> Vec<f32> {
    let mut s = offline();
    let (voice, _handle) = s.graph().insert(voice(cents));
    for port in 0..2 {
        s.graph()
            .set_output_source(port, GraphSource::Node(voice, port));
    }
    s.update();
    run(&s, frames, block, false)
}

/// The dominant frequency of `x[from..]` between 500 and 900 Hz: the peak
/// of its Hann-windowed spectrum, scanned in quarter-hertz steps. A
/// tolerance check on it is portable (a last-ulp libm difference moves no
/// peak).
fn dominant_frequency(x: &[f32], from: usize, rate: f64) -> f64 {
    use std::f64::consts::TAU;
    let w = &x[from..];
    let n = w.len() as f64;
    let mut best = (0.0, 0.0);
    let mut f = 500.0;
    while f < 900.0 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in w.iter().enumerate() {
            let hann = 0.5 - 0.5 * (TAU * i as f64 / n).cos();
            let p = TAU * f * i as f64 / rate;
            re += f64::from(s) * hann * p.cos();
            im -= f64::from(s) * hann * p.sin();
        }
        let m = re * re + im * im;
        if m > best.1 {
            best = (f, m);
        }
        f += 0.25;
    }
    best.0
}

/// **A sampler voice plays in time through the builder's engine**, at
/// `block`-frame device blocks with a rolling transport: the dry voice is
/// the tone it plays, to the bit, on both channels; the voice a fifth up
/// renders the same bits as at 64-frame blocks, and (Linux/glibc) the
/// golden digest's.
///
/// Mutation (run in `bevy-tutti`, where this test was recorded):
/// `Cents::to_pitch_ratio` dividing by 1 100 cents to the octave → the
/// pitched voice is not a fifth up, on every target.
///
/// Mutation (run here): `Session::render` rendering a block of silence
/// (skipping `Engine::process`) → the dry voice is silent → fails.
fn a_voice_plays_in_time(block: usize) {
    let frames = 24_000;
    // The dry voice is pinned to the tone itself; only the pitched one,
    // which has no closed form, carries a digest.
    for (cents, want) in [(0.0f32, None), (700.0, Some(0x5681_b9dc_0fbe_0695u64))] {
        let rendered = render_voice(cents, frames, block);
        let peak = rendered.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.5, "{block}-frame blocks, {cents} cents: silent");
        if cents == 0.0 {
            for (i, s) in rendered.as_chunks::<2>().0.iter().enumerate() {
                let want = tone_at(i).to_bits();
                assert!(
                    s[0].to_bits() == want && s[1].to_bits() == want,
                    "{block}-frame blocks: frame {i} read {s:?}, the tone is {}",
                    tone_at(i)
                );
            }
        } else {
            // Portable, where the digest is not: a fifth up from 440 Hz is
            // 440 · 2^(7/12) ≈ 659.26 Hz. Past the vocoder's first few
            // thousand frames, within 1% (a semitone is 6%).
            let left: Vec<f32> = rendered.iter().step_by(2).copied().collect();
            let want = 440.0 * 2f64.powf(7.0 / 12.0);
            let got = dominant_frequency(&left, 4_096, RATE);
            assert!(
                (got - want).abs() < want * 1e-2,
                "{block}-frame blocks: {got} Hz, a fifth up is {want} Hz"
            );
            let by64 = render_voice(cents, frames, 64);
            if let Some(i) = by64
                .iter()
                .zip(&rendered)
                .position(|(a, b)| a.to_bits() != b.to_bits())
            {
                panic!(
                    "{block}-frame blocks, {cents} cents: parts from 64-frame blocks at \
                     frame {} (channel {}): {} vs {}",
                    i / 2,
                    i % 2,
                    rendered[i],
                    by64[i]
                );
            }
        }
        if let Some(want) = want {
            assert_golden(&format!("voice, {cents} cents"), digest(&rendered), want);
        }
    }
}

#[test]
fn a_voice_plays_in_time_at_256_frame_blocks() {
    a_voice_plays_in_time(256);
}

#[test]
fn a_voice_plays_in_time_at_512_frame_blocks() {
    a_voice_plays_in_time(512);
}
