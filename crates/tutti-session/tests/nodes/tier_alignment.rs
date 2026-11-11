//! A voice of either tier arrives where the compiler puts it, and nowhere
//! else: doc 025 stage 5 cut 2 (the preroll left the streamer, §2.9).
//!
//! The graph is one `QUAD` root. Outputs 0 and 1 are a stereo voice (the same
//! file, as a disk stream or in memory) placed at beat 1; output 2 is a
//! latent path (a limiter's lookahead of `L` frames, whatever it reports), so
//! the compiler delays outputs 0 and 1 by `L` to line them up with it
//! (`Session::compensation()` is `[L, L, 0, 0]`). A voice reports no latency
//! of its own, so its audio, started at beat 1 (frame 24 000 at 120 BPM and
//! 48 kHz), must be heard at frame `24 000 + L` and not a frame sooner or
//! later: file frame `k` on output frame `24 000 + L + k`.
//!
//! The oracle is arithmetic on the declared figures: the start frame, the
//! compiler's declared compensation and the file's own frame numbers (a ramp
//! whose every frame is distinct). No render is compared with another.
//!
//! Before cut 2 the session handed the streamer that same table, so the disk
//! tier read `L` frames earlier on top of the compiler's own ring: it lagged
//! the memory tier by `L` (the memory tier, which never pre-rolled, was right).

use std::path::Path;
use std::time::{Duration, Instant};
use tutti_types::MaxBlock;

use tutti_nodes::LimiterNode;
use tutti_nodes::testing::Const;
use tutti_sampler::testing::settle_threaded;
use tutti_sampler::{DiskVoice, MemorySource, Placement, StreamPhase};
use tutti_session::{GraphSource, Session};
use tutti_transport::MotionEvent;
use tutti_types::{Beat, ChannelLayout, Db, InterleavedMut, SampleRate, Samples};

const RATE: f64 = 48_000.0;
/// Beat 1 at 120 BPM and `RATE`.
const AT: usize = 24_000;
/// File frames, and how many of them the test reads back.
const LEN: usize = 4_800;
const BLOCK: usize = 512;

/// Frame `i` of the file: distinct per frame (a ramp), exactly what an f32
/// WAV hands back; the right channel is its negative.
fn value(i: usize) -> f32 {
    (i as f32 + 1.0) * 1e-5
}

fn write_ramp(path: &Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: RATE as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("writes");
    for i in 0..LEN {
        w.write_sample(value(i)).expect("writes");
        w.write_sample(-value(i)).expect("writes");
    }
    w.finalize().expect("writes");
}

/// How the voice under test is made.
#[derive(Clone, Copy, Debug)]
enum Tier {
    Disk,
    Memory,
}

/// Outputs 0 and 1 of the session rendering `tier`'s voice beside a latent
/// path, rolling from beat 0: `AT + 2 000` frames each, and the compensation
/// the compiler declared for output 0.
fn render(tier: Tier) -> (Vec<f32>, Vec<f32>, Samples) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);

    let mut s = Session::builder()
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::QUAD,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");

    // The latent path, on output 2.
    let (src, _) = s.graph().insert(Const::mono(1.0));
    let (eff, _) = s.graph().insert(LimiterNode::with_channels(
        ChannelLayout::MONO,
        Db(-1.0),
        Db(-0.3),
    ));
    s.graph().set_source(eff, 0, GraphSource::Node(src, 0));
    s.graph().set_output_source(2, GraphSource::Node(eff, 0));

    // The voice, on outputs 0 and 1.
    // The stream's handle is held for the whole render: dropping it would end
    // the stream.
    let (voice, stream) = match tier {
        Tier::Disk => {
            let streamer = s.disk_streamer().expect("an offline session streams");
            // The streamer opens the stream on its own thread.
            let (stream, reader) =
                settle_threaded(streamer.open(&path, Placement::HEAD)).expect("the stream opens");
            let voice = DiskVoice::from_reader(reader, Beat(1.0), None);
            (s.graph().insert(voice).0, Some(stream))
        }
        Tier::Memory => {
            let mut wave = tutti_codec::Wave::new(2, tutti_nodes::SampleRate::new(RATE));
            for i in 0..LEN {
                wave.push_frame(&[value(i), -value(i)]);
            }
            let source = MemorySource::placed(std::sync::Arc::new(wave), Beat(1.0), None)
                .expect("a placeable wave");
            (s.graph().insert(source).0, None)
        }
    };
    s.graph().set_output_source(0, GraphSource::Node(voice, 0));
    s.graph().set_output_source(1, GraphSource::Node(voice, 1));
    s.update();

    let declared = s.compensation().preroll(0).expect("a plan was sent");
    assert_eq!(
        s.compensation().preroll(1),
        Some(declared),
        "both voice channels carry the same compensation"
    );
    assert_eq!(
        s.compensation().preroll(2),
        Some(Samples(0)),
        "the latent path is the one the rest wait for"
    );
    assert!(
        declared.get() > 0,
        "the limiter's lookahead is the compensation, or this tests nothing"
    );

    // The streamer fills the ring on its own thread, and an offline render
    // does not wait for it. A ring that is not yet primed reads silence, which
    // fails the assertions below, so wait for the stream to say it is: its
    // status leaves `Opening` once the ring holds what the streamer reads
    // ahead (the first 4 200 of the file's 4 800 frames, which covers the
    // frames read back).
    if let Some(stream) = &stream {
        let deadline = Instant::now() + Duration::from_secs(10);
        let phase = loop {
            let phase = stream.status().phase;
            if phase != StreamPhase::Opening || Instant::now() > deadline {
                break phase;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(
            matches!(phase, StreamPhase::Live | StreamPhase::Ended),
            "the stream is not primed: {phase:?}"
        );
    }

    s.transport()
        .motion()
        .try_send(MotionEvent::Play)
        .expect("room");
    let frames = AT + 2_000;
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let mut block = vec![0.0f32; BLOCK * 4];
    while left.len() < frames {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::QUAD));
        left.extend(block.as_chunks::<4>().0.iter().map(|f| f[0]));
        right.extend(block.as_chunks::<4>().0.iter().map(|f| f[1]));
    }
    (left, right, declared)
}

/// **A voice of either tier is heard at its start frame plus the compiler's
/// compensation, once.** Silent before `AT + L`, then file frame `k` on
/// output frame `AT + L + k`, on both channels.
#[test]
fn a_voice_of_either_tier_arrives_at_its_start_plus_the_declared_compensation() {
    for tier in [Tier::Disk, Tier::Memory] {
        let (left, right, declared) = render(tier);
        let first = AT + declared.get();
        assert!(
            left[..first].iter().all(|&x| x == 0.0) && right[..first].iter().all(|&x| x == 0.0),
            "the {tier:?} tier sounded before frame {first} ({AT} + {declared:?})"
        );
        for k in 0..1_000 {
            assert_eq!(
                left[first + k],
                value(k),
                "the {tier:?} tier: left, file frame {k} at output frame {}",
                first + k
            );
            assert_eq!(
                right[first + k],
                -value(k),
                "the {tier:?} tier: right, file frame {k}"
            );
        }
    }
}
