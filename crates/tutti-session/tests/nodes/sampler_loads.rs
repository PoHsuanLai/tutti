//! A stream or a clip opened on the session's spawner lands in `update()`
//! (design 030, A5 and A6): the open's event carries the call's id, a missing
//! file lands its typed error, and a clip plays the file's own samples from
//! either tier.

use std::path::Path;
use std::time::{Duration, Instant};

use tutti_sampler::{OpenError, Placement, StreamPhase};
use tutti_session::{
    OutPort, Residency, Root, STREAM_ABOVE_BYTES, SamplerError, SamplerEvent, SamplerOpen, Session,
    SessionEvent,
};
use tutti_transport::MotionEvent;
use tutti_types::{Beat, ChannelLayout, InterleavedMut, MaxBlock, SampleRate, Samples};

const RATE: f64 = 48_000.0;
/// File frames.
const LEN: usize = 4_800;
const BLOCK: usize = 512;

/// Frame `i` of the ramp file: distinct per frame, exact in `f32`.
fn value(i: usize) -> f32 {
    (i as f32 + 1.0) * 1e-5
}

fn write_ramp(path: &Path) {
    let samples: Vec<f32> = (0..LEN).map(value).collect();
    tutti_fixtures::signal::write_wav_f32(path, SampleRate(RATE), 1, &samples);
}

fn session() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds")
}

/// The sampler events `update()` lands until `want` of them have, in order.
fn landed(s: &mut Session, want: usize) -> Vec<SamplerEvent> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut events = Vec::new();
    while events.len() < want {
        assert!(Instant::now() < deadline, "the open never landed");
        for event in s.update() {
            if let SessionEvent::Sampler(event) = event {
                events.push(event);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    events
}

/// **An open lands as `Opened` in `update()`, with the id the call returned.**
/// Two opens in flight are told apart by their ids.
#[test]
fn an_open_lands_opened_with_the_calls_id() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    let first = s.open_stream(&path, Placement::HEAD).expect("a streamer");
    let second = s.open_stream(&path, Placement::HEAD).expect("a streamer");
    assert_ne!(first, second);

    let mut opened: Vec<SamplerOpen> = landed(&mut s, 2)
        .into_iter()
        .map(|event| match event {
            SamplerEvent::Opened { id, stream, reader } => {
                assert_eq!(stream.id(), reader.id(), "the two halves of one stream");
                id
            }
            other => panic!("expected an open, got {other:?}"),
        })
        .collect();
    opened.sort_by_key(|id| format!("{id:?}"));
    let mut asked = vec![first, second];
    asked.sort_by_key(|id| format!("{id:?}"));
    assert_eq!(opened, asked, "each call's id came back once");
}

/// **A missing file lands `OpenFailed`**, with the call's id and the typed
/// error (`OpenError::Io(NotFound)`), not a stream that never starts.
#[test]
fn a_missing_file_lands_open_failed_with_the_typed_error() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut s = session();
    let id = s
        .open_stream(dir.path().join("absent.wav"), Placement::HEAD)
        .expect("a streamer");
    let events = landed(&mut s, 1);
    let [SamplerEvent::OpenFailed { id: failed, error }] = &events[..] else {
        panic!("expected one failed open, got {events:?}");
    };
    assert_eq!(*failed, id);
    assert!(
        matches!(
            error,
            SamplerError::Open(OpenError::Io(std::io::ErrorKind::NotFound))
        ),
        "{error:?}"
    );
}

/// A session with no disk streamer refuses an open at the call.
#[test]
fn a_session_without_a_streamer_refuses_an_open_at_the_call() {
    let mut s = session();
    drop(s.take_disk_streamer());
    let dir = tempfile::tempdir().expect("a temp dir");
    assert!(matches!(
        s.open_stream(dir.path().join("a.wav"), Placement::HEAD),
        Err(SamplerError::NoStreamer)
    ));
}

/// The clip `load_clip` landed for `id`, from the events of one `update()` run.
fn clip_of(events: Vec<SamplerEvent>, id: SamplerOpen) -> tutti_sampler::Clip {
    let mut events = events.into_iter();
    match events.next() {
        Some(SamplerEvent::ClipLoaded { id: got, clip }) => {
            assert_eq!(got, id);
            clip
        }
        other => panic!("expected the clip, got {other:?}"),
    }
}

/// Renders `clip` at beat 0 on a rolling session for `frames` frames,
/// waiting first for a streamed clip's ring to prime.
fn play(clip: tutti_sampler::Clip, frames: usize) -> Vec<f32> {
    let mut s = session();
    let (key, handle) = s.graph().add(clip);
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(key, 0)])
        .expect("the clip's channel into the root's");
    let _ = s.update();
    // A streamed clip's ring is filled on the streamer's thread, and an
    // offline render does not wait for it: wait for the stream to say it is
    // primed.
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(status) = handle.status() {
        if status.phase != StreamPhase::Opening {
            break;
        }
        assert!(Instant::now() < deadline, "the stream was not primed");
        std::thread::sleep(Duration::from_millis(1));
    }
    s.transport()
        .motion()
        .try_send(MotionEvent::Play)
        .expect("room");
    let mut out = Vec::new();
    let mut block = vec![0.0f32; BLOCK * 2];
    while out.len() < frames {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
        out.extend(block.chunks(2).map(|frame| frame[0]));
    }
    out.truncate(frames);
    drop(handle);
    out
}

/// **A clip of either tier plays the file's own samples.** The same ramp file
/// loaded resident and streamed: each is checked against the ramp the test
/// wrote (frame `i` is `value(i)`), neither against the other.
#[test]
fn a_clip_of_either_tier_plays_the_files_samples() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    for residency in [Residency::Resident, Residency::Streamed] {
        let mut s = session();
        let id = s.load_clip(&path, residency).expect("a streamer");
        let clip = clip_of(landed(&mut s, 1), id);
        assert_eq!(clip.is_streamed(), residency == Residency::Streamed);
        let heard = play(clip.at(Beat(0.0)), 3_000);
        // The transport starts on the render's next block, so the file begins
        // a block or so in, under the engine's start fade (it begins at gain
        // 0, so the file's first frame is the silent one before the first
        // sound); past the fade the output is the file's frames, one per
        // frame.
        let first = heard
            .iter()
            .position(|&x| x != 0.0)
            .unwrap_or_else(|| panic!("{residency:?}: silent"))
            - 1;
        assert!(first <= 2 * BLOCK, "{residency:?}: began at frame {first}");
        for k in 1_100..2_000 {
            assert_eq!(
                heard[first + k],
                value(k),
                "{residency:?}: file frame {k} at output frame {}",
                first + k
            );
        }
    }
}

/// **`Auto` decides by the file's decoded size.** A short file is resident; one
/// past [`STREAM_ABOVE_BYTES`] of decoded audio streams. (The long file is
/// 16-bit mono, whose decoded `f32` form is twice its size on disk, so the
/// file is half the line.)
#[test]
fn auto_streams_a_file_past_the_threshold_and_holds_a_short_one() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let short = dir.path().join("short.wav");
    write_ramp(&short);
    let long = dir.path().join("long.wav");
    let frames = (STREAM_ABOVE_BYTES / 4) as usize + 1;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 8_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&long, spec).expect("creates");
    for _ in 0..frames {
        w.write_sample(0i16).expect("writes");
    }
    w.finalize().expect("finalizes");

    let mut s = session();
    let ids = [
        s.load_clip(&short, Residency::Auto).expect("loads"),
        s.load_clip(&long, Residency::Auto).expect("loads"),
    ];
    let events = landed(&mut s, 2);
    for (id, streamed) in ids.into_iter().zip([false, true]) {
        let clip = events
            .iter()
            .find_map(|event| match event {
                SamplerEvent::ClipLoaded { id: got, clip } if *got == id => Some(clip),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no clip landed for {id:?}: {events:?}"));
        assert_eq!(clip.is_streamed(), streamed, "{id:?}");
    }
}
